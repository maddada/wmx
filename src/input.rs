#[derive(Debug)]
pub enum Input {
    Bytes(Vec<u8>),
    Visibility {
        state: &'static str,
        rows: u16,
        cols: u16,
    },
    Refresh,
    Detach,
}

#[derive(Default)]
pub struct InputFilter {
    pending: Vec<u8>,
}

impl InputFilter {
    pub fn feed(&mut self, bytes: &[u8], detach_key: bool) -> Vec<Input> {
        const PREFIX: &[u8] = b"\x1b]1337;ZMX_";
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut output = Vec::new();
        while !self.pending.is_empty() {
            if PREFIX.starts_with(&self.pending) {
                break;
            }
            if self.pending.starts_with(PREFIX) {
                if let Some(end) = self.pending.iter().position(|byte| *byte == 7) {
                    if !output.is_empty() {
                        events.push(Input::Bytes(std::mem::take(&mut output)));
                    }
                    let control = String::from_utf8_lossy(&self.pending[PREFIX.len()..end]);
                    if control == "REFRESH" {
                        events.push(Input::Refresh);
                    }
                    for (prefix, state) in [
                        ("VISIBLE=", "visible"),
                        ("CHAT=", "chat"),
                        ("HIDDEN=", "parked"),
                    ] {
                        if let Some((rows, cols)) = control
                            .strip_prefix(prefix)
                            .and_then(|text| text.split_once(','))
                        {
                            if let (Ok(rows), Ok(cols)) = (rows.parse::<u16>(), cols.parse::<u16>())
                            {
                                if rows > 0 && cols > 0 {
                                    events.push(Input::Visibility { state, rows, cols });
                                }
                            }
                        }
                    }
                    self.pending.drain(..=end);
                    continue;
                }
                if self.pending.len() < 128 {
                    break;
                }
                self.pending.clear();
                break;
            }
            let byte = self.pending.remove(0);
            if byte == 0x1c && detach_key {
                if !output.is_empty() {
                    events.push(Input::Bytes(std::mem::take(&mut output)));
                }
                events.push(Input::Detach);
                break;
            }
            output.push(byte);
        }
        if !output.is_empty() {
            events.push(Input::Bytes(output));
        }
        events
    }
    pub fn flush(&mut self) -> Vec<Input> {
        if self.pending.starts_with(b"\x1b]1337;ZMX_") || self.pending.is_empty() {
            return Vec::new();
        }
        vec![Input::Bytes(std::mem::take(&mut self.pending))]
    }
}
