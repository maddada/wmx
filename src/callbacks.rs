#[derive(Default)]
pub(crate) struct TerminalCallbacks {
    pub replies: Vec<u8>,
    pub title: String,
    /// Kitty keyboard flag stacks for the main and alternate screens.
    keyboard: [KeyboardStack; 2],
}

impl TerminalCallbacks {
    /// Kitty keyboard flags the application currently requests. Non-zero means
    /// it decodes CSI-u key sequences from VT input itself.
    pub fn keyboard_flags(&self, screen: &vt100::Screen) -> u8 {
        self.keyboard[usize::from(screen.alternate_screen())].current()
    }
}

/// Mirrors Ghostty's fixed ring (`KeyFlagStack`), the terminal Ghostex
/// attaches with, so both sides agree after any push/pop/set sequence.
#[derive(Default)]
struct KeyboardStack {
    flags: [u8; 8],
    index: usize,
}

impl KeyboardStack {
    fn current(&self) -> u8 {
        self.flags[self.index]
    }

    fn push(&mut self, flags: u8) {
        self.index = (self.index + 1) % self.flags.len();
        self.flags[self.index] = flags;
    }

    fn pop(&mut self, count: usize) {
        if count >= self.flags.len() {
            *self = Self::default();
            return;
        }
        for _ in 0..count {
            self.flags[self.index] = 0;
            self.index = (self.index + self.flags.len() - 1) % self.flags.len();
        }
    }

    fn set(&mut self, flags: u8, mode: u16) {
        let current = &mut self.flags[self.index];
        *current = match mode {
            2 => *current | flags,
            3 => *current & !flags,
            _ => flags,
        };
    }
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
        if command == 'u' {
            let stack = &mut self.keyboard[usize::from(screen.alternate_screen())];
            // Kitty defines five flag bits.
            let flags = (value & 0x1f) as u8;
            match i1 {
                Some(b'>') => stack.push(flags),
                Some(b'<') => stack.pop(usize::from(value.max(1))),
                Some(b'=') => stack.set(
                    flags,
                    params
                        .get(1)
                        .and_then(|values| values.first())
                        .copied()
                        .unwrap_or(1),
                ),
                Some(b'?') => self
                    .replies
                    .extend_from_slice(format!("\x1b[?{}u", stack.current()).as_bytes()),
                _ => {}
            }
            return;
        }
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
    }
}
