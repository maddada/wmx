use std::{fmt::Write, ops::Range, sync::OnceLock};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// CDXC:PlatformSupport 2026-10-06 WHY:
/// Importing user32 statically made every wmx process connect to the desktop while loading. When Windows shuts down, gxserver respawns each session's `watch-title` after the old one exits; user32 could no longer initialise against the closing desktop, so each new wmx.exe failed with 0xc0000142 and Windows showed one "unable to start correctly" dialog per live session. Only CSI-u key translation in `attach`/`send` needs user32, so load it there, and keep wmx's import table free of user32 and gdi32.
fn vk_key_scan(unit: u16) -> i16 {
    type VkKeyScanW = unsafe extern "system" fn(u16) -> i16;
    static FUNCTION: OnceLock<Option<VkKeyScanW>> = OnceLock::new();
    let function = FUNCTION.get_or_init(|| unsafe {
        let library: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        let module = LoadLibraryExW(
            library.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, c"VkKeyScanW".as_ptr().cast())
            .map(|address| std::mem::transmute::<_, VkKeyScanW>(address))
    });
    function.map_or(-1, |function| unsafe { function(unit) })
}

/// Translate character keys at the ConPTY boundary, before the existing IPC.
/// ConPTY does not decode CSI-u and its layout synthesis can lose Unicode text.
/// Client-side translation also fixes already-running generation-1 daemons.
/// POSIX zmx deliberately forwards these bytes: its programs decode VT directly.
#[derive(Default)]
pub struct ConsoleInput {
    pending: Vec<u8>,
    pasted: bool,
    kitty_keys: bool,
}

impl ConsoleInput {
    /// CDXC:Terminal 2026-10-04 WHY:
    /// CSI-u keys become Win32 records only for applications that read console records (Codex, PSReadLine). An application that pushed kitty keyboard flags (Claude Code) reads VT input, where ConPTY re-encodes a Shift+Enter record as a bare CR: Shift+Enter submitted the prompt instead of inserting a newline. Forward CSI-u verbatim to those applications, as POSIX zmx does; ConPTY passes the unknown sequence through to them unchanged.
    pub fn set_kitty_keys(&mut self, flags: u8) {
        self.kitty_keys = flags != 0;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut output = Reads::default();
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
                if !self.pasted && !self.kitty_keys {
                    if let Some(key) = csi_u(sequence) {
                        key.write(&mut output);
                        offset += length;
                        continue;
                    }
                }
                output.push(sequence);
                offset += length;
            } else if remaining[0].is_ascii() {
                output.push(&remaining[..1]);
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
                if let Some(text) = std::str::from_utf8(&remaining[..length])
                    .ok()
                    .filter(|text| !self.kitty_keys && fits_record(text))
                {
                    for unit in text.encode_utf16() {
                        output.press(0, unit, 0);
                    }
                } else {
                    output.push(&remaining[..length]);
                }
                offset += length;
            }
        }
        self.pending.drain(..offset);
        let mut output = output.finish();
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
        let mut output = Reads::default();
        if self.pending == b"\x1b" {
            self.pending.clear();
            output.press(27, 27, 0);
        } else {
            output.push(&std::mem::take(&mut self.pending));
        }
        output.finish()
    }
}

/// How many bytes the inbox conhost of Windows 11 22621 reads from its ConPTY input pipe at a time.
const READ: usize = 256;
/// `ESC [ _`, the shortest no-op: a key-up record whose fields are all zero, which console readers skip. Leading zeros lengthen it.
const NOOP: usize = 3;

/// The bytes of one write to the ConPTY input pipe, laid out in conhost's reads.
///
/// CDXC:Terminal 2026-10-25 WHY:
/// The inbox conhost of Windows 11 22621 reads its input pipe 256 bytes at a time and treats each read as whole keystrokes: a sequence still open at the end of a read is dispatched as it stands, and the rest of it is typed as text. A record reader (Codex, PSReadLine) got `;1;0;1_` or a stray `_` in place of the character whose record a read cut. So no character or sequence crosses a multiple of 256: one that would is preceded by a no-op that fills the read, and every write ends on a read boundary, so the next write starts on one even when conhost has not finished reading this one. One or two bytes are too few for a no-op, so then what fills the read's last three bytes moves into the next read behind a longer one.
#[derive(Default)]
struct Reads {
    bytes: Vec<u8>,
    /// The latest unit longer than one byte.
    last: Range<usize>,
}

