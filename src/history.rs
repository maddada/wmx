/// Capture only the requested recent rows. Limit bytes before base64/JSON framing.
pub(crate) fn capture(screen: &vt100::Screen, rows: usize, vt: bool) -> Vec<u8> {
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
        let batch: Vec<Vec<u8>> = if vt {
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
    let current = if vt {
        screen.state_formatted()
    } else {
        screen.contents().into_bytes()
    };
    while bytes + current.len() > 1024 * 1024 && !lines.is_empty() {
        bytes -= lines.pop_front().unwrap().len();
    }
    let mut output: Vec<u8> = lines.into_iter().flatten().collect();
    output.extend(current);
    output
}
