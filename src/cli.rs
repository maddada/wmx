use super::{
    client, daemon,
    protocol::{read_frame, Launch},
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};

pub(crate) fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("");
    let argument = |index: usize| {
        args.get(index)
            .map(String::as_str)
            .context("Missing session argument")
    };
    match verb {
        "serve-encoded" => daemon::run(serde_json::from_slice(&STANDARD.decode(argument(1)?)?)?),
        "serve" => daemon::run(read_frame(&mut BufReader::new(std::io::stdin().lock()))?),
        "start-encoded" => client::start(serde_json::from_slice(&STANDARD.decode(argument(1)?)?)?),
        "start" => client::start(read_frame::<Launch>(&mut BufReader::new(
            std::io::stdin().lock(),
        ))?),
        "list" | "ls" | "l" => {
            for endpoint in client::list()? {
                if args.iter().any(|arg| arg == "--short") {
                    println!("{}", endpoint.name);
                } else {
                    println!(
                        "{}",
                        json!({"name": endpoint.name, "pid": endpoint.pid, "shellPid": endpoint.shell_pid})
                    );
                }
            }
            Ok(())
        }
        "version" | "v" | "--version" => {
            println!(
                "version\t{}\nwire_generation\t{}\nprovider\twmx\nsession_dir\t{}",
                env!("CARGO_PKG_VERSION"),
                super::protocol::WIRE_GENERATION,
                super::protocol::directory().display()
            );
            Ok(())
        }
        "help" | "--help" | "-h" | "" => {
            println!("wmx: persistent Windows terminal sessions\nCommands: start, start-encoded, attach, list [--short], exists, send, history [--vt] [--scrollback N], grid, resize, refresh, refresh-if-stale, watch-title, detach, kill, version");
            Ok(())
        }
        "detach" | "d" => {
            let name = args
                .get(1)
                .cloned()
                .or_else(|| std::env::var("WMX_SESSION").ok())
                .or_else(|| std::env::var("ZMX_SESSION").ok())
                .context("Not inside a wmx session")?;
            client::request(&name, "detach", Value::Null)?;
            Ok(())
        }
        "refresh" => {
            client::request(argument(1)?, "refresh", Value::Null)?;
            Ok(())
        }
        "refresh-if-stale" => {
            let rows = argument(2)?.parse::<u16>()?;
            let cols = argument(3)?.parse::<u16>()?;
            let reply = client::request(
                argument(1)?,
                "refresh-if-stale",
                json!({"rows": rows, "cols": cols}),
            )?;
            let stale = reply["applied"]
                .as_bool()
                .context("Missing refresh acknowledgement")?;
            println!(
                "refresh-if-stale {}",
                if stale { "applied" } else { "skipped" }
            );
            Ok(())
        }
        "process-snapshot" => super::process_snapshot::print(),
        "inspect" => {
            let owner = super::process_owner::inspect(argument(1)?)?;
            println!(
                "{}",
                match owner {
                    Some(endpoint) =>
                        json!({"exists": true, "pid": endpoint.pid, "shellPid": endpoint.shell_pid}),
                    None => json!({"exists": false}),
                }
            );
            Ok(())
        }
        "exists" => {
            client::request(argument(1)?, "ping", Value::Null)?;
            Ok(())
        }
        "attach" | "a" => {
            let mut name = None;
            let mut editor = None;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--require-existing" => {}
                    "--prompt-editor" => {
                        index += 1;
                        editor = Some(argument(index)?);
                    }
                    value if value.starts_with("--prompt-editor=") => {
                        editor = value.strip_prefix("--prompt-editor=")
                    }
                    value if !value.starts_with('-') && name.is_none() => name = Some(value),
                    _ => bail!("Unsupported attach argument"),
                }
                index += 1;
            }
            if editor.is_some_and(|value| !matches!(value, "monaco" | "code-server")) {
                bail!("Unsupported prompt editor");
            }
            client::attach(name.context("Missing session name")?, editor)
        }
        "prompt-editor-capability" => {
            let name = args
                .get(1)
                .cloned()
                .or_else(|| std::env::var("WMX_SESSION").ok())
                .or_else(|| std::env::var("ZMX_SESSION").ok())
                .context("Not inside a wmx session")?;
            let reply = client::request(&name, "prompt-editor-capability", Value::Null)?;
            println!("{}", reply["capability"].as_str().unwrap_or("editor"));
            Ok(())
        }
        "watch-title" => client::watch_title(argument(1)?),
        "kill" | "k" => {
            if args.get(1).is_some_and(|arg| arg == "--force") {
                return super::process_owner::force_kill(argument(2)?);
            }
            for name in args.iter().skip(1) {
                client::request(name, "kill", Value::Null)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while super::protocol::path(name).exists() {
                    if std::time::Instant::now() >= deadline {
                        bail!("Session did not finish shutting down");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            Ok(())
        }
        "send" | "s" => {
            let mut bytes = args
                .iter()
                .skip(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
                .into_bytes();
            if args.len() == 2 {
                std::io::stdin()
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
            }
            if bytes.len() > 1024 * 1024 {
                bail!("Input exceeds 1 MiB");
            }
            client::request(argument(1)?, "input", json!(STANDARD.encode(bytes)))?;
            Ok(())
        }
        "history" | "hi" => {
            let vt = args.iter().any(|arg| arg == "--vt");
            let scrollback = args
                .iter()
                .position(|arg| arg == "--scrollback")
                .and_then(|index| args.get(index + 1))
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(10_000);
            let reply = client::request(
                argument(1)?,
                "history",
                json!({"vt": vt, "scrollback": scrollback}),
            )?;
            if vt {
                std::io::stdout().write_all(
                    &STANDARD.decode(
                        reply["output"]
                            .as_str()
                            .context("Missing terminal snapshot")?,
                    )?,
                )?;
            } else {
                print!(
                    "{}",
                    reply["text"].as_str().context("Missing terminal text")?
                );
            }
            Ok(())
        }
        "grid" => {
            println!("{}", client::request(argument(1)?, "grid", Value::Null)?);
            Ok(())
        }
        "resize" => {
            client::request(
                argument(1)?,
                "resize",
                json!({"rows": argument(2)?.parse::<u16>()?, "cols": argument(3)?.parse::<u16>()?}),
            )?;
            Ok(())
        }
        _ => bail!("Expected start, attach, list, exists, send, history, grid, resize, or kill"),
    }
}
