//! The normalized event model every adapter targets, and the cursor that addresses one event.
//!
//! The split that matters is [`Event::body`]. A header carries what a list needs (kind, time, tool
//! name, sizes) and nothing else; the body carries the text. A window read returns headers with
//! every string leaf capped at [`Budget::header_bytes`] and no bodies at all, so serving 200 events
//! costs kilobytes rather than megabytes, and a reader that wants one message asks for that one
//! cursor's body.

use std::fmt;
use std::str::FromStr;

/// Which store a source belongs to. The variant set is the agent set tma detects, including the
/// one the reader refuses, because a refusal has to name which store it is refusing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Store {
    Claude,
    Codex,
    Gemini,
    Pi,
    /// Served as a typed refusal: cursor-agent's transcript has no tool results, no timestamps and
    /// no version stamp, so a window over it renders as holes. See [`crate::Refusal`].
    Cursor,
    /// SQLite rather than a file, and served only when the `opencode` feature is compiled in.
    OpenCode,
}

impl Store {
    /// The agent name tma stamps in `@agent_name`.
    pub fn as_str(self) -> &'static str {
        match self {
            Store::Claude => "claude",
            Store::Codex => "codex",
            Store::Gemini => "gemini",
            Store::Pi => "pi",
            Store::Cursor => "cursor",
            Store::OpenCode => "opencode",
        }
    }

    /// The store behind an `@agent_name`, or `None` for an agent with no known store.
    pub fn from_agent(name: &str) -> Option<Store> {
        match name {
            "claude" => Some(Store::Claude),
            "codex" => Some(Store::Codex),
            "gemini" => Some(Store::Gemini),
            "pi" => Some(Store::Pi),
            "cursor" => Some(Store::Cursor),
            "opencode" => Some(Store::OpenCode),
            _ => None,
        }
    }

    /// Whether the reader can serve this store's records today. OpenCode's answer is a build fact:
    /// its reader is the `opencode` feature, and a build without it refuses rather than half-serves.
    pub fn is_readable(self) -> bool {
        match self {
            Store::Claude | Store::Codex | Store::Gemini | Store::Pi => true,
            Store::OpenCode => cfg!(feature = "opencode"),
            Store::Cursor => false,
        }
    }

    /// Whether this store's records are line-oriented JSON in a file. False for OpenCode alone,
    /// which is a database and takes the other reader.
    pub(crate) fn is_file_store(self) -> bool {
        self != Store::OpenCode
    }
}

impl fmt::Display for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity half of a cursor: what the file was when the cursor was minted.
///
/// `(dev, ino)` catches a rotation or a replace-by-rename, and `size` catches a truncation. The
/// size check is one-sided on purpose: a live transcript grows on every append, so a cursor stays
/// valid while the file only gets longer and is refused the moment it gets shorter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// A stable address for one event: which file, where in it, and which event of that record.
///
/// One JSONL line can yield several events (an assistant message with two content blocks), so the
/// offset alone does not address an event; `part` indexes within the record. `offset` is the START
/// of the record's line, which is what makes `before` paging exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cursor {
    pub file: FileId,
    /// File size when the cursor was minted. A later size below this means the file was rewritten.
    pub size: u64,
    /// Byte offset of the first byte of the record's line.
    pub offset: u64,
    /// Index of this event within the record's events, 0-based.
    pub part: u32,
}

impl Cursor {
    /// Order two cursors within the same file: record position first, then part.
    pub fn position(&self) -> (u64, u32) {
        (self.offset, self.part)
    }
}

/// The wire form: opaque to callers, five fixed-width hex fields behind a version tag. Opaque
/// means callers must not build one; it is parseable so a refusal can be `cursor-invalid` rather
/// than a panic on a hand-edited token.
impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "t1.{:x}.{:x}.{:x}.{:x}.{:x}",
            self.file.dev, self.file.ino, self.size, self.offset, self.part
        )
    }
}

/// A cursor string that is not a cursor. The caller turns this into [`crate::Refusal::CursorInvalid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorParseError;

impl fmt::Display for CursorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a transcript cursor")
    }
}

impl std::error::Error for CursorParseError {}

impl FromStr for Cursor {
    type Err = CursorParseError;

    fn from_str(s: &str) -> Result<Cursor, CursorParseError> {
        let mut parts = s.split('.');
        if parts.next() != Some("t1") {
            return Err(CursorParseError);
        }
        let mut hex = || {
            parts
                .next()
                .and_then(|f| u64::from_str_radix(f, 16).ok())
                .ok_or(CursorParseError)
        };
        let (dev, ino, size, offset) = (hex()?, hex()?, hex()?, hex()?);
        let part = u32::try_from(hex()?).map_err(|_| CursorParseError)?;
        if parts.next().is_some() {
            return Err(CursorParseError);
        }
        Ok(Cursor {
            file: FileId { dev, ino },
            size,
            offset,
            part,
        })
    }
}

