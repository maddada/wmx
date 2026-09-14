#[derive(Default)]
pub(crate) struct TerminalCallbacks {
    pub replies: Vec<u8>,
    pub events: Vec<u8>,
    pub title: String,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if i2.is_some() {
            return;
        }
        let value = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        match (i1, command, value) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.replies.extend_from_slice(b"\x1b[>0;1;0c"),
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.len() == 2 && params[1] == b"?" {
            match params[0] {
                b"10" => self
                    .replies
                    .extend_from_slice(b"\x1b]10;rgb:eeee/eeee/eeee\x1b\\"),
                b"11" => self
                    .replies
                    .extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
                _ => {}
            }
        }
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title)
            .chars()
            .filter(|ch| !ch.is_control())
            .collect();
        self.events.extend_from_slice(b"\x1b]2;");
        self.events.extend(
            title
                .iter()
                .copied()
                .filter(|byte| *byte >= 32 && *byte != 127),
        );
        self.events.push(7);
    }

    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.events.push(7);
    }
}
