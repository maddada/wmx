use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, Write},
    path::PathBuf,
};

/// Bump only for incompatible IPC changes; additive operations are capability-probed.
/// The generation is independent of zmx's wire number. Ghostex records it per session.
pub(crate) const WIRE_GENERATION: u32 = 1;
pub(crate) const MAX_FRAME: usize = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(crate) struct Launch {
    pub name: String,
    pub cwd: String,
    pub shell: String,
    pub startup: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub protocol: u32,
    pub name: String,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub shell_pid: u32,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub token: String,
    pub operation: String,
    #[serde(default)]
    pub data: Value,
}

pub(crate) fn directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("WMX_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(directory);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wmx")
        .join("sessions")
}

pub(crate) fn path(name: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(name.as_bytes()));
    directory().join(format!("{key}.json"))
}

pub(crate) fn endpoint(name: &str) -> Result<Endpoint> {
    let endpoint: Endpoint = serde_json::from_slice(&fs::read(path(name))?)?;
    if endpoint.protocol != 1 {
        bail!("This native session uses an unsupported protocol version");
    }
    if endpoint.name != name {
        bail!("Session endpoint identity does not match");
    }
    Ok(endpoint)
}

pub(crate) fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        bail!("Session message exceeds its size limit");
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame<T: for<'a> Deserialize<'a>>(reader: &mut impl BufRead) -> Result<T> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            bail!("Session connection closed");
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        if bytes.len() + count > MAX_FRAME {
            bail!("Session message exceeds its size limit");
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    serde_json::from_slice(&bytes).context("Invalid session message")
}