/// One normalized event: where it is, when the store says it happened, what it is, and (only on a
/// body read) its text.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub cursor: Cursor,
    /// The store's own timestamp string, verbatim, when it wrote one. Not parsed: the stores
    /// disagree on format and the reader has no business normalizing a value it cannot verify.
    pub ts: Option<String>,
    pub kind: EventKind,
    /// The event's first line, capped at the header budget. Present on both header and body reads
    /// so a list can render without a second call.
    pub preview: Option<String>,
    /// The full text or JSON behind the event. `None` on a window read, by construction.
    pub body: Option<Body>,
}

/// What an event's body holds. `Json` is a tool's input or output re-serialized compactly; `Text`
/// is prose the model or the user wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Text(String),
    Json(String),
}

impl Body {
    pub fn as_str(&self) -> &str {
        match self {
            Body::Text(s) | Body::Json(s) => s,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Body::Text(_) => "text",
            Body::Json(_) => "json",
        }
    }
}

/// The event vocabulary, ported from the transcript spike's model. Every store's records map into
/// this set; `Bookkeeping` is "known and deliberately not drawn", `Unknown` is "a hole to close",
/// and keeping them apart is what lets a drift check assert `unknown == 0` on a pinned corpus.
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    SessionMeta(SessionMeta),
    UserMessage {
        bytes: usize,
        attachments: usize,
    },
    AssistantText {
        bytes: usize,
    },
    Thinking {
        bytes: usize,
        redacted: bool,
    },
    ToolCall {
        name: String,
        call_id: Option<String>,
        arg_keys: Vec<String>,
        bytes: usize,
    },
    ToolResult {
        call_id: Option<String>,
        status: ResultStatus,
        bytes: usize,
    },
    /// A request the user must answer. No file store writes one as its own record; the variant
    /// exists because a store that does write one (codex's approval events) has somewhere to land.
    PermissionRequest {
        tool: String,
        call_id: Option<String>,
    },
    TurnBoundary {
        kind: TurnKind,
        reason: Option<String>,
    },
    Usage {
        input: Option<u64>,
        output: Option<u64>,
        total: Option<u64>,
        context_window: Option<u64>,
        /// Total cost of the turn in USD, when the store writes one. pi is the only store that does.
        cost_usd: Option<f64>,
    },
    /// A pointer to a nested agent's own transcript. `external_file` means it lives in its own
    /// file, which is the only case the reader can follow today.
    SubagentRef {
        child_id: String,
        external_file: bool,
    },
    Compaction {
        kind: String,
    },
    Attachment {
        kind: String,
        bytes: usize,
    },
    Bookkeeping {
        type_name: String,
    },
    Unknown {
        type_name: String,
    },
}

impl EventKind {
    /// The stable label the JSON surface and the text surface both print.
    pub fn label(&self) -> &'static str {
        match self {
            EventKind::SessionMeta(_) => "session_meta",
            EventKind::UserMessage { .. } => "user_message",
            EventKind::AssistantText { .. } => "assistant_text",
            EventKind::Thinking { .. } => "thinking",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolResult { .. } => "tool_result",
            EventKind::PermissionRequest { .. } => "permission_request",
            EventKind::TurnBoundary { .. } => "turn_boundary",
            EventKind::Usage { .. } => "usage",
            EventKind::SubagentRef { .. } => "subagent_ref",
            EventKind::Compaction { .. } => "compaction",
            EventKind::Attachment { .. } => "attachment",
            EventKind::Bookkeeping { .. } => "bookkeeping",
            EventKind::Unknown { .. } => "unknown",
        }
    }

    /// Whether a transcript view would draw this. Bookkeeping and unknowns are structure, not
    /// conversation.
    pub fn is_renderable(&self) -> bool {
        !matches!(
            self,
            EventKind::Bookkeeping { .. } | EventKind::Unknown { .. }
        )
    }

    /// Cap every string this kind carries at `budget` bytes, for a headers-only read.
    fn truncate_strings(&mut self, budget: usize) {
        let cap = |s: &mut String| truncate_utf8(s, budget);
        match self {
            EventKind::SessionMeta(m) => {
                cap(&mut m.agent);
                for v in [&mut m.session_id, &mut m.cwd, &mut m.version, &mut m.model]
                    .into_iter()
                    .flatten()
                {
                    cap(v);
                }
            }
            EventKind::ToolCall {
                name,
                call_id,
                arg_keys,
                ..
            } => {
                cap(name);
                if let Some(id) = call_id {
                    cap(id);
                }
                for k in arg_keys {
                    cap(k);
                }
            }
            EventKind::ToolResult { call_id, .. } => {
                if let Some(id) = call_id {
                    cap(id);
                }
            }
            EventKind::PermissionRequest { tool, call_id } => {
                cap(tool);
                if let Some(id) = call_id {
                    cap(id);
                }
            }
            EventKind::TurnBoundary { reason, .. } => {
                if let Some(r) = reason {
                    cap(r);
                }
            }
            EventKind::SubagentRef { child_id, .. } => cap(child_id),
            EventKind::Compaction { kind } => cap(kind),
            EventKind::Attachment { kind, .. } => cap(kind),
            EventKind::Bookkeeping { type_name } | EventKind::Unknown { type_name } => {
                cap(type_name)
            }
            EventKind::UserMessage { .. }
            | EventKind::AssistantText { .. }
            | EventKind::Thinking { .. }
            | EventKind::Usage { .. } => {}
        }
    }
}

