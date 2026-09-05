//! Claude Code: `~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`, one JSON object per line.
//!
//! Nested agents are not inline. A `tool_use` named `Task` becomes a [`EventKind::SubagentRef`]
//! pointing at `<session-uuid>/subagents/agent-<id>.jsonl`, which the reader serves as its own
//! session rather than interleaving into the parent's window.

use crate::adapters::{block, json_of, str_at, Mapped};
use crate::json::Value;
use crate::model::{Body, EventKind, SessionMeta, TurnKind};

/// Sidecar record types the transcript view never draws. Naming them keeps them out of the
/// `Unknown` counter, which is the counter a drift check watches.
const SIDECARS: &[&str] = &[
    "mode",
    "permission-mode",
    "ai-title",
    "last-prompt",
    "atis-latch",
    "file-history-snapshot",
    "file-history-delta",
    "queue-operation",
    "bridge-session",
    "summary",
];

pub(super) fn map(v: &Value) -> Vec<Mapped> {
    let ty = str_at(v, "type").unwrap_or_default();
    match ty.as_str() {
        // Role first: the envelope's `type` and the message's `role` agree here, and the content
        // blocks are read in the role's light.
        "user" | "assistant" => {
            let msg = v.get("message");
            let role = msg.and_then(|m| str_at(m, "role")).unwrap_or(ty.clone());
            match msg.and_then(|m| m.get("content")) {
                Some(Value::Str(text)) => vec![Mapped::with(
                    EventKind::UserMessage {
                        bytes: text.len(),
                        attachments: 0,
                    },
                    Body::Text(text.clone()),
                )],
                Some(Value::Arr(items)) => items.iter().map(|it| block(it, &role)).collect(),
                _ => vec![Mapped::unknown(format!("claude/{ty}/no-content"))],
            }
        }
        "system" => {
            let sub = str_at(v, "subtype").unwrap_or_else(|| "-".into());
            match sub.as_str() {
                "turn_duration" => vec![Mapped::bare(EventKind::TurnBoundary {
                    kind: TurnKind::End,
                    reason: Some(sub),
                })],
                "compact_boundary" => vec![Mapped::bare(EventKind::Compaction { kind: sub })],
                _ => vec![Mapped::bookkeeping(format!("system/{sub}"))],
            }
        }
        "attachment" => {
            let (_, bytes) = json_of(v.get("attachment"));
            vec![Mapped::bare(EventKind::Attachment {
                kind: v
                    .path(&["attachment", "type"])
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                bytes,
            })]
        }
        t if SIDECARS.contains(&t) => vec![Mapped::bookkeeping(ty)],
        other => vec![Mapped::unknown(format!("claude/{other}"))],
    }
}

/// Claude writes no header record; every message record carries the envelope instead, so the first
/// one that does is the session's meta.
pub(super) fn session_meta(v: &Value) -> Option<SessionMeta> {
    let session_id = str_at(v, "sessionId")?;
    Some(SessionMeta {
        agent: "claude".into(),
        session_id: Some(session_id),
        cwd: str_at(v, "cwd"),
        version: str_at(v, "version"),
        model: None,
    })
}
