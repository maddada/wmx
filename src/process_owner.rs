use super::protocol::{self, Endpoint};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path, time::UNIX_EPOCH};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0},
    System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    },
};

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Registry liveness is independent of IPC, so a busy or older daemon stays observable.
/// Check image identity and creation time before trusting a registry PID that Windows may have reused.
fn owner(name: &str, terminate: bool) -> Result<Option<(Endpoint, Process)>> {
    let path = protocol::path(name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let endpoint: Endpoint = serde_json::from_slice(&bytes)?;
    if endpoint.name != name {
        bail!("Session endpoint identity does not match");
    }
    let access = PROCESS_QUERY_LIMITED_INFORMATION
        | PROCESS_SYNCHRONIZE
        | if terminate { PROCESS_TERMINATE } else { 0 };
    let raw = unsafe { OpenProcess(access, 0, endpoint.pid) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(87) {
            return Ok(None);
        }
        return Err(error.into());
    }
    let process = Process(raw);
    let mut status = 0;
    if unsafe { GetExitCodeProcess(raw, &mut status) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if status != 259 {
        return Ok(None);
    }
    let mut image = vec![0u16; 32768];
    let mut size = image.len() as u32;
    if unsafe { QueryFullProcessImageNameW(raw, 0, image.as_mut_ptr(), &mut size) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let image = String::from_utf16_lossy(&image[..size as usize]);
    let filename = Path::new(&image)
        .file_name()
        .context("Missing process image")?
        .to_string_lossy()
        .to_lowercase();
    if !matches!(filename.as_str(), "wmx.exe" | "ghostex-session-host.exe") {
        return Ok(None);
    }
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    if unsafe { GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let created = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    let modified = fs::metadata(path)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        / 100
        + 116_444_736_000_000_000;
    if u128::from(created) > modified {
        return Ok(None);
    }
    Ok(Some((endpoint, process)))
}

pub fn inspect(name: &str) -> Result<Option<Endpoint>> {
    Ok(owner(name, false)?.map(|(endpoint, _)| endpoint))
}

/// Explicit recovery for an incompatible/unresponsive daemon, never an automatic fallback for a busy compatible one.
pub fn force_kill(name: &str) -> Result<()> {
    let Some((endpoint, process)) = owner(name, true)? else {
        return Ok(());
    };
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { WaitForSingleObject(process.0, 5000) } != WAIT_OBJECT_0 {
        bail!("Session host did not exit");
    }
    let path = protocol::path(name);
    if let Ok(bytes) = fs::read(&path) {
        let current: Endpoint = serde_json::from_slice(&bytes)?;
        if current.pid == endpoint.pid && current.token == endpoint.token {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}
