use super::protocol::*;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rand::RngCore;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

struct Terminal {
    parser: vt100::Parser<super::callbacks::TerminalCallbacks>,
    subscribers: BTreeMap<u64, SyncSender<Vec<u8>>>,
    display: super::display::DisplayPolicy,
    title_subscribers: Vec<SyncSender<String>>,
    titles: super::title_events::Coalescer,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    process: std::os::windows::io::OwnedHandle,
}

/// CDXC:PlatformSupport 2026-09-14 DECISION:
/// User requested native Windows agents and projects without WSL.
/// Each PowerShell session owns its ConPTY in a detached process so closing the desktop or restarting gxserver does not terminate the agent.
pub(crate) fn run(launch: Launch) -> Result<()> {
    fs::create_dir_all(directory())?;
    let endpoint_path = path(&launch.name);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(endpoint_path.with_extension("lock"))?;
    lock.try_lock()
        .context("This native session is already running")?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let size = PtySize {
        rows: super::display::RESTING_ROWS,
        cols: super::display::RESTING_COLS,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = native_pty_system().openpty(size)?;
    let mut command = CommandBuilder::new(&launch.shell);
    command.arg("-NoLogo");
    let startup = launch.startup.unwrap_or_default();
    let encoded = STANDARD.encode(
        startup
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    command.args(["-NoExit", "-EncodedCommand", &encoded]);
    command.cwd(&launch.cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env(
        "TERM_PROGRAM",
        std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "wmx".into()),
    );
    command.env("WMX_SESSION", &launch.name);
    command.env("ZMX_SESSION", &launch.name);
    if let Some(directory) = std::env::current_exe()?.parent() {
        let mut paths = vec![directory.to_path_buf()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command.env("PATH", std::env::join_paths(paths)?);
    }
    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let shell_pid = child
        .process_id()
        .context("PowerShell did not return a process id")?;
    let terminal = Arc::new(Mutex::new(Terminal {
        parser: vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            10_000,
            super::callbacks::TerminalCallbacks::default(),
        ),
        subscribers: BTreeMap::new(),
        display: super::display::DisplayPolicy::default(),
        title_subscribers: Vec::new(),
        titles: super::title_events::Coalescer::default(),
        master: pair.master,
        writer,
        process: unsafe {
            std::os::windows::io::BorrowedHandle::borrow_raw(
                child
                    .as_raw_handle()
                    .context("Missing PowerShell process handle")?,
            )
        }
        .try_clone_to_owned()?,
    }));
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let endpoint = Endpoint {
        protocol: 1,
        name: launch.name,
        port: listener.local_addr()?.port(),
        token: STANDARD.encode(secret),
        pid: std::process::id(),
        shell_pid,
    };
    let alive = Arc::new(AtomicBool::new(true));
    let output_terminal = terminal.clone();
    let output_alive = alive.clone();
    thread::spawn(move || {
        let mut buffer = [0u8; 16_384];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let mut state = output_terminal.lock().unwrap_or_else(|e| e.into_inner());
            let previous = state.parser.screen().clone();
            let previous_title = state.parser.callbacks().title.clone();
            state.parser.process(&buffer[..count]);
            let title = state.parser.callbacks().title.clone();
            if title != previous_title {
                state.titles.observe(&title, Instant::now());
            }
            let replies = std::mem::take(&mut state.parser.callbacks_mut().replies);
            if !replies.is_empty() {
                if state
                    .writer
                    .write_all(&replies)
                    .and_then(|_| state.writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            let mut output = state.parser.screen().state_diff(&previous);
            output.append(&mut state.parser.callbacks_mut().events);
            if !output.is_empty() {
                state
                    .subscribers
                    .retain(|_, client| client.try_send(output.clone()).is_ok());
            }
        }
        output_alive.store(false, Ordering::Release);
    });
    // The registry file is published only after ConPTY and its reader are ready.
    let temporary = endpoint_path.with_extension("new");
    fs::write(&temporary, serde_json::to_vec(&endpoint)?)?;
    if endpoint_path.exists() {
        fs::remove_file(&endpoint_path)?;
    }
    fs::rename(temporary, &endpoint_path)?;
    listener.set_nonblocking(true)?;
    let mut workers = Vec::new();
    while alive.load(Ordering::Acquire) && child.try_wait()?.is_none() {
        {
            let mut state = terminal.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(title) = state.titles.take_due(Instant::now()) {
                state
                    .title_subscribers
                    .retain(|client| client.try_send(title.clone()).is_ok());
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                workers.retain(|worker: &thread::JoinHandle<()>| !worker.is_finished());
                if workers.len() >= 64 {
                    drop(stream);
                    continue;
                }
                let state = terminal.clone();
                let endpoint = endpoint.clone();
                workers.push(thread::spawn(move || {
                    let mut error_stream = stream.try_clone().ok();
                    if let Err(error) = serve(stream, state, &endpoint) {
                        if let Some(stream) = error_stream.as_mut() {
                            let _ = write_frame(stream, &json!({"error": error.to_string()}));
                        }
                    }
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
    {
        let mut state = terminal.lock().unwrap_or_else(|error| error.into_inner());
        state.subscribers.clear();
        state.title_subscribers.clear();
    }
    for worker in workers {
        let _ = worker.join();
    }
    let _ = fs::remove_file(&endpoint_path);
    Ok(())
}

fn serve(mut stream: TcpStream, terminal: Arc<Mutex<Terminal>>, endpoint: &Endpoint) -> Result<()> {
    // Winsock inherits the listener's nonblocking mode. A client may connect
    // before it writes its first frame, so restore blocking reads with a deadline.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let request: Request = read_frame(&mut reader)?;
    if request.token.len() != endpoint.token.len()
        || !bool::from(request.token.as_bytes().ct_eq(endpoint.token.as_bytes()))
    {
        bail!("Session authentication failed");
    }
    if request.operation == "watch-title" {
        let (tx, rx) = sync_channel(64);
        {
            let mut state = terminal.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(title) = state.titles.last_emitted() {
                write_frame(&mut stream, &json!({"title": title}))?;
            }
            state.title_subscribers.push(tx);
        }
        stream.set_read_timeout(Some(Duration::from_millis(1)))?;
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(title) => write_frame(&mut stream, &json!({"title": title}))?,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match stream.peek(&mut [0u8; 1])
                {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(error.into()),
                },
            }
        }
        return Ok(());
    }
    if request.operation == "attach" {
        let (tx, rx) = sync_channel(256);
        let client_id = request
            .data
            .get("clientId")
            .and_then(Value::as_u64)
            .unwrap_or_else(rand::random);
        {
            let mut state = terminal.lock().unwrap_or_else(|e| e.into_inner());
            if state.subscribers.contains_key(&client_id) {
                bail!("Duplicate client id");
            }
            let (current_rows, current_cols) = state.parser.screen().size();
            let rows = dimension(&request.data, "rows").unwrap_or(current_rows);
            let cols = dimension(&request.data, "cols").unwrap_or(current_cols);
            let prompt_editor = request
                .data
                .get("promptEditor")
                .and_then(Value::as_str)
                .filter(|value| matches!(*value, "monaco" | "code-server"))
                .map(str::to_string);
            state.display.attach(client_id, rows, cols, prompt_editor);
            state.elect_grid()?;
            let snapshot = state.snapshot(true);
            write_frame(&mut stream, &json!({"output": STANDARD.encode(snapshot)}))?;
            state.subscribers.insert(client_id, tx);
        }
        let _attachment = Attachment {
            terminal: terminal.clone(),
            client_id,
        };
        stream.set_read_timeout(Some(Duration::from_millis(1)))?;
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(output) => {
                    write_frame(&mut stream, &json!({"output": STANDARD.encode(output)}))?
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Reopening an idle terminal must release the previous attachment even when the shell produces no output.
                    match stream.peek(&mut [0u8; 1]) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        return Ok(());
    }
    let response = {
        let mut state = terminal.lock().unwrap_or_else(|e| e.into_inner());
        match request.operation.as_str() {
            "ping" => {
                json!({"pid": endpoint.pid, "shellPid": endpoint.shell_pid, "name": endpoint.name,
                    "capabilities": ["client-visibility", "refresh", "detach"], "wire_generation": WIRE_GENERATION})
            }
            "input" => {
                let bytes = STANDARD.decode(
                    request
                        .data
                        .as_str()
                        .or_else(|| request.data.get("bytes").and_then(Value::as_str))
                        .context("Missing input")?,
                )?;
                if let Some(id) = request.data.get("clientId").and_then(Value::as_u64) {
                    state.display.input(id);
                    state.elect_grid()?;
                }
                state.writer.write_all(&bytes)?;
                state.writer.flush()?;
                json!({"ok": true})
            }
            "refresh-if-stale" => {
                let rows = dimension(&request.data, "rows")?;
                let cols = dimension(&request.data, "cols")?;
                let applied = state.parser.screen().size() != (rows, cols);
                if applied {
                    state.resize(rows, cols)?;
                }
                json!({"applied": applied})
            }
            "resize" => {
                let rows = dimension(&request.data, "rows")?;
                let cols = dimension(&request.data, "cols")?;
                if let Some(id) = request.data.get("clientId").and_then(Value::as_u64) {
                    state.display.resize(id, rows, cols);
                    state.elect_grid()?;
                } else {
                    state.resize(rows, cols)?;
                }
                json!({"ok": true})
            }
            "visibility" => {
                let id = request
                    .data
                    .get("clientId")
                    .and_then(Value::as_u64)
                    .context("Missing client id")?;
                let visibility = request
                    .data
                    .get("state")
                    .and_then(Value::as_str)
                    .and_then(super::display::Visibility::parse)
                    .context("Invalid visibility")?;
                let rows = dimension(&request.data, "rows")?;
                let cols = dimension(&request.data, "cols")?;
                state.display.visibility(id, visibility, rows, cols);
                state.elect_grid()?;
                json!({"ok": true})
            }
            "refresh" => {
                let bytes = state.parser.screen().state_formatted();
                if let Some(id) = request.data.get("clientId").and_then(Value::as_u64) {
                    if let Some(client) = state.subscribers.get(&id) {
                        let _ = client.try_send(bytes);
                    }
                } else {
                    state
                        .subscribers
                        .retain(|_, client| client.try_send(bytes.clone()).is_ok());
                }
                json!({"ok": true})
            }
            "detach" => {
                state.subscribers.clear();
                state.display = super::display::DisplayPolicy::default();
                json!({"ok": true})
            }
            "history" => {
                let vt = request.data.get("vt").and_then(Value::as_bool) == Some(true);
                let scrollback = request
                    .data
                    .get("scrollback")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(10_000) as usize;
                let bytes = super::history::capture(state.parser.screen(), scrollback, vt);
                if vt {
                    json!({"output": STANDARD.encode(bytes)})
                } else {
                    json!({"text": String::from_utf8_lossy(&bytes)})
                }
            }
            "prompt-editor-capability" => json!({"capability": state.display.prompt_editor()}),
            "grid" => {
                let (rows, cols) = state.parser.screen().size();
                state.display.snapshot(rows, cols)
            }
            "kill" => {
                // CDXC:PlatformSupport 2026-09-14 WHY:
                // portable-pty 0.9's cloned Windows killer treats successful TerminateProcess as an error.
                // Use the owned process handle and the Win32 success convention so sleep receives its acknowledgement.
                use std::os::windows::io::AsRawHandle;
                if unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        state.process.as_raw_handle(),
                        1,
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                json!({"ok": true})
            }
            _ => bail!("Unknown native session operation"),
        }
    };
    write_frame(&mut stream, &response)
}

fn dimension(data: &Value, key: &str) -> Result<u16> {
    data.get(key)
        .and_then(Value::as_u64)
        .filter(|value| (1..=2000).contains(value))
        .map(|value| value as u16)
        .with_context(|| format!("Invalid terminal {key}"))
}

impl Terminal {
    fn snapshot(&self, scrollback: bool) -> Vec<u8> {
        let mut output = if scrollback {
            super::history::snapshot(self.parser.screen())
        } else {
            self.parser.screen().state_formatted()
        };
        let title = &self.parser.callbacks().title;
        if !title.is_empty() {
            output.extend_from_slice(b"\x1b]2;");
            output.extend_from_slice(title.as_bytes());
            output.push(7);
        }
        output
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        if self.parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.screen_mut().set_size(rows, cols);
        let snapshot = self.snapshot(false);
        self.subscribers
            .retain(|_, client| client.try_send(snapshot.clone()).is_ok());
        Ok(())
    }
    fn elect_grid(&mut self) -> Result<()> {
        let (rows, cols) = self.display.grid(self.parser.screen().size());
        self.resize(rows, cols)
    }
}

struct Attachment {
    terminal: Arc<Mutex<Terminal>>,
    client_id: u64,
}
impl Drop for Attachment {
    fn drop(&mut self) {
        let mut state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.subscribers.remove(&self.client_id);
        state.display.remove(self.client_id);
        let _ = state.elect_grid();
    }
}
