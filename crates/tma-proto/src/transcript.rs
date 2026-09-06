//! The transcript window: what the agent in a pane has been writing, bounded.
//!
//! The event vocabulary mirrors `tma_transcript`, whose types carry no serde of their own, so the
//! shapes here are the wire half of that model rather than a second model.
//! `crates/tma/tests/proto_drift.rs` pins the kind labels against the reader's own.

use serde::{Deserialize, Serialize};

/// A stable address for one event, opaque to the device. It is parseable on the host so a
/// hand-edited token earns a `cursor-invalid` refusal rather than a panic; a device only ever
/// echoes one back.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub String);

impl Cursor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Cursor {
    fn from(s: String) -> Cursor {
        Cursor(s)
    }
}

/// The three bounds a window read honours, each a cap on work the caller did not ask for. The
/// device asks; the host clamps. Defaults are `tma_transcript::Budget`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    /// Longest string leaf a header may carry.
    pub header_bytes: u32,
    /// Most bytes one call may read from disk.
    pub read_bytes: u64,
    /// Most header bytes one window may return.
    pub frame_bytes: u32,
}

impl Budget {
    pub const DEFAULT_HEADER_BYTES: u32 = 256;
    pub const DEFAULT_READ_BYTES: u64 = 1024 * 1024;
    pub const DEFAULT_FRAME_BYTES: u32 = 32 * 1024;
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            header_bytes: Budget::DEFAULT_HEADER_BYTES,
            read_bytes: Budget::DEFAULT_READ_BYTES,
            frame_bytes: Budget::DEFAULT_FRAME_BYTES,
        }
    }
}

/// Ask for a page of events, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRequest {
    pub pane: String,
    /// How many events to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u32>,
    /// Page backwards from here. Absent asks for the end of the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Cursor>,
    #[serde(default)]
    pub budget: Budget,
}

/// One page of headers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub pane: String,
    pub agent: String,
    #[serde(default)]
    pub session: Option<SessionMeta>,
    /// Page further backwards from here. `null` once the head of the transcript is in the window.
    #[serde(default)]
    pub older: Option<Cursor>,
    /// True when the byte budget, not the event count, ended the scan.
    #[serde(default)]
    pub budget_truncated: bool,
    /// How many records in this window the reader could not classify. Asserted at zero on the
    /// pinned corpus and non-zero on a drift fixture: the reader must not error on an unseen store
    /// version, and someone must still notice.
    #[serde(default)]
    pub unknown: u32,
    pub events: Vec<EventHeader>,
}

/// Ask for one event's body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRequest {
    pub pane: String,
    pub cursor: Cursor,
}

/// One event. A window read returns these with `body` absent by construction; an event read returns
/// one with it populated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventHeader {
    pub cursor: Cursor,
    #[serde(flatten)]
    pub kind: EventKind,
    /// The store's own timestamp string, verbatim. Not parsed: the stores disagree on format and
    /// the host has no business normalizing a value it cannot verify.
    #[serde(default)]
    pub ts: Option<String>,
    /// The event's first line, capped at the header budget.
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub body: Option<Body>,
}

/// The event vocabulary every store's records map into. `Bookkeeping` is "known and deliberately
/// not drawn", `Unknown` is "a hole to close", and keeping them apart is what lets a drift check
/// assert the unknown counter at zero on a pinned corpus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionMeta(SessionMeta),
    UserMessage {
        bytes: u64,
        attachments: u32,
    },
    AssistantText {
        bytes: u64,
    },
    Thinking {
        bytes: u64,
        redacted: bool,
    },
    ToolCall {
        name: String,
        #[serde(default)]
        call_id: Option<String>,
        arg_keys: Vec<String>,
        bytes: u64,
    },
    ToolResult {
        #[serde(default)]
        call_id: Option<String>,
        status: ResultStatus,
        bytes: u64,
    },
    /// A request the user must answer, where the store writes one as its own record.
    PermissionRequest {
        tool: String,
        #[serde(default)]
        call_id: Option<String>,
    },
    TurnBoundary {
        boundary: TurnKind,
        #[serde(default)]
        reason: Option<String>,
    },
    Usage {
        #[serde(default)]
        input: Option<u64>,
        #[serde(default)]
        output: Option<u64>,
        #[serde(default)]
        total: Option<u64>,
        #[serde(default)]
        context_window: Option<u64>,
        #[serde(default, with = "crate::money")]
        cost_usd: Option<f64>,
    },
    /// A pointer to a nested agent's own transcript.
    SubagentRef {
        child_id: String,
        external_file: bool,
    },
    Compaction {
        compaction: String,
    },
    Attachment {
        attachment: String,
        bytes: u64,
    },
    Bookkeeping {
        type_name: String,
    },
    Unknown {
        type_name: String,
    },
}

/// The header record every store but cursor-agent writes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub agent: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The store's own version stamp, which is what fixtures are pinned on: a session written by an
    /// old build keeps its old shape whatever the installed CLI reports.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

vocabulary! {
    pub enum ResultStatus {
        Ok = "ok",
        Error = "error",
        Pending = "pending",
        Unknown = "unknown",
    }
}

vocabulary! {
    pub enum TurnKind {
        Start = "start",
        End = "end",
    }
}

vocabulary! {
    /// Whether a body is prose or a tool's re-serialized input or output.
    pub enum BodyKind {
        Text = "text",
        Json = "json",
    }
}

/// The full text behind an event, fetched one cursor at a time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    pub kind: BodyKind,
    pub text: String,
}
