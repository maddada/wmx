//! Title timing and semantic normalization follow zmx/src/title_events.zig.
//! Spinner heartbeats keep gxserver's working detector alive without flooding subscribers.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Coalescer {
    latest: Option<(String, String)>,
    emitted: Option<(String, String, Instant)>,
    pending: Option<(Instant, Instant)>,
    heartbeat: bool,
}

impl Coalescer {
    pub fn observe(&mut self, title: &str, now: Instant) {
        let title = trim(title);
        if title.is_empty() || self.latest.as_ref().is_some_and(|(raw, _)| raw == title) {
            return;
        }
        let signature = semantic_signature(title);
        if self
            .latest
            .as_ref()
            .is_some_and(|(_, last)| *last == signature)
            || (self.pending.is_none()
                && self
                    .emitted
                    .as_ref()
                    .is_some_and(|(_, last, _)| *last == signature))
        {
            self.latest = Some((title.into(), signature.clone()));
            if self.pending.is_none()
                && self
                    .emitted
                    .as_ref()
                    .is_some_and(|(raw, last, _)| *last == signature && raw != title)
            {
                self.heartbeat = true;
            }
            return;
        }
        let burst = self.pending.map_or(now, |(burst, _)| burst);
        self.pending = Some((burst, now));
        self.heartbeat = false;
        self.latest = Some((title.into(), signature));
    }

    pub fn last_emitted(&self) -> Option<&str> {
        self.emitted.as_ref().map(|(title, _, _)| title.as_str())
    }

    pub fn take_due(&mut self, now: Instant) -> Option<String> {
        let pending_due = self.pending.is_some_and(|(burst, changed)| {
            now.duration_since(changed) >= Duration::from_secs(1)
                || now.duration_since(burst) >= Duration::from_secs(6)
        });
        let heartbeat_due = self.heartbeat
            && self
                .emitted
                .as_ref()
                .is_some_and(|(_, _, time)| now.duration_since(*time) >= Duration::from_secs(2));
        if !pending_due && !heartbeat_due {
            return None;
        }
        if pending_due {
            self.pending = None;
        } else {
            self.heartbeat = false;
        }
        let (title, signature) = self.latest.as_ref()?;
        if self
            .emitted
            .as_ref()
            .is_some_and(|(raw, last, _)| last == signature && (pending_due || raw == title))
        {
            return None;
        }
        self.emitted = Some((title.clone(), signature.clone(), now));
        Some(title.clone())
    }
}

fn whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r')
}
fn trim(value: &str) -> &str {
    value.trim_matches(whitespace)
}
fn animated(ch: char) -> bool {
    matches!(
        ch,
        '\u{2800}'
            ..='\u{28ff}'
                | '·'
                | '•'
                | '⋅'
                | '◦'
                | '✳'
                | '✶'
                | '✻'
                | '✽'
                | '✸'
                | '✹'
                | '✺'
                | '✷'
                | '✴'
                | '✦'
                | '◇'
                | '🤖'
    )
}
fn without_elapsed(value: &str) -> &str {
    let original = value.trim_end_matches(whitespace);
    let mut rest = original;
    loop {
        let Some(unit) = rest.chars().last() else {
            break;
        };
        if !matches!(unit, 's' | 'm' | 'h' | 'd') {
            break;
        }
        let before_unit = &rest[..rest.len() - 1];
        let digits = before_unit.trim_end_matches(|ch: char| ch.is_ascii_digit());
        if digits.len() == before_unit.len()
            || (!digits.is_empty() && !digits.ends_with(whitespace))
        {
            break;
        }
        rest = digits.trim_end_matches(whitespace);
    }
    if rest.is_empty() {
        original
    } else {
        rest
    }
}
fn semantic_signature(title: &str) -> String {
    if let Some(bracket) = title.strip_prefix('[') {
        let bracket = bracket.trim_start_matches(whitespace);
        if let Some(marker) = bracket
            .chars()
            .next()
            .filter(|ch| animated(*ch) || matches!(ch, '!' | '.'))
        {
            if let Some(rest) = bracket[marker.len_utf8()..]
                .trim_start_matches(whitespace)
                .strip_prefix(']')
            {
                let rest = trim(rest);
                if rest
                    .get(..15)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Action Required"))
                {
                    return format!("[action-required] {}", without_elapsed(rest));
                }
            }
        }
    }
    let stripped = title.trim_start_matches(|ch| whitespace(ch) || ch == '*' || animated(ch));
    let had_animation = stripped.len() != title.len();
    let normalized = stripped
        .split(whitespace)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let dots_removed = normalized
        .trim_end_matches(['.', '·'])
        .trim_end_matches(whitespace);
    let normalized = if dots_removed.len() != normalized.len() && dots_removed.ends_with("Working")
    {
        dots_removed
    } else {
        &normalized
    };
    if had_animation {
        without_elapsed(normalized).into()
    } else {
        normalized.into()
    }
}
