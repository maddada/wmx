use anyhow::Result;
use std::{collections::HashMap, io::Write, ptr};
use windows_sys::{
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    },
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// CDXC:PlatformSupport 2026-09-27 WHY:
/// The process table is read natively (Toolhelp for pid and parent pid, NtQueryInformationProcess for each command line) instead of through PowerShell's `Get-CimInstance Win32_Process`.
/// The WMI query took 5.4s with about 35 sessions running, longer than gxserver's 5s probe timeout, so every identity probe failed and gxserver ran the next one straight after it; the probes held gxserver's request threads and chat sends timed out.
/// The output keeps the `ps -axo pid=,ppid=,tty=,command=` shape gxserver parses, with `??` as the tty and no line breaks inside a command line.
/// CDXC:SessionIdentity 2026-10-05 WHY:
/// Windows never reparents an orphan, so a process keeps the pid of a parent that exited, and Windows hands that pid to new processes. Every wmx daemon is such an orphan (its launcher exits). When a short-lived tool process inside one session reused a daemon's old parent pid, gxserver's tree walk found that whole other session (its `claude --resume <id>`) below the first session's agent, took the deeper process as the session's agent, and the thread got the coordinator's conversation id, title and final message (observed 2026-10-04, thread G0hhy and coordinator G4snt). A parent pid is printed only when both processes' creation times can be read and the parent was created first; otherwise the row has parent 0. A link that cannot be checked does not count: a pwsh that could not be opened (no command line, no times) still parented a session's wmx daemon while its own dead parent's pid belonged to a new `sleep`.
/// SEE-ALSO: Ghostex server/src/zmx/process_identity.rs `resolve_process_tree_agent_identity` (the tree walk this output feeds).
pub(crate) fn print() -> Result<()> {
    let mut output = String::from("__GHOSTEX_ZMX_LIST__\n");
    for session in super::client::list()? {
        output.push_str(&format!(
            "name={} pid={}\n",
            session.name, session.shell_pid
        ));
    }
    output.push_str("__GHOSTEX_PS__\n");
    let rows = processes()?
        .into_iter()
        .map(|(pid, parent_pid)| {
            let details = details(pid);
            (pid, parent_pid, details)
        })
        .collect::<Vec<_>>();
    let created = rows
        .iter()
        .filter_map(|(pid, _, details)| Some((*pid, details.as_ref()?.created?)))
        .collect::<HashMap<_, _>>();
    for (pid, parent_pid, details) in &rows {
        let parent_pid = match (created.get(parent_pid), created.get(pid)) {
            (Some(parent), Some(child)) if parent <= child => *parent_pid,
            _ => 0,
        };
        let command = details
            .as_ref()
            .map(|details| details.command.replace(['\r', '\n'], " "))
            .unwrap_or_default();
        output.push_str(&format!("{pid} {parent_pid} ?? {command}\n"));
    }
    std::io::stdout().write_all(output.as_bytes())?;
    Ok(())
}

fn processes() -> Result<Vec<(u32, u32)>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let snapshot = Handle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut processes = Vec::new();
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    loop {
        processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            break;
        }
    }
    Ok(processes)
}

struct Details {
    command: String,
    /// Creation time in 100ns FILETIME units.
    created: Option<u64>,
}

/// `None` when the process cannot be opened (another user's or a protected process), which is
/// where WMI reported no command line either.
fn details(pid: u32) -> Option<Details> {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    let process = Handle(raw);
    Some(Details {
        command: command_line(&process).unwrap_or_default(),
        created: created(&process),
    })
}

fn created(process: &Handle) -> Option<u64> {
    let mut times: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    let [creation, exit, kernel, user] = &mut times;
    if unsafe { GetProcessTimes(process.0, creation, exit, kernel, user) } == 0 {
        return None;
    }
    Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

fn command_line(process: &Handle) -> Option<String> {
    let mut length = 0u32;
    unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessCommandLineInformation,
            ptr::null_mut(),
            0,
            &mut length,
        );
    }
    if (length as usize) < std::mem::size_of::<UNICODE_STRING>() {
        return None;
    }
    // u64 cells keep the UNICODE_STRING header at the alignment it is read with.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessCommandLineInformation,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut length,
        )
    };
    if status < 0 {
        return None;
    }
    let text = unsafe { &*(buffer.as_ptr() as *const UNICODE_STRING) };
    if text.Buffer.is_null() || text.Length == 0 {
        return Some(String::new());
    }
    let units = unsafe { std::slice::from_raw_parts(text.Buffer, text.Length as usize / 2) };
    Some(String::from_utf16_lossy(units))
}
