use std::fmt::Write;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;

/// Translate character keys at the ConPTY boundary, before the existing IPC.
/// ConPTY does not decode CSI-u and its layout synthesis can lose Unicode text.
/// Client-side translation also fixes already-running generation-1 daemons.
/// POSIX zmx deliberately forwards these bytes: its programs decode VT directly.
#[derive(Default)]
pub struct ConsoleInput {
    pending: Vec<u8>,
    pasted: bool,
}

impl ConsoleInput {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            let remaining = &self.pending[offset..];
            if remaining[0] == 0x1b {
                if remaining.len() == 1 {
                    break;
                }
                let length = match remaining[1] {
                    b'[' => remaining[2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map(|n| n + 3),
                    b']' | b'P' | b'_' | b'^' => remaining[2..]
                        .iter()
                        .position(|b| *b == 7)
                        .map(|n| n + 3)
                        .or_else(|| {
                            remaining
                                .windows(2)
                                .position(|pair| pair == b"\x1b\\")
                                .map(|n| n + 2)
                        }),
                    b'O' => (remaining.len() >= 3).then_some(3),
                    _ => Some(1),
                };
                let Some(length) = length else { break };
                let sequence = &remaining[..length];
                if sequence == b"\x1b[200~" {
                    self.pasted = true;
                } else if sequence == b"\x1b[201~" {
                    self.pasted = false;
                }
                if !self.pasted {
                    if let Some(key) = csi_u(sequence) {
                        output.extend_from_slice(key.as_bytes());
                        offset += length;
                        continue;
                    }
                }
                output.extend_from_slice(sequence);
                offset += length;
            } else if remaining[0].is_ascii() {
                output.push(remaining[0]);
                offset += 1;
            } else {
                let length = match remaining[0] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 1,
                };
                if remaining.len() < length {
                    break;
                }
                if let Ok(text) = std::str::from_utf8(&remaining[..length]) {
                    let mut encoded = String::new();
                    for unit in text.encode_utf16() {
                        record(&mut encoded, 0, unit, 0, true);
                    }
                    output.extend_from_slice(encoded.as_bytes());
                } else {
                    output.extend_from_slice(&remaining[..length]);
                }
                offset += length;
            }
        }
        self.pending.drain(..offset);
        // Bound malformed/unterminated control strings without losing their bytes.
        if self.pending.len() > 64 * 1024 {
            output.extend(self.finish());
        }
        output
    }

    pub fn idle(&mut self) -> Vec<u8> {
        // Escape must work on its own; keep split UTF-8 and CSI sequences intact.
        if self.pending == b"\x1b" {
            self.finish()
        } else {
            Vec::new()
        }
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if self.pending == b"\x1b" {
            self.pending.clear();
            let mut encoded = String::new();
            record(&mut encoded, 27, 27, 0, true);
            return encoded.into_bytes();
        }
        std::mem::take(&mut self.pending)
    }
}

fn record(output: &mut String, vk: u16, unicode: u16, modifiers: u32, down: bool) {
    let _ = write!(
        output,
        "\x1b[{vk};0;{unicode};{};{modifiers};1_",
        u8::from(down)
    );
}

fn csi_u(sequence: &[u8]) -> Option<String> {
    let body = std::str::from_utf8(sequence)
        .ok()?
        .strip_prefix("\x1b[")?
        .strip_suffix('u')?;
    let mut fields = body.split(';');
    let mut codes = fields.next()?.split(':');
    let code = codes.next()?.parse::<u32>().ok()?;
    if (57344..=63743).contains(&code) {
        return None;
    }
    let shifted = codes.next().and_then(|s| s.parse::<u32>().ok());
    let mut modifiers = fields.next().unwrap_or("1").split(':');
    let mask = modifiers.next()?.parse::<u32>().ok()?.checked_sub(1)?;
    if mask & !0xc7 != 0 {
        return None;
    }
    let event = modifiers.next().unwrap_or("1").parse::<u8>().ok()?;
    if !(1..=3).contains(&event) {
        return None;
    }
    // Win32 control-state bits differ from kitty's modifier bits.
    let state = (if mask & 1 != 0 { 16 } else { 0 })
        | (if mask & 2 != 0 { 2 } else { 0 })
        | (if mask & 4 != 0 { 8 } else { 0 })
        | (if mask & 64 != 0 { 128 } else { 0 })
        | (if mask & 128 != 0 { 32 } else { 0 });
    let key = char::from_u32(code)?;
    let vk = match code {
        9 | 13 | 27 => code as u16,
        127 => 8,
        _ if code <= u16::MAX as u32 => {
            let mapped = unsafe { VkKeyScanW(code as u16) };
            if mapped == -1 {
                0
            } else {
                mapped as u16 & 0xff
            }
        }
        _ => 0,
    };
    let mut text = if mask & 1 != 0 {
        shifted
            .and_then(char::from_u32)
            .unwrap_or_else(|| key.to_ascii_uppercase())
    } else {
        key
    };
    if code == 127 {
        text = '\x08';
    }
    if mask & 4 != 0 {
        text = match code {
            32 | 64 => '\0',
            65..=95 | 97..=122 => char::from_u32(code & 0x1f)?,
            _ => text,
        };
    }
    let mut output = String::new();
    for unit in text.encode_utf16(&mut [0; 2]) {
        record(&mut output, vk, *unit, state, event != 3);
    }
    Some(output)
}
