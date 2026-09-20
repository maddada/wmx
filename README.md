# wmx

Persistent Windows terminal sessions backed by ConPTY. wmx is the Windows counterpart to [Ghostex's zmx fork](https://github.com/maddada/zmx). It implements the terminal session features used by Ghostex desktop, web, and mobile, with PowerShell sessions that survive client disconnection and gxserver restarts.

Build on Windows with Rust 1.89 or newer and the MSVC toolchain:

```powershell
cargo build --release --locked
./tests/windows-smoke.ps1 -WmxExe ./target/release/wmx.exe
```

Windows 10 version 1809 or newer is required for ConPTY. PowerShell 7 is recommended; Windows PowerShell 5.1 also works. wmx is an independent MIT-licensed program, extracted from Ghostex's original `ghostex-session-host.exe`.

## Session API

| Operation     | wmx interface                                                              | zmx behavior preserved                                                                                  |
| ------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| Start         | `start` with one JSON line on stdin, or `start-encoded BASE64_JSON`        | Idempotent detached session creation; existing sessions keep their process and environment.             |
| Attach        | `attach NAME [--require-existing] [--prompt-editor monaco\|code-server]`   | Snapshot followed by live VT output, raw input, reconnect, and Ctrl-\\ detach.                          |
| Inventory     | `list [--short]`, `exists NAME`, `grid NAME`                               | Named sessions, liveness, and display leadership/grid metadata.                                         |
| Input         | `send NAME [TEXT ...]`, or raw bytes on stdin                              | Input is delivered to the existing PTY; stdin is bounded to 1 MiB.                                      |
| History       | `history NAME [--vt] [--scrollback N]`                                     | Plain text or VT screen plus bounded scrollback.                                                        |
| Display       | `resize NAME ROWS COLS`, `refresh NAME`, `refresh-if-stale NAME ROWS COLS` | Atomic stale-grid comparison and refreshed snapshots.                                                   |
| Titles        | `watch-title NAME`                                                         | JSON lines, semantic debounce at 1 second, spinner heartbeat at 2 seconds, maximum settle at 6 seconds. |
| Prompt editor | `prompt-editor-capability [NAME]`                                          | Active visible client's editor capability, otherwise `editor`.                                          |
| Detach        | `detach [NAME]`                                                            | Disconnect clients while the shell stays alive.                                                         |
| Stop          | `kill NAME ...`                                                            | Wait for daemon cleanup before returning, allowing immediate restart.                                   |
| Compatibility | `version`                                                                  | Includes a `wire_generation` line used by Ghostex's compatibility cycle.                                |

Aliases: `a`, `l`/`ls`, `s`, `hi`, `d`, `k`, and `v`. An attachment always requires an existing session. Unlike zmx's command-line `run`, the start interface carries native Windows paths and startup code as structured JSON:

```powershell
@{
  name = 'work'
  cwd = $HOME
  shell = (Get-Command pwsh.exe).Source
  startup = "Write-Output 'Ready'"
} | ConvertTo-Json -Compress | ./target/release/wmx.exe start
./target/release/wmx.exe attach work
```

The shell receives `WMX_SESSION` and `ZMX_SESSION`. `WMX_DIR` chooses the session registry; the default is `%LOCALAPPDATA%/wmx/sessions`. Ghostex sets its own registry directory and agent environment through its server adapter. wmx contains no Ghostex path or prompt-editor setup policy.

`inspect NAME` reports registry/process liveness without asking the daemon to answer IPC. `kill --force NAME` is explicit recovery for an incompatible or unresponsive daemon. It verifies the process image and creation time before terminating the recorded owner, protecting against reused PIDs.

## Shared behavior and maintenance

Formatted history exports physical rows with literal padding and SGR styles, matching the chat parser contract. Plain history exports the same physical rows without styling, so a row that fills the last column keeps its own line instead of being glued to the row below it. Attachment snapshots separately restore cursor position and terminal modes.

This is API and behavior compatibility, not zmx binary wire compatibility. wmx uses authenticated, bounded JSON frames over per-session loopback TCP. Registry records contain a random per-session token. zmx uses its Unix IPC protocol. Keep registry data private to the Windows account.

Every change to a Ghostex-consumed zmx feature must be reviewed against this table and the corresponding wmx modules:

| Shared contract                                                                     | zmx source                     | wmx source                                               |
| ----------------------------------------------------------------------------------- | ------------------------------ | -------------------------------------------------------- |
| Visible/chat/parked clients, latest active visible leader, 200-column resting width | `src/loop.zig`, `src/ipc.zig`  | `src/display.rs`, `src/daemon.rs`                        |
| `ZMX_VISIBLE`, `ZMX_CHAT`, `ZMX_HIDDEN`, `ZMX_REFRESH` private OSCs                 | `src/loop.zig`                 | `src/input.rs`, `src/attachment.rs`                      |
| Screen/history and refresh                                                          | `src/loop.zig`                 | `src/history.rs`, `src/daemon.rs`                        |
| Titles and spinner normalization                                                    | `src/title_events.zig`         | `src/title_events.rs`, `src/callbacks.rs`                |
| Prompt-editor client capability                                                     | `src/ipc.zig`, `src/loop.zig`  | `src/display.rs`, `src/attachment.rs`                    |
| Detached lifecycle and recovery                                                     | `src/main.zig`, `src/loop.zig` | `src/launch.rs`, `src/client.rs`, `src/process_owner.rs` |

Ghostex's adapters live in `server/src/zmx/scripts.rs` and `scripts_windows.rs`. The desktop, web, and mobile private OSC emitters stay byte-identical. Wire generations are independent between providers. Bump wmx's `WIRE_GENERATION` in `src/protocol.rs` only when old daemons cannot serve new clients; probe additive capabilities so compatible daemons survive app updates. The original session-host protocol is generation 1 and remains supported.

Run the Windows smoke script after changes to either side of these contracts. It exercises real ConPTY sessions, delayed first requests, competing attachments, display leadership, prompt-editor handoff, persistent shell/history, stdin EOF, and graceful/forced shutdown.

## Future work

Features Ghostex does not currently require remain future work: arbitrary executable `run`/`r` syntax and implicit attach-or-create, session labels and rename, HTML history export, key-name send syntax, detach-all across all sessions, arbitrary non-PowerShell shells, and upstream zmx features outside the table. They should be added when a Ghostex client needs them, with the corresponding zmx behavior documented and verified.
