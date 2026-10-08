use super::protocol::*;
use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::{
    fs,
    io::{BufReader, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::Duration,
};

pub(crate) fn request(name: &str, operation: &str, data: Value) -> Result<Value> {
    let endpoint = endpoint(name)?;
    let mut stream = connect(&endpoint)?;
    write_frame(
        &mut stream,
        &Request {
            token: endpoint.token,
            operation: operation.into(),
            data,
        },
    )?;
    let value: Value = read_frame(&mut BufReader::new(stream))?;
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        bail!("{message}");
    }
    Ok(value)
}

fn connect(endpoint: &Endpoint) -> Result<TcpStream> {
    let address = SocketAddr::from(([127, 0, 0, 1], endpoint.port));
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// CDXC:Zmx 2026-09-23 WHY:
/// Stale registry records cost two seconds each in TCP connection timeouts, preventing process snapshots from completing within gxserver's five-second deadline. Verify the recorded host image and creation time before contacting its port, so reused PIDs cannot make dead sessions appear live.
pub(crate) fn list() -> Result<Vec<Endpoint>> {
    let entries = match fs::read_dir(directory()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut endpoints = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        if let Ok(endpoint) = serde_json::from_slice::<Endpoint>(&fs::read(path)?) {
            let Ok(Some(endpoint)) = super::process_owner::inspect(&endpoint.name) else {
                continue;
            };
            if request(&endpoint.name, "ping", Value::Null).is_ok() {
                endpoints.push(endpoint);
            }
        }
    }
    Ok(endpoints)
}

/// CDXC:PlatformSupport 2026-09-28 WHY:
/// The first session start after installing or updating Ghostex waits on the antivirus scan of the new host binary, which took longer than the five seconds a new host used to get and failed the start of a session that then came up anyway. A host that answers returns at once, so the longer wait only covers that slow first start. A registry record whose host is gone is skipped rather than pinged, as `list` does: its port may belong to another program by now and hold each ping for the full read timeout.
/// SEE-ALSO: server/src/zmx/launch.rs `ZMX_START_COMMAND_TIMEOUT_MS` must stay longer than this wait.
const START_READY_TIMEOUT: Duration = Duration::from_secs(20);

fn host_answers(name: &str) -> bool {
    matches!(super::process_owner::inspect(name), Ok(Some(_)))
        && request(name, "ping", Value::Null).is_ok()
}

pub(crate) fn start(launch: Launch) -> Result<()> {
    if host_answers(&launch.name) {
        return Ok(());
    }
    let encoded = STANDARD.encode(serde_json::to_vec(&launch)?);
    let mut child = Some(super::launch::spawn(&encoded)?);
    let deadline = std::time::Instant::now() + START_READY_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if host_answers(&launch.name) {
            return Ok(());
        }
        if let Some(status) = child
            .as_ref()
            .map(|child| child.exited())
            .transpose()?
            .flatten()
        {
            if !host_holds_lock(&launch.name) {
                bail!("Native session host exited with status {status}");
            }
            child = None;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(child) = child {
        child.terminate();
    }
    bail!("Native PowerShell session did not become ready");
}

/// CDXC:PlatformSupport 2026-09-28 WHY:
/// Two gxserver requests can start the same new session at once (the first session after onboarding did). The second host loses the session lock and exits with status 1 while the first is still coming up, which surfaced "Native session host exited with status 1" for a session that was running fine. A start whose host lost that race waits for the host that holds the lock instead.
fn host_holds_lock(name: &str) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path(name).with_extension("lock"))
        .is_ok_and(|lock| lock.try_lock().is_err())
}

/// CDXC:Terminal 2026-10-04 WHY:
/// The session's ConPTY moves down a row in the same column with a bare LF (`ESC[K LF`), as on a real VT with LNM off. An attachment's own console (the desktop's ConPTY) without `DISABLE_NEWLINE_AUTO_RETURN` turned every such LF into CR LF, so text Claude Code's `/usage` redrew at column 4 landed at column 1 and the leftovers stuck at the left edge. The flag also gives that console the VT's delayed wrap at the last column.
pub(crate) struct ConsoleMode {
    input: windows_sys::Win32::Foundation::HANDLE,
    output: windows_sys::Win32::Foundation::HANDLE,
    input_mode: u32,
    output_mode: u32,
}

impl ConsoleMode {
    pub(crate) fn raw() -> Result<Self> {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut input_mode = 0;
            let mut output_mode = 0;
            if GetConsoleMode(input, &mut input_mode) == 0
                || GetConsoleMode(output, &mut output_mode) == 0
            {
                bail!("Native terminal attachment requires a Windows console");
            }
            let mode = Self {
                input,
                output,
                input_mode,
                output_mode,
            };
            if SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS) == 0
                || SetConsoleMode(
                    output,
                    output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
                ) == 0
            {
                bail!("Unable to enable native terminal input");
            }
            Ok(mode)
        }
    }
}

impl Drop for ConsoleMode {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.input, self.input_mode);
            windows_sys::Win32::System::Console::SetConsoleMode(self.output, self.output_mode);
        }
    }
}

/// CDXC:SessionStatus 2026-09-14 WHY:
/// gxserver observes titles independently of terminal attachments. A missing native watch-title command caused repeated failed launches and stale agent status.
pub(crate) fn watch_title(name: &str) -> Result<()> {
    let endpoint = endpoint(name)?;
    let mut stream = connect(&endpoint)?;
    stream.set_read_timeout(None)?;
    write_frame(
        &mut stream,
        &Request {
            token: endpoint.token,
            operation: "watch-title".into(),
            data: Value::Null,
        },
    )?;
    let mut reader = BufReader::new(stream);
    let mut output = std::io::stdout().lock();
    loop {
        let frame: Value = read_frame(&mut reader)?;
        if let Some(error) = frame.get("error").and_then(Value::as_str) {
            bail!("{error}");
        }
        write_frame(&mut output, &frame)?;
        output.flush()?;
    }
}

/// Holds the session's chat claim until stdin closes or the session ends. Prints one
/// `{"ok":true}` line once the daemon holds it; a daemon without the `chat-claim`
/// capability answers with an error instead.
pub(crate) fn chat_claim(name: &str) -> Result<()> {
    let endpoint = endpoint(name)?;
    let mut stream = connect(&endpoint)?;
    write_frame(
        &mut stream,
        &Request {
            token: endpoint.token,
            operation: "chat-claim".into(),
            data: Value::Null,
        },
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let reply: Value = read_frame(&mut reader)?;
    if let Some(message) = reply.get("error").and_then(Value::as_str) {
        bail!("{message}");
    }
    {
        let mut output = std::io::stdout().lock();
        write_frame(&mut output, &reply)?;
    }
    // The clone keeps the five-second read timeout `connect` set; a timeout is not the end of the
    // session, so the reading half waits without one.
    reader.get_ref().set_read_timeout(None)?;
    // The daemon closes the connection when its session ends.
    thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        std::process::exit(0);
    });
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

pub(crate) use super::attachment::attach;
