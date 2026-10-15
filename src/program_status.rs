//! Program Status Protocol (OSC 7501): a program reports what it is doing
//! (idle, working, done, blocked, error) and the daemon keeps one record per
//! id with the spec's lifetime rules. Spec:
//! https://www.superlogical.com/rex/docs/build/program-status (revision 0.4).
//!
//! CDXC:SessionStatus 2026-10-14 DECISION:
//! User: "i want this implemented in the app and i want to have a setting to switch to using it instead of all the weird stuff we're doing to track when the agent is working or has attention status". The session daemon parses OSC 7501 and keeps the records because only it sees every byte of a session nobody is looking at, and it answers `OSC 7501 ; ?` itself so programs (Claude Code only reports after that answer) learn the protocol is supported with no client attached. Windows has no process groups, so working and blocked records end at the next OSC 133;A prompt, on `clear`, or when the session ends, not when the reporting program exits.
//! SEE-ALSO: .dependencies/zmx/src/program_status.zig (the zmx twin; keep the rules identical), server/src/program_status/.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use serde_json::{json, Value};

pub(crate) const MAX_SEQUENCE_BYTES: usize = 4096;
const MAX_KEY_BYTES: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_APP_BYTES: usize = 32;
const MAX_ID_BYTES: usize = 128;
const MAX_ID_SEGMENT_BYTES: usize = 32;
const MAX_ID_DEPTH: usize = 8;
pub(crate) const MAX_RECORDS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl State {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    /// working and blocked end at the next prompt.
    fn is_transient(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Terminator {
    Bel,
    St,
}

/// The feature-detection reply. It never carries anything from a report.
pub(crate) fn query_reply(terminator: Terminator) -> &'static [u8] {
    match terminator {
        Terminator::Bel => b"\x1b]7501;?\x07",
        Terminator::St => b"\x1b]7501;?\x1b\\",
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    /// Empty for the root record.
    pub id: String,
    pub state: State,
    pub kind: Option<&'static str>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    pub seq: u64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Report {
    clear: bool,
    state: Option<State>,
    id: String,
    kind: Option<&'static str>,
    progress: Option<u8>,
    app: Option<String>,
    title: Option<String>,
    msg: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Body {
    Query,
    Report(Report),
    Ignored,
}

fn is_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
}

fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return false;
    }
    let segments: Vec<&str> = id.split('/').collect();
    segments.len() <= MAX_ID_DEPTH
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_ID_SEGMENT_BYTES
                && segment.bytes().all(is_id_byte)
        })
}

fn valid_app(app: &str) -> bool {
    !app.is_empty() && app.len() <= MAX_APP_BYTES && app.bytes().all(is_id_byte)
}

fn parse_progress(value: &str) -> Option<u8> {
    if value.is_empty() || value.len() > 3 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u8>().ok().filter(|number| *number <= 100)
}

