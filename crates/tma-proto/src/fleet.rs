//! The fleet: the rows a device lists, the transitions it streams, and the scoping both accept.

use serde::{Deserialize, Serialize};

use crate::state::{Detail, State, StateFilter};

/// One agent pane as it leaves the machine.
///
/// The key set is `tma_ui::surfaces::RowSurface::Protocol` exactly, and
/// `crates/tma/tests/proto_drift.rs` asserts that against the host writer rather than trusting this
/// comment. There is **no `title` field**: a pane title is agent-supplied text, and R2 is a property
/// of the struct rather than of a redaction pass someone can forget to run.
///
/// Absent values are an explicit `null`, matching the host writer key for key. `#[serde(default)]`
/// is the reader's half of the same rule: a frame from a writer that predates a key parses, with
/// that key's default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FleetRow {
    pub pane: String,
    pub agent: String,
    pub state: State,
    #[serde(default)]
    pub detail: Option<Detail>,
    /// `@agent_since` in epoch seconds, and the same instant in ms. Both keys ride the host row.
    #[serde(default)]
    pub since: u64,
    #[serde(default)]
    pub since_ms: u64,
    /// `max(@agent_since, @agent_turn_at)`, absolute epoch ms. This is the value
    /// [`crate::Binder::expect_episode_ms`] quotes, and it is deliberately not an age: an age cannot
    /// be compared for equality against a stamp read later.
    #[serde(default)]
    pub episode_ms: u64,
    pub locator: String,
    #[serde(default)]
    pub attention: bool,
    /// Idle with the attention flag still up: finished, output unreviewed.
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub transcript: Option<String>,
    /// The permission decision the pane is waiting on. A reply quotes it, but a match is necessary
    /// and never sufficient: the server's 404 is the authority.
    #[serde(default)]
    pub permission_request: Option<String>,
    /// When the host last stamped the tuple: the row's own freshness anchor.
    #[serde(default)]
    pub stamped_at_ms: Option<u64>,
    #[serde(default)]
    pub context: Option<u8>,
    #[serde(default)]
    pub context_at_ms: Option<u64>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub quota: Option<Quota>,
    #[serde(default, with = "crate::money")]
    pub cost_usd: Option<f64>,
    /// `repo`, `branch` and `worktree` are null together: the pane's cwd never resolved to a repo.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub worktree: Option<bool>,
    #[serde(default)]
    pub pending_tool: Option<String>,
    #[serde(default)]
    pub pending_call: Option<String>,
    /// Agent-supplied text: one line describing the pending call. It rides this row and the pane
    /// options only, never a notification payload.
    #[serde(default)]
    pub pending_summary: Option<String>,
    /// Where the row was observed. Two hosts' rows collide on `%5` without these.
    pub server: String,
    pub host: String,
}

/// The account quota annotation. Account-wide rather than per-pane, so several rows carrying the
/// same numbers is correct and summing them means nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub pct: u8,
    /// The window the percent is of. A percent with no window token cannot be read.
    pub window: String,
    #[serde(default)]
    pub resets_at_ms: Option<u64>,
}

/// The whole fleet in scope, one frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub agents: Vec<FleetRow>,
}

/// One row's state transition, as the streaming cycle observed it.
///
/// `from` and `to` are the empty string at the open ends of a pane's life: `""` to a state for a
/// pane that appeared, a state to `""` for one that vanished. `""` is deliberately not `unknown`,
/// which is a state a pane can genuinely be observed in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// When the stream **observed** the transition, not necessarily when the agent changed.
    pub at_ms: u64,
    pub pane: String,
    pub agent: String,
    #[serde(with = "open_ended")]
    pub from: Option<State>,
    #[serde(with = "open_ended")]
    pub to: Option<State>,
    #[serde(default)]
    pub detail: Option<Detail>,
    pub locator: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

/// `""` for the open end of a pane's life, the host's own encoding.
mod open_ended {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::state::State;

    pub(super) fn serialize<S: Serializer>(v: &Option<State>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.map_or("", State::token))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<State>, D::Error> {
        let token = String::deserialize(d)?;
        if token.is_empty() {
            return Ok(None);
        }
        State::from_token(&token)
            .map(Some)
            .ok_or_else(|| serde::de::Error::unknown_variant(&token, State::TOKENS))
    }
}

/// Which panes a request is about. An empty selector is the whole fleet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StateFilter>,
}

/// Ask for the fleet once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
}

/// Ask to keep receiving it. `events` picks transitions over whole snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    #[serde(default)]
    pub events: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
}
