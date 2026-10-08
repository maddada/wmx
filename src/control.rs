//! The attach control pipe: Ghostex's display claims for one attachment, around ConPTY.
//!
//! CDXC:Zmx 2026-10-08 WHY:
//! Windows' ConPTY parses what a terminal writes into the attachment's console and drops every OSC it does not know, so the `ZMX_VISIBLE` / `ZMX_CHAT` / `ZMX_HIDDEN` claims and the `ZMX_DETACH` handshake a Ghostex terminal wrote never reached `wmx attach` (`wmx grid` read every desktop client as `visible`). A caller that sets `WMX_ATTACH_CONTROL` to a pipe name gets a named pipe it connects to and writes the same OSC bytes into; replies (`ZMX_DETACH_CAP`, `ZMX_DETACH_ACK`) come back on it. Only Ghostex control sequences are read from the pipe; other bytes are dropped, never typed.
//! One thread owns the pipe. A blocking read on a synchronous pipe handle holds every other I/O on that handle, so a reply written while a read waited never left: after each request the thread polls with `PeekNamedPipe` for a while, which is when the replies come, and otherwise waits in a blocking read.
//! SEE-ALSO: src/attachment.rs, apps/desktop/src/terminal_model/zmx_control.rs in Ghostex.

use anyhow::{bail, Result};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    },
};

pub(crate) const ENVIRONMENT: &str = "WMX_ATTACH_CONTROL";
const DETACH_CAPABLE: &[u8] = b"\x1b]1337;ZMX_DETACH_CAP=1\x07";
/// How often the pipe is looked at while replies may be due, and for how long after traffic.
const POLL: Duration = Duration::from_millis(5);
const POLL_AFTER_TRAFFIC: Duration = Duration::from_secs(5);

/// Creates the pipe now (so a controller can connect as soon as this attachment runs) and serves
/// one controller on a thread of its own: every read goes to `on_bytes`, every `replies` message
/// is written back.
pub(crate) fn serve(
    name: &str,
    replies: Receiver<Reply>,
    mut on_bytes: impl FnMut(Vec<u8>) -> bool + Send + 'static,
) -> Result<()> {
    if !name.starts_with(r"\\.\pipe\") || name.len() > 256 {
        bail!("Invalid attach control pipe name");
    }
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut pipe = unsafe { File::from_raw_handle(handle as _) };
    std::thread::Builder::new()
        .name("wmx-attach-control".into())
        .spawn(move || {
            let handle = pipe.as_raw_handle() as _;
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected || pipe.write_all(DETACH_CAPABLE).is_err() {
                return;
            }
            let mut buffer = [0u8; 4096];
            // Replies only ever follow a request, so the pipe is polled for a while after each
            // read (and after connecting) and otherwise waits in a blocking read.
            let mut polling_until = Instant::now() + POLL_AFTER_TRAFFIC;
            loop {
                if Instant::now() >= polling_until {
                    match pipe.read(&mut buffer) {
                        Ok(count) if count > 0 => {
                            if !on_bytes(buffer[..count].to_vec()) {
                                return;
                            }
                            polling_until = Instant::now() + POLL_AFTER_TRAFFIC;
                            continue;
                        }
                        _ => return,
                    }
                }
                while let Ok((reply, written)) = replies.try_recv() {
                    let result = pipe.write_all(&reply).and_then(|()| pipe.flush());
                    if let Some(written) = written {
                        let _ = written.send(());
                    }
                    if result.is_err() {
                        return;
                    }
                }
                let mut available = 0u32;
                let peeked = unsafe {
                    PeekNamedPipe(
                        handle,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                } != 0;
                if !peeked {
                    return;
                }
                if available == 0 {
                    std::thread::sleep(POLL);
                    continue;
                }
                let wanted = (available as usize).min(buffer.len());
                match pipe.read(&mut buffer[..wanted]) {
                    Ok(count) if count > 0 => {
                        if !on_bytes(buffer[..count].to_vec()) {
                            return;
                        }
                        polling_until = Instant::now() + POLL_AFTER_TRAFFIC;
                    }
                    _ => return,
                }
            }
        })?;
    Ok(())
}

/// A reply, and who to tell once it was written (or could not be).
pub(crate) type Reply = (Vec<u8>, Option<Sender<()>>);
