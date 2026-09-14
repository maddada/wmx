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
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
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
    emit(&first)?;
    let alive = Arc::new(AtomicBool::new(true));
    let input_alive = alive.clone();
    let input_name = name.to_string();
    let input_socket = stream.try_clone()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 8192];
        while let Ok(count) = stdin.read(&mut buffer) {
            if count == 0 || tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        let mut filter = InputFilter::default();
        let detach_key = std::env::var_os("ZMX_NO_DETACH_KEY").is_none()
            && std::env::var_os("WMX_NO_DETACH_KEY").is_none();
        'input: while input_alive.load(Ordering::Acquire) {
            let events = match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(bytes) => filter.feed(&bytes, detach_key),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => filter.flush(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            for event in events {
                let (operation, data) = match event {
                    Input::Detach => break 'input,
                    Input::Bytes(bytes) => (
                        "input",
                        if supports_visibility {
                            json!({"clientId": client_id, "bytes": STANDARD.encode(bytes)})
                        } else {
                            json!(STANDARD.encode(bytes))
                        },
                    ),
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
        }
        input_alive.store(false, Ordering::Release);
        let _ = input_socket.shutdown(Shutdown::Both);
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
            emit(&frame)?;
        }
        Ok(())
    })();
    alive.store(false, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn emit(frame: &Value) -> Result<()> {
    if let Some(error) = frame.get("error").and_then(Value::as_str) {
        bail!("{error}");
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
