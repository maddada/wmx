/// Capture only the requested recent rows. Limit bytes before base64/JSON framing.
pub(crate) fn capture(screen: &vt100::Screen, rows: usize, vt: bool) -> Vec<u8> {
    capture_inner(screen, rows, vt, false)
}

/// CDXC:Zmx 2026-09-24 WHY:
/// Replayed history must cross the client's viewport before the live-grid clear; otherwise short scrollback is erased on attachment. zmx's serializeTerminalState follows the same two-phase replay contract.
pub(crate) fn snapshot(screen: &vt100::Screen) -> Vec<u8> {
    capture_inner(screen, 10_000, true, true)
}

fn capture_inner(screen: &vt100::Screen, rows: usize, vt: bool, replay: bool) -> Vec<u8> {
    let mut screen = screen.clone();
    let (height, width) = screen.size();
    screen.set_scrollback(rows);
    let available = screen.scrollback();
    let mut lines = std::collections::VecDeque::new();
    let mut bytes = 0;
    let mut remaining = available;
    while remaining > 0 {
        screen.set_scrollback(remaining);
        let count = remaining.min(height as usize);
        let batch: Vec<Vec<u8>> = if vt && !replay {
            (0..count as u16)
                .map(|row| styled_row(&screen, row, width))
                .collect()
        } else if vt {
            screen.rows_formatted(0, width).take(count).collect()
        } else {
            screen
                .rows(0, width)
                .take(count)
                .map(String::into_bytes)
                .collect()
        };
        for mut line in batch {
            line.extend_from_slice(b"\r\n");
            bytes += line.len();
            lines.push_back(line);
            while bytes > 512 * 1024 {
                if let Some(line) = lines.pop_front() {
                    bytes -= line.len();
                }
            }
        }
        remaining -= count;
    }
    screen.set_scrollback(0);
    let current = if vt && !replay {
        (0..height)
            .flat_map(|row| {
                let mut line = styled_row(&screen, row, width);
                line.extend_from_slice(b"\r\n");
                line
            })
            .collect()
    } else if vt {
        screen.state_formatted()
    } else {
        plain_rows(&screen, height, width)
    };
    while bytes + current.len() > 1024 * 1024 && !lines.is_empty() {
        bytes -= lines.pop_front().unwrap().len();
    }
    let replay_history = replay && !lines.is_empty();
    let mut output = Vec::new();
    if replay_history {
        output.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    }
    output.extend(lines.into_iter().flatten());
    if replay_history {
        // Attach elects this client and resizes the grid before snapshotting.
        // Each saved row already ends in CRLF; flush the remaining viewport.
        for _ in 1..height {
            output.extend_from_slice(b"\r\n");
        }
    }
    output.extend(current);
    output
}

/// CDXC:SessionChat 2026-09-20 WHY:
/// The live grid is emitted as physical rows, exactly like the scrollback batches
/// above it and like zmx's own serializer. `vt100::Screen::contents` drops the
/// newline after any row carrying the soft-wrap flag, and ConPTY repaints a row
/// that fills the last column by letting the cursor auto-wrap instead of writing
/// a line break, so every full-width row was glued to the row beneath it. That
/// merged an agent's composer rules with its input and statusline rows, and the
/// shape detectors that look for `rule / marker / rule` then reported a ready
/// input box as missing.
fn plain_rows(screen: &vt100::Screen, height: u16, width: u16) -> Vec<u8> {
    let mut output = Vec::new();
    for row in screen.rows(0, width).take(height as usize) {
        output.extend_from_slice(row.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output
}

/// CDXC:SessionChat 2026-09-14 WHY:
/// Chat reads styled physical rows like zmx history, not cursor-motion replay.
/// Replay collapses blank composer padding into cursor jumps, making a ready Codex input box look unavailable forever.
fn styled_row(screen: &vt100::Screen, row: u16, width: u16) -> Vec<u8> {
    let mut output = String::from("\x1b[0m");
    let mut previous = None;
    for col in 0..width {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let style = (
            cell.fgcolor(),
            cell.bgcolor(),
            cell.bold(),
            cell.dim(),
            cell.italic(),
            cell.underline(),
            cell.inverse(),
        );
        if previous != Some(style) {
            output.push_str("\x1b[0");
            for (enabled, code) in [
                (style.2, ";1"),
                (style.3, ";2"),
                (style.4, ";3"),
                (style.5, ";4"),
                (style.6, ";7"),
            ] {
                if enabled {
                    output.push_str(code);
                }
            }
            color(&mut output, style.0, 38);
            color(&mut output, style.1, 48);
            output.push('m');
            previous = Some(style);
        }
        if cell.has_contents() {
            output.push_str(cell.contents());
        } else {
            output.push(' ');
        }
    }
    output.push_str("\x1b[0m");
    output.into_bytes()
}

fn color(output: &mut String, color: vt100::Color, code: u8) {
    use std::fmt::Write;
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(index) => {
            let _ = write!(output, ";{code};5;{index}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(output, ";{code};2;{r};{g};{b}");
        }
    }
}
