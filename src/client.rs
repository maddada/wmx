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
            if request(&endpoint.name, "ping", Value::Null).is_ok() {
                endpoints.push(endpoint);
            }
        }
    }
    Ok(endpoints)
}

pub(crate) fn start(launch: Launch) -> Result<()> {
    if request(&launch.name, "ping", Value::Null).is_ok() {
        return Ok(());
    }
    let encoded = STANDARD.encode(serde_json::to_vec(&launch)?);
    let child = super::launch::spawn(&encoded)?;
    for _ in 0..100 {
        if request(&launch.name, "ping", Value::Null).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.exited()? {
            bail!("Native session host exited with status {status}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    child.terminate();
    bail!("Native PowerShell session did not become ready");
}

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
                || SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
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

pub(crate) use super::attachment::attach;
