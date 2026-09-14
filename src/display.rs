use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const RESTING_ROWS: u16 = 50;
pub const RESTING_COLS: u16 = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Visible,
    Chat,
    Parked,
}

impl Visibility {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "visible" => Some(Self::Visible),
            "chat" => Some(Self::Chat),
            "parked" => Some(Self::Parked),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Chat => "chat",
            Self::Parked => "parked",
        }
    }
}

struct Client {
    visibility: Visibility,
    rows: u16,
    cols: u16,
    activity: u64,
    explicit_visibility: bool,
    prompt_editor: Option<String>,
}

#[derive(Default)]
pub struct DisplayPolicy {
    clients: BTreeMap<u64, Client>,
    clock: u64,
}

impl DisplayPolicy {
    pub fn attach(&mut self, id: u64, rows: u16, cols: u16, prompt_editor: Option<String>) {
        self.clock += 1;
        self.clients.insert(
            id,
            Client {
                visibility: Visibility::Visible,
                rows,
                cols,
                activity: self.clock,
                explicit_visibility: false,
                prompt_editor,
            },
        );
    }

    pub fn remove(&mut self, id: u64) {
        self.clients.remove(&id);
    }

    pub fn resize(&mut self, id: u64, rows: u16, cols: u16) {
        if let Some(client) = self.clients.get_mut(&id) {
            if !client.explicit_visibility {
                client.rows = rows;
                client.cols = cols;
            }
        }
    }

    pub fn visibility(&mut self, id: u64, visibility: Visibility, rows: u16, cols: u16) {
        if let Some(client) = self.clients.get_mut(&id) {
            client.explicit_visibility = true;
            client.visibility = visibility;
            client.rows = rows;
            client.cols = cols;
            if visibility == Visibility::Visible {
                self.clock += 1;
                client.activity = self.clock;
            }
        }
    }

    pub fn input(&mut self, id: u64) {
        if let Some(client) = self.clients.get_mut(&id) {
            if client.visibility == Visibility::Visible {
                self.clock += 1;
                client.activity = self.clock;
            }
        }
    }

    fn leader(&self) -> Option<(u64, &Client)> {
        self.clients
            .iter()
            .filter(|(_, client)| client.visibility == Visibility::Visible)
            .max_by_key(|(_, client)| client.activity)
            .map(|(id, client)| (*id, client))
    }

    /// CDXC:Zmx 2026-09-14 DECISION:
    /// User: wmx must share the zmx behavior Ghostex uses, and future changes to that contract must cover both providers.
    /// Mirrors zmx's electLeader: visible terminals own the grid; chat may widen it to 200; unattended sessions retain their grid.
    pub fn grid(&self, current: (u16, u16)) -> (u16, u16) {
        if let Some((_, leader)) = self.leader() {
            return (leader.rows, leader.cols);
        }
        if self
            .clients
            .values()
            .any(|client| client.visibility == Visibility::Chat)
        {
            let rows = self
                .clients
                .values()
                .max_by_key(|client| client.activity)
                .map(|client| client.rows)
                .unwrap_or(current.0);
            return (rows, current.1.max(RESTING_COLS));
        }
        current
    }

    pub fn prompt_editor(&self) -> &str {
        self.leader()
            .and_then(|(_, client)| client.prompt_editor.as_deref())
            .unwrap_or("editor")
    }

    pub fn snapshot(&self, rows: u16, cols: u16) -> Value {
        json!({
            "rows": rows, "cols": cols,
            "leader_fd": self.leader().map(|(id, _)| json!(id)).unwrap_or(json!(-1)),
            "resting_rows": RESTING_ROWS, "resting_cols": RESTING_COLS,
            "chat_claim": self.clients.values().any(|client| client.visibility == Visibility::Chat),
            "clients": self.clients.iter().map(|(id, client)| json!({
                "fd": id, "state": client.visibility.name(), "activity": client.activity,
                "last_size": { "rows": client.rows, "cols": client.cols }
            })).collect::<Vec<_>>()
        })
    }
}