/// Decode standard base64 (padding optional) and refuse control characters
/// and invalid UTF-8. `None` discards the whole report.
fn decode_text(encoded: &str, limit: usize) -> Option<String> {
    let mut trimmed = encoded;
    for _ in 0..2 {
        trimmed = trimmed.strip_suffix('=').unwrap_or(trimmed);
    }
    if trimmed.contains('=') {
        return None;
    }
    let bytes = STANDARD_NO_PAD.decode(trimmed).ok()?;
    if bytes.len() > limit {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let has_control = text
        .chars()
        .any(|ch| (ch as u32) <= 0x1f || (0x7f..=0x9f).contains(&(ch as u32)));
    (!has_control).then_some(text)
}

/// Parse the body between `OSC 7501 ;` and the terminator. `sequence_len` is
/// the whole sequence (OSC through ST) for the 4096-byte cap.
pub(crate) fn parse_body(body: &[u8], sequence_len: usize) -> Body {
    if body.first() == Some(&b'?') || body.trim_ascii() == b"?" {
        return Body::Query;
    }
    if sequence_len > MAX_SEQUENCE_BYTES {
        return Body::Ignored;
    }
    let Ok(body) = std::str::from_utf8(body) else {
        return Body::Ignored;
    };
    let mut state_text = None;
    let mut id_text = None;
    let mut kind_text = None;
    let mut progress_text = None;
    let mut app_text = None;
    let mut title_text = None;
    let mut msg_text = None;
    for raw_pair in body.split(':') {
        let pair = raw_pair.trim_matches([' ', '\t']);
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim_matches([' ', '\t']);
        let value = value.trim_matches([' ', '\t']);
        if key.is_empty() {
            continue;
        }
        if key.len() > MAX_KEY_BYTES {
            return Body::Ignored;
        }
        if !key.bytes().all(|byte| byte.is_ascii_lowercase()) || !value.bytes().all(is_value_byte) {
            continue;
        }
        // Unknown keys are ignored: that is how the protocol is extended.
        match key {
            "state" => state_text = Some(value),
            "id" => id_text = Some(value),
            "kind" => kind_text = Some(value),
            "progress" => progress_text = Some(value),
            "app" => app_text = Some(value),
            "title" => title_text = Some(value),
            "msg" => msg_text = Some(value),
            _ => {}
        }
    }
    let Some(state_value) = state_text else {
        return Body::Ignored;
    };
    let mut report = Report::default();
    if state_value == "clear" {
        report.clear = true;
    } else {
        match State::parse(state_value) {
            Some(state) => report.state = Some(state),
            None => return Body::Ignored,
        }
    }
    if let Some(id) = id_text {
        if !valid_id(id) {
            return Body::Ignored;
        }
        report.id = id.to_string();
    }
    // Limits are checked on every pair before anything is decoded or stored.
    if app_text.is_some_and(|app| app.len() > MAX_APP_BYTES)
        || title_text.is_some_and(|title| title.len() > MAX_TITLE_ENCODED)
        || msg_text.is_some_and(|msg| msg.len() > MAX_MSG_ENCODED)
    {
        return Body::Ignored;
    }
    if report.clear {
        return Body::Report(report);
    }
    let state = report.state.unwrap_or(State::Idle);
    if state == State::Blocked {
        report.kind = kind_text.and_then(|kind| match kind {
            "permission" => Some("permission"),
            "question" => Some("question"),
            "auth" => Some("auth"),
            _ => None,
        });
    }
    if state.is_transient() {
        report.progress = progress_text.and_then(parse_progress);
    }
    report.app = app_text.filter(|app| valid_app(app)).map(str::to_string);
    if let Some(title) = title_text {
        match decode_text(title, MAX_TITLE_DECODED) {
            Some(text) => report.title = Some(text),
            None => return Body::Ignored,
        }
    }
    if let Some(msg) = msg_text {
        match decode_text(msg, MAX_MSG_DECODED) {
            Some(text) => report.msg = Some(text),
            None => return Body::Ignored,
        }
    }
    Body::Report(report)
}

fn is_self_or_descendant(id: &str, ancestor: &str) -> bool {
    ancestor.is_empty()
        || id == ancestor
        || (id.starts_with(ancestor) && id.as_bytes().get(ancestor.len()) == Some(&b'/'))
}

/// One terminal's records.
#[derive(Default)]
pub(crate) struct Store {
    records: Vec<Record>,
    seq: u64,
    /// Set once any report was applied; gxserver's "Auto" setting trusts a
    /// session's reports from then on.
    ever_reported: bool,
    /// Bumped on every change, so the daemon knows a snapshot is due.
    pub generation: u64,
    /// Input deliveries that submitted something (an Enter). Rides along in
    /// every snapshot; gxserver counts an agent's working → idle as a finished
    /// turn only when the working began after such input (zmx `Store.submits`).
    submits: u64,
}

impl Store {
    /// Count `bytes` (input bound for the program) when it submits something.
    pub(crate) fn note_input(&mut self, bytes: &[u8]) {
        if is_submit(bytes) {
            self.submits += 1;
        }
    }

    pub(crate) fn apply(&mut self, report: Report, now_ms: i64) {
        self.ever_reported = true;
        if report.clear {
            let before = self.records.len();
            self.records
                .retain(|record| !is_self_or_descendant(&record.id, &report.id));
            if self.records.len() != before {
                self.generation += 1;
            }
            return;
        }
        self.seq += 1;
        let record = Record {
            id: report.id,
            state: report.state.unwrap_or(State::Idle),
            kind: report.kind,
            progress: report.progress,
            app: report.app,
            title: report.title,
            msg: report.msg,
            seq: self.seq,
            updated_at_ms: now_ms,
        };
        if let Some(existing) = self
            .records
            .iter_mut()
            .find(|existing| existing.id == record.id)
        {
            *existing = record;
        } else {
            if self.records.len() >= MAX_RECORDS {
                if let Some(oldest) = self
                    .records
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, existing)| existing.seq)
                    .map(|(index, _)| index)
                {
                    self.records.remove(oldest);
                }
            }
            self.records.push(record);
        }
        self.generation += 1;
    }

    /// OSC 133;A: working, blocked and idle records end; done and error stay.
    pub(crate) fn on_prompt_start(&mut self) {
        let before = self.records.len();
        self.records
            .retain(|record| !(record.state.is_transient() || record.state == State::Idle));
        if self.records.len() != before {
            self.generation += 1;
        }
    }

    /// RIS (full reset) removes every record. DECSTR does not.
    pub(crate) fn on_full_reset(&mut self) {
        if !self.records.is_empty() {
            self.records.clear();
            self.generation += 1;
        }
    }

    /// `{"reported":bool,"records":[...]}`, the same shape zmx sends.
    pub(crate) fn to_json(&self) -> Value {
        let records: Vec<Value> = self
            .records
            .iter()
            .map(|record| {
                let mut value = json!({
                    "id": record.id,
                    "state": record.state.as_str(),
                    "seq": record.seq,
                    "updatedAtMs": record.updated_at_ms,
                });
                let object = value.as_object_mut().expect("record is an object");
                if let Some(kind) = record.kind {
                    object.insert("kind".into(), json!(kind));
                }
                if let Some(progress) = record.progress {
                    object.insert("progress".into(), json!(progress));
                }
                for (key, text) in [
                    ("app", &record.app),
                    ("title", &record.title),
                    ("msg", &record.msg),
                ] {
                    if let Some(text) = text {
                        object.insert(key.into(), json!(text));
                    }
                }
                value
            })
            .collect();
        json!({"reported": self.ever_reported, "submits": self.submits, "records": records})
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    /// `None` body when the sequence went over the cap.
    ProgramStatus {
        body: Option<Vec<u8>>,
        sequence_len: usize,
        terminator: Terminator,
    },
    PromptStart,
    FullReset,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Mode {
    #[default]
    Ground,
    Esc,
    OscNumber,
    Osc7501,
    Osc7501Esc,
    Osc133,
    Osc133Esc,
    OscSkip,
    OscSkipEsc,
    StringSkip,
    StringSkipEsc,
}

/// Streaming scanner over pty output: finds `OSC 7501`, `OSC 133;A` and
/// `ESC c` across read boundaries, buffering only 7501 bodies.
#[derive(Default)]
pub(crate) struct Scanner {
    mode: Mode,
    number: u32,
    digits: u8,
    body: Vec<u8>,
    overflow: bool,
    semantic: Vec<u8>,
}

impl Scanner {
    pub(crate) fn feed(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        for &byte in bytes {
            self.step(byte, events);
        }
    }

    fn finish(&mut self, terminator: Terminator, events: &mut Vec<Event>) {
        match self.mode {
            Mode::Osc7501 | Mode::Osc7501Esc => {
                let sequence_len =
                    2 + 4 + 1 + self.body.len() + if terminator == Terminator::Bel { 1 } else { 2 };
                events.push(Event::ProgramStatus {
                    body: (!self.overflow).then(|| std::mem::take(&mut self.body)),
                    sequence_len: if self.overflow {
                        MAX_SEQUENCE_BYTES + 1
                    } else {
                        sequence_len
                    },
                    terminator,
                });
            }
            Mode::Osc133 | Mode::Osc133Esc => {
                if self.semantic.first() == Some(&b'A')
                    && (self.semantic.len() == 1 || self.semantic.get(1) == Some(&b';'))
                {
                    events.push(Event::PromptStart);
                }
            }
            _ => {}
        }
        self.body.clear();
        self.mode = Mode::Ground;
    }

    fn step(&mut self, byte: u8, events: &mut Vec<Event>) {
        match self.mode {
            Mode::Ground => {
                if byte == 0x1b {
                    self.mode = Mode::Esc;
                }
            }
            Mode::Esc => {
                self.mode = match byte {
                    b']' => {
                        self.number = 0;
                        self.digits = 0;
                        Mode::OscNumber
                    }
                    b'c' => {
                        events.push(Event::FullReset);
                        Mode::Ground
                    }
                    b'P' | b'X' | b'^' | b'_' => Mode::StringSkip,
                    0x1b => Mode::Esc,
                    _ => Mode::Ground,
                }
            }
            Mode::OscNumber => match byte {
                b'0'..=b'9' if self.digits < 6 => {
                    self.number = self.number * 10 + u32::from(byte - b'0');
                    self.digits += 1;
                }
                b';' if self.digits == 4 && self.number == 7501 => {
                    self.body.clear();
                    self.overflow = false;
                    self.mode = Mode::Osc7501;
                }
                b';' if self.digits == 3 && self.number == 133 => {
                    self.semantic.clear();
                    self.mode = Mode::Osc133;
                }
                0x07 | 0x18 | 0x1a => self.mode = Mode::Ground,
                0x1b => self.mode = Mode::OscSkipEsc,
                _ => self.mode = Mode::OscSkip,
            },
            Mode::Osc7501 => match byte {
                0x07 => self.finish(Terminator::Bel, events),
                0x1b => self.mode = Mode::Osc7501Esc,
                0x18 | 0x1a => self.mode = Mode::Ground,
                _ => {
                    if self.body.len() < MAX_SEQUENCE_BYTES {
                        self.body.push(byte);
                    } else {
                        self.overflow = true;
                    }
                }
            },
            Mode::Osc133 => match byte {
                0x07 => self.finish(Terminator::Bel, events),
                0x1b => self.mode = Mode::Osc133Esc,
                0x18 | 0x1a => self.mode = Mode::Ground,
                _ => {
                    if self.semantic.len() < 2 {
                        self.semantic.push(byte);
                    }
                }
            },
            Mode::Osc7501Esc | Mode::Osc133Esc => {
                if byte == b'\\' {
                    self.finish(Terminator::St, events);
                } else {
                    // ESC that is not ST aborts the string and starts a new escape.
                    self.mode = Mode::Esc;
                    self.step(byte, events);
                }
            }
            Mode::OscSkip => match byte {
                0x07 | 0x18 | 0x1a => self.mode = Mode::Ground,
                0x1b => self.mode = Mode::OscSkipEsc,
                _ => {}
            },
            Mode::OscSkipEsc | Mode::StringSkipEsc => {
                if byte == b'\\' {
                    self.mode = Mode::Ground;
                } else {
                    self.mode = Mode::Esc;
                    self.step(byte, events);
                }
            }
            Mode::StringSkip => match byte {
                0x18 | 0x1a => self.mode = Mode::Ground,
                0x1b => self.mode = Mode::StringSkipEsc,
                _ => {}
            },
        }
    }
}

/// Enter as a terminal sends it: CR or LF, or the kitty "report all keys"
/// forms of Enter and keypad Enter.
pub(crate) fn is_submit(bytes: &[u8]) -> bool {
    let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
        || contains(b"\x1b[13u")
        || contains(b"\x1b[13;")
        || contains(b"\x1b[57414")
}

/// Remove complete `OSC 7501 ; ? <ST|BEL>` queries from output bound for
/// attached clients: the daemon answers them itself, and a client terminal
/// that also answered would hand the program a second reply.
pub(crate) fn strip_queries(data: &[u8]) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"\x1b]7501;?";
    if !data.windows(PREFIX.len()).any(|window| window == PREFIX) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut removed = false;
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(PREFIX) {
            let after = i + PREFIX.len();
            if data.get(after) == Some(&0x07) {
                i = after + 1;
                removed = true;
                continue;
            }
            if data.get(after) == Some(&0x1b) && data.get(after + 1) == Some(&b'\\') {
                i = after + 2;
                removed = true;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    removed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(store: &mut Store, chunks: &[&[u8]]) -> (usize, usize) {
        let mut scanner = Scanner::default();
        let mut queries = 0;
        let mut ignored = 0;
        for chunk in chunks {
            let mut events = Vec::new();
            scanner.feed(chunk, &mut events);
            for event in events {
                match event {
                    Event::ProgramStatus { body: None, .. } => ignored += 1,
                    Event::ProgramStatus {
                        body: Some(body),
                        sequence_len,
                        ..
                    } => match parse_body(&body, sequence_len) {
                        Body::Query => queries += 1,
                        Body::Ignored => ignored += 1,
                        Body::Report(report) => store.apply(report, 1000),
                    },
                    Event::PromptStart => store.on_prompt_start(),
                    Event::FullReset => store.on_full_reset(),
                }
            }
        }
        (queries, ignored)
    }

    fn record<'a>(store: &'a Store, id: &str) -> Option<&'a Record> {
        store.records.iter().find(|record| record.id == id)
    }

    #[test]
    fn report_replaces_record_and_splits_across_reads() {
        let mut store = Store::default();
        run(
            &mut store,
            &[
                b"\x1b]75",
                b"01;state=working:app=brew:progress=40:msg=SW5zdGFsbGluZyB1cGRhdGVz\x1b",
                b"\\",
            ],
        );
        let root = record(&store, "").unwrap();
        assert_eq!(root.state, State::Working);
        assert_eq!(root.progress, Some(40));
        assert_eq!(root.msg.as_deref(), Some("Installing updates"));
        run(&mut store, &[b"\x1b]7501;state=done\x07"]);
        let root = record(&store, "").unwrap();
        assert_eq!(root.state, State::Done);
        assert!(root.app.is_none() && root.msg.is_none());
    }

    #[test]
    fn invalid_reports_are_discarded() {
        let mut store = Store::default();
        let big = format!("\x1b]7501;state=working:msg={}\x07", "A".repeat(5000));
        let (_, ignored) = run(
            &mut store,
            &[
                b"\x1b]7501;state=sleeping\x07",
                b"\x1b]7501;state=working:msg=YQpi\x07",
                b"\x1b]7501;state=working:id=a//b\x07",
                b"\x1b]7501;state=working:title=woU=\x07",
                big.as_bytes(),
            ],
        );
        assert_eq!(ignored, 5);
        assert!(store.records.is_empty());
    }

    #[test]
    fn lifetimes() {
        let mut store = Store::default();
        run(
            &mut store,
            &[
                b"\x1b]7501;state=working:id=build\x07",
                b"\x1b]7501;state=blocked:kind=auth:id=build/test\x07",
                b"\x1b]7501;state=done:id=d\x07",
                b"\x1b]7501;state=error:id=e\x07",
                b"\x1b]7501;state=clear:id=build\x07",
            ],
        );
        assert!(record(&store, "build/test").is_none());
        run(
            &mut store,
            &[b"\x1b]7501;state=working\x07", b"\x1b]133;A\x07"],
        );
        assert_eq!(store.records.len(), 2);
        run(&mut store, &[b"\x1bc"]);
        assert!(store.records.is_empty());
    }

    #[test]
    fn query_and_strip() {
        let mut store = Store::default();
        let (queries, _) = run(&mut store, &[b"\x1b]7501;?\x1b\\"]);
        assert_eq!(queries, 1);
        assert_eq!(
            strip_queries(b"a\x1b]7501;?\x07b\x1b]7501;state=idle\x07").unwrap(),
            b"ab\x1b]7501;state=idle\x07"
        );
    }

    #[test]
    fn json_shape_matches_zmx() {
        let mut store = Store::default();
        run(
            &mut store,
            &[b"\x1b]7501;state=blocked:kind=question:app=claude-code:msg=c2F5ICJoaSIgXA==\x07"],
        );
        assert_eq!(
            store.to_json().to_string(),
            r#"{"records":[{"app":"claude-code","id":"","kind":"question","msg":"say \"hi\" \\","seq":1,"state":"blocked","updatedAtMs":1000}],"reported":true,"submits":0}"#
        );
    }
}
