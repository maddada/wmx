use super::{
    client::request,
    input::{Input, InputFilter},
    protocol::*,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    io::{BufReader, IsTerminal, Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub fn attach(name: &str, prompt_editor: Option<&str>) -> Result<()> {
    let _console = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Some(super::client::ConsoleMode::raw()?)
    } else {
        None
    };
    let endpoint = endpoint(name)?;
    let features = request(name, "ping", Value::Null)?;
    let supports_visibility = features["capabilities"]
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value == "client-visibility"));
    let client_id: u64 = rand::random::<u32>() as u64;
    let mut stream = TcpStream::connect(("127.0.0.1", endpoint.port))?;
    stream.set_nodelay(true)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((200, 50));
    write_frame(
        &mut stream,
        &Request {
            token: endpoint.token,
            operation: "attach".into(),
            data: json!({"clientId": client_id, "rows": rows, "cols": cols, "promptEditor": prompt_editor}),
        },
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let first: Value = read_frame(&mut reader)?;
    // The daemon's view of the application's kitty keyboard flags; daemons
    // without the field keep every CSI-u key translated.
    let keyboard = Arc::new(AtomicU8::new(0));
    emit(&first, &keyboard)?;
    let _keyboard = KeyboardMode::enable()?;
    let alive = Arc::new(AtomicBool::new(true));
    let input_alive = alive.clone();
    let input_name = name.to_string();
    let input_socket = stream.try_clone()?;
    let input_keyboard = keyboard.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    let (replies, pending_replies) = std::sync::mpsc::channel::<super::control::Reply>();
    if let Some(pipe) = std::env::var(super::control::ENVIRONMENT)
        .ok()
        .filter(|value| !value.is_empty())
    {
        let control = tx.clone();
        // Without the pipe, claims fall back to the console as before.
        let _ = super::control::serve(&pipe, pending_replies, move |bytes| {
            control.send(Incoming::Control(bytes)).is_ok()
        });
    }
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 8192];
        while let Ok(count) = stdin.read(&mut buffer) {
            if count == 0 || tx.send(Incoming::Stdin(buffer[..count].to_vec())).is_err() {
                break;
            }
        }
    });
    let input_thread = thread::spawn(move || {
        let mut filter = InputFilter::default();
        let mut control_filter = InputFilter::default();
        let mut encoder = super::console_input::ConsoleInput::default();
        let detach_key = std::env::var_os("ZMX_NO_DETACH_KEY").is_none()
            && std::env::var_os("WMX_NO_DETACH_KEY").is_none();
        let mut detach_nonce: Option<String> = None;
        let mut last_stdin = Instant::now();
        'input: while input_alive.load(Ordering::Acquire) {
            let mut idle = false;
            let events = match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Incoming::Stdin(bytes)) => {
                    last_stdin = Instant::now();
                    filter.feed(&bytes, detach_key)
                }
                // Only Ghostex controls are read from the pipe; nothing on it is typed.
                Ok(Incoming::Control(bytes)) => control_filter
                    .feed(&bytes, false)
                    .into_iter()
                    .filter(|event| !matches!(event, Input::Bytes(_) | Input::Detach))
                    .collect(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    idle = true;
                    let mut events = filter.flush();
                    let bytes = encoder.idle();
                    if !bytes.is_empty() {
                        events.push(Input::Bytes(bytes));
                    }
                    events
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            for event in events {
                let (operation, data) = match event {
                    Input::Detach => break 'input,
                    Input::DetachRequest(nonce) => {
                        detach_nonce = Some(nonce);
                        continue;
                    }
                    Input::Bytes(bytes) => {
                        encoder.set_kitty_keys(input_keyboard.load(Ordering::Acquire));
                        let mut bytes = encoder.feed(&bytes);
                        // InputFilter releases a standalone Escape after its own idle delay.
                        if bytes.is_empty() {
                            bytes = encoder.idle();
                        }
                        if bytes.is_empty() {
                            continue;
                        }
                        (
                            "input",
                            if supports_visibility {
                                json!({"clientId": client_id, "bytes": STANDARD.encode(bytes)})
                            } else {
                                json!(STANDARD.encode(bytes))
                            },
                        )
                    }
                    Input::Visibility { state, rows, cols } if supports_visibility => (
                        "visibility",
                        json!({"clientId": client_id, "state": state, "rows": rows, "cols": cols}),
                    ),
                    Input::Visibility {
                        state: "visible" | "chat",
                        rows,
                        cols,
                    } => ("resize", json!({"rows": rows, "cols": cols})),
                    Input::Refresh if supports_visibility => {
                        ("refresh", json!({"clientId": client_id}))
                    }
                    _ => continue,
                };
                if request(&input_name, operation, data).is_err() {
                    break 'input;
                }
            }
            // CDXC:Zmx 2026-10-08 WHY:
            // The detach request arrives on the pipe while the keys written before it travel through ConPTY, so the two can be read in either order. Detach only after the console has been quiet for `DETACH_QUIET` and everything read from it went to the daemon; the acknowledgement then proves earlier input was delivered, as zmx's ordered Detach does.
            if idle && last_stdin.elapsed() >= DETACH_QUIET {
                if detach_nonce.is_some() {
                    break 'input;
                }
            }
        }
        input_alive.store(false, Ordering::Release);
        let _ = input_socket.shutdown(Shutdown::Both);
        if let Some(nonce) = detach_nonce {
            // The process exits with the attachment, so wait until the acknowledgement is out.
            let (written, confirmed) = std::sync::mpsc::channel();
            let acknowledgement = format!("\x1b]1337;ZMX_DETACH_ACK={nonce}\x07").into_bytes();
            if replies.send((acknowledgement, Some(written))).is_ok() {
                let _ = confirmed.recv_timeout(Duration::from_secs(1));
            }
        }
    });
    let resize_alive = alive.clone();
    let resize_name = name.to_string();
    thread::spawn(move || {
        let mut previous = Some((cols, rows));
        while resize_alive.load(Ordering::Acquire) {
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                if previous != Some((cols, rows)) {
                    let data = if supports_visibility {
                        json!({"clientId": client_id, "rows": rows, "cols": cols})
                    } else {
                        json!({"rows": rows, "cols": cols})
                    };
                    if request(&resize_name, "resize", data).is_err() {
                        break;
                    }
                    previous = Some((cols, rows));
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let result = (|| {
        while let Ok(frame) = read_frame::<Value>(&mut reader) {
            emit(&frame, &keyboard)?;
        }
        Ok(())
    })();
    alive.store(false, Ordering::Release);
    // A detach requested on the control pipe answers it from that thread before this process ends.
    let _ = input_thread.join();
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn emit(frame: &Value, keyboard: &AtomicU8) -> Result<()> {
    if let Some(error) = frame.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    if let Some(flags) = frame.get("keyboard").and_then(Value::as_u64) {
        keyboard.store(flags as u8, Ordering::Release);
    }
    let bytes = STANDARD.decode(
        frame
            .get("output")
            .and_then(Value::as_str)
            .context("Invalid terminal output")?,
    )?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

// The client now consumes CSI-u. Ask capable terminals to preserve modifiers
// before ConPTY sees them; restore the caller's keyboard mode on detach/EOF.
struct KeyboardMode;

impl KeyboardMode {
    fn enable() -> Result<Self> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"\x1b[>1u")?;
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for KeyboardMode {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(b"\x1b[<u");
        let _ = stdout.flush();
    }
}

/// How long the console must stay quiet before a requested detach goes ahead.
const DETACH_QUIET: Duration = Duration::from_millis(100);

enum Incoming {
    /// Bytes the terminal typed into this attachment's console.
    Stdin(Vec<u8>),
    /// Bytes from the attach control pipe (`control.rs`).
    Control(Vec<u8>),
}