impl Event {
    /// Reduce to a header: drop the body, derive the preview from it, and cap every remaining
    /// string at the budget. This is the only thing that makes a window read bounded, so it runs
    /// on the reader's side of the adapter rather than being each adapter's job to remember.
    pub fn to_header(mut self, budget: &Budget) -> Event {
        if self.preview.is_none() {
            self.preview = self.body.as_ref().map(|b| first_line(b.as_str()));
        }
        self.body = None;
        if let Some(p) = &mut self.preview {
            truncate_utf8(p, budget.header_bytes);
        }
        self.kind.truncate_strings(budget.header_bytes);
        self
    }

    /// A header's size on the wire, near enough to bound a frame by: the label, the cursor, the
    /// timestamp and the preview are what dominate.
    pub(crate) fn header_cost(&self) -> usize {
        const FIXED: usize = 96; // punctuation, keys, and the cursor token
        FIXED
            + self.ts.as_ref().map_or(0, String::len)
            + self.preview.as_ref().map_or(0, String::len)
    }
}

/// The first line of a body, which is what a list row shows.
fn first_line(s: &str) -> String {
    s.split('\n').next().unwrap_or("").trim_end().to_string()
}

/// Truncate to at most `max` bytes without splitting a character.
fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// The header record every store but cursor-agent writes, or that the reader synthesizes from the
/// envelope fields the records carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub agent: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// The store's own version stamp. This is the key fixtures are pinned on, not the installed
    /// CLI version, because a session written by an old build keeps its old shape.
    pub version: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    Ok,
    Error,
    Pending,
    Unknown,
}

impl ResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ResultStatus::Ok => "ok",
            ResultStatus::Error => "error",
            ResultStatus::Pending => "pending",
            ResultStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    Start,
    End,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::Start => "start",
            TurnKind::End => "end",
        }
    }
}

/// The three bounds a window read honours. Every one of them is a cap on work the caller did not
/// ask for: a phone asking for 200 events must not be handed a 44 MiB file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Longest string leaf a header may carry. 256 bytes by default.
    pub header_bytes: usize,
    /// Most bytes one call may read from disk. 1 MiB by default, the same ceiling the codex tail
    /// widens to.
    pub read_bytes: u64,
    /// Most header bytes one window may return. 32 KiB by default, from the spike's measured 18.4
    /// to 21.3 KiB for 200 events, with margin.
    pub frame_bytes: usize,
}

impl Budget {
    pub const DEFAULT_HEADER_BYTES: usize = 256;
    pub const DEFAULT_READ_BYTES: u64 = 1024 * 1024;
    pub const DEFAULT_FRAME_BYTES: usize = 32 * 1024;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> Cursor {
        Cursor {
            file: FileId {
                dev: 0x1234,
                ino: 0xdead_beef,
            },
            size: 4096,
            offset: 512,
            part: 2,
        }
    }

    #[test]
    fn cursor_round_trips_through_its_opaque_form() {
        let c = cursor();
        let text = c.to_string();
        assert_eq!(text, "t1.1234.deadbeef.1000.200.2");
        assert_eq!(text.parse::<Cursor>().unwrap(), c);
    }

    #[test]
    fn a_hand_edited_cursor_is_a_parse_error_not_a_panic() {
        for bad in [
            "",
            "t1",
            "t2.1.1.1.1.1",
            "t1.1.1.1.1",
            "t1.1.1.1.1.1.1",
            "t1.z.1.1.1.1",
        ] {
            assert!(bad.parse::<Cursor>().is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_header_drops_the_body_and_caps_every_string() {
        let long = "y".repeat(400);
        let event = Event {
            cursor: cursor(),
            ts: None,
            kind: EventKind::ToolCall {
                name: long.clone(),
                call_id: Some(long.clone()),
                arg_keys: vec![long.clone()],
                bytes: 400,
            },
            preview: None,
            body: Some(Body::Json(format!("{{\"a\":\"{long}\"}}\nsecond line"))),
        };
        let header = event.to_header(&Budget::default());
        assert_eq!(header.body, None);
        assert_eq!(header.preview.as_ref().map(String::len), Some(256));
        match header.kind {
            EventKind::ToolCall {
                name,
                call_id,
                arg_keys,
                ..
            } => {
                assert_eq!(name.len(), 256);
                assert_eq!(call_id.unwrap().len(), 256);
                assert_eq!(arg_keys[0].len(), 256);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Four-byte chars against a budget that lands mid-character.
        let mut s = "😀".repeat(10);
        truncate_utf8(&mut s, 10);
        assert_eq!(s, "😀😀");
    }
}
