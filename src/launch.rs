use anyhow::{Context, Result};
use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, STILL_ACTIVE},
    System::Threading::{
        CreateProcessW, GetExitCodeProcess, TerminateProcess, PROCESS_INFORMATION, STARTUPINFOW,
    },
};

pub(crate) struct DetachedProcess(HANDLE);
impl Drop for DetachedProcess {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl DetachedProcess {
    pub fn exited(&self) -> Result<Option<u32>> {
        let mut status = 0;
        if unsafe { GetExitCodeProcess(self.0, &mut status) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((status != STILL_ACTIVE as u32).then_some(status))
    }
    pub fn terminate(&self) {
        unsafe {
            TerminateProcess(self.0, 1);
        }
    }
}

/// CDXC:PlatformSupport 2026-09-14 WHY:
/// A detached host inherited gxserver's command capture pipe handles, so the start command exited successfully but gxserver waited forever for EOF.
/// The persistent host must inherit the environment without inheriting any handles from its short-lived launcher.
pub(crate) fn spawn(encoded_launch: &str) -> Result<DetachedProcess> {
    let executable = std::env::current_exe()?;
    let application: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let command = format!(
        "\"{}\" serve-encoded {encoded_launch}",
        executable.display()
    );
    let mut command: Vec<u16> = OsStr::new(&command).encode_wide().chain(Some(0)).collect();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0x0000_0008 | 0x0000_0200 | 0x0100_0000,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 && std::io::Error::last_os_error().raw_os_error() == Some(5) {
        // A limited-user Task Scheduler job can deny breakaway even though it permits a persistent detached child.
        let command_text = format!(
            "\"{}\" serve-encoded {encoded_launch}",
            executable.display()
        );
        command = OsStr::new(&command_text)
            .encode_wide()
            .chain(Some(0))
            .collect();
        created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0x0000_0008 | 0x0000_0200,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut process,
            )
        };
    }
    if created == 0 {
        return Err(std::io::Error::last_os_error())
            .context("Unable to detach the native session host");
    }
    unsafe {
        CloseHandle(process.hThread);
    }
    Ok(DetachedProcess(process.hProcess))
}
