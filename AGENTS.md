# Working on wmx

Read README.md before changing session behavior. wmx implements the subset of zmx consumed by Ghostex. A change in either provider must be checked against the shared contract table there; update both implementations or document why the other provider is unaffected.

Keep Ghostex-specific paths, settings, agent startup, and prompt-editor setup in Ghostex's `server/src/zmx/scripts_windows.rs` adapter. wmx owns ConPTY, terminal state, client display leadership, authenticated IPC, and persistent process lifecycle.

Build and verify on native Windows with `cargo build --release --locked` and `./tests/windows-smoke.ps1 -WmxExe ./target/release/wmx.exe`. Preserve PowerShell 5.1 and PowerShell 7 support. Client EOF and app/server restart must not kill a session. Do not stop user sessions to test a change; use a unique `WMX_DIR`.

Only incompatible wire changes increment `WIRE_GENERATION` in `src/protocol.rs`. Additive features use capability probes. A generation bump can terminate live agents when Ghostex next starts, so explain that impact before installation. Never substitute a binary hash for wire compatibility.