impl Reads {
    /// Appends a unit conhost must read whole: a byte, a character or a sequence.
    fn push(&mut self, unit: &[u8]) {
        self.make_room(unit.len());
        let start = self.bytes.len();
        self.bytes.extend_from_slice(unit);
        if unit.len() > 1 {
            self.last = start..self.bytes.len();
        }
    }

    /// A key press as a keyboard makes it: down, then up.
    ///
    /// CDXC:Terminal 2026-10-25 WHY:
    /// conhost merges a key-down into an identical key-down still waiting in its input buffer by raising the repeat count, and crossterm ignores repeat counts, so `ééé` reached Codex as `é`. The key-up in between keeps every press its own record, as for the ASCII text conhost types itself. A surrogate half gets no key-up: crossterm takes a surrogate from any key record as the next half of a pair, so the key-up would break the pair.
    fn press(&mut self, vk: u16, unicode: u16, modifiers: u32) {
        self.record(vk, unicode, modifiers, true);
        if !(0xd800..=0xdfff).contains(&unicode) {
            self.record(vk, unicode, modifiers, false);
        }
    }

    fn record(&mut self, vk: u16, unicode: u16, modifiers: u32, down: bool) {
        let mut record = String::new();
        let _ = write!(
            record,
            "\x1b[{vk};0;{unicode};{};{modifiers};1_",
            u8::from(down)
        );
        self.push(record.as_bytes());
    }

    /// Pads to the end of the current read when `length` more bytes would cross it.
    fn make_room(&mut self, length: usize) {
        let mut offset = self.bytes.len() % READ;
        if offset + length <= READ || length > READ {
            return;
        }
        if READ - offset < NOOP {
            let read = self.bytes.len() - offset;
            let mut start = read + READ - NOOP;
            if self.last.contains(&start) {
                start = self.last.start;
            }
            // A single unit fills the read, so nothing can move.
            if start == read {
                return;
            }
            let moved = self.bytes.split_off(start);
            let padding = READ - (start - read);
            self.noop(padding);
            if self.last.start >= start {
                self.last = self.last.start + padding..self.last.end + padding;
            }
            self.bytes.extend(moved);
            offset = self.bytes.len() % READ;
            if offset + length <= READ || READ - offset < NOOP {
                return;
            }
        }
        self.noop(READ - offset);
    }

    fn noop(&mut self, length: usize) {
        self.bytes.extend_from_slice(b"\x1b[");
        self.bytes.resize(self.bytes.len() + length - NOOP, b'0');
        self.bytes.push(b'_');
    }

    /// Ends the write on a read boundary.
    fn finish(mut self) -> Vec<u8> {
        if !self.bytes.is_empty() {
            self.make_room(READ);
        }
        self.bytes
    }
}

/// Whether `text` can travel as Win32 input records: only an application that reads console records needs them, and only for units the record parameter can carry.
///
/// CDXC:Terminal 2026-10-25 WHY:
/// The inbox conhost of Windows 11 22621 clamps every record parameter at 32767, so a unit above 0x7FFF (both emoji surrogate halves, the U+FE0F variation selector, every Hangul syllable) reached the application as U+7FFF ("翿"); such characters go as UTF-8, which ConPTY decodes itself, also across its 256-byte reads (`Reads`). An application that pushed kitty keyboard flags (Claude Code, which reads VT input) gets all its text as UTF-8 and kept every character at every offset. A record reader (Codex, PSReadLine) keeps records for the rest: as UTF-8, ConPTY types a character outside the code page (✅, ⚠, ❓) as Alt+Numpad and hands it over on the Alt key release, which crossterm reports as a release that Codex ignores.
fn fits_record(text: &str) -> bool {
    text.encode_utf16().all(|unit| unit <= 0x7fff)
}

/// A CSI-u key as Win32 input records.
struct Key {
    vk: u16,
    text: char,
    state: u32,
    /// kitty's event type: 1 press, 2 repeat, 3 release.
    event: u8,
}

impl Key {
    fn write(&self, output: &mut Reads) {
        for unit in self.text.encode_utf16(&mut [0; 2]) {
            match self.event {
                1 => output.press(self.vk, *unit, self.state),
                event => output.record(self.vk, *unit, self.state, event == 2),
            }
        }
    }
}

fn csi_u(sequence: &[u8]) -> Option<Key> {
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
            let mapped = vk_key_scan(code as u16);
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
    Some(Key {
        vk,
        text,
        state,
        event,
    })
}
