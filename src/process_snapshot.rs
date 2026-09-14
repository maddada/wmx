use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    io::Write,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

/// CDXC:PlatformSupport 2026-09-14 WHY:
/// CREATE_NO_WINDOW does not propagate to PowerShell's native-command grandchildren.
/// Listing sessions inside the host avoids a visible Windows Terminal launch on every background identity poll.
pub(crate) fn print() -> Result<()> {
    println!("__GHOSTEX_ZMX_LIST__");
    for session in super::client::list()? {
        println!("name={} pid={}", session.name, session.shell_pid);
    }
    println!("__GHOSTEX_PS__");
    let shell = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| "C:/Windows".into()),
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let script = "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); Get-CimInstance Win32_Process | ForEach-Object { [string]$_.ProcessId + ' ' + [string]$_.ParentProcessId + ' ?? ' + $_.CommandLine }";
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let output = Command::new(shell)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("Unable to read Windows process identities");
    }
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}
