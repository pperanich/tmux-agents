//! One adapter per store, plus the two rules they all obey.
//!
//! **Stateless per record.** An adapter maps one record to zero or more events and carries nothing
//! between records. That is not a style choice: it is what makes an end-anchored read of a suffix
//! of the file produce exactly the events a full-file parse would produce for that suffix, which is
//! what [`crate::Reader::window`] depends on to page backwards without ever reading the head.
//!
//! **Role before block type.** A store's record carries a role, and the role decides what its
//! content blocks mean. pi is the store that proves it: a `toolResult` record's content is a plain
//! `text` block, so an adapter that dispatches on block type first renders the tool's output as the
//! assistant's own prose, with no `Unknown` to alert on. Every adapter here reads the role first.

mod claude;
mod codex;
mod gemini;
mod pi;

use crate::json::Value;
use crate::model::{Body, EventKind, ResultStatus, SessionMeta, Store};

/// One event as an adapter produces it, before the reader attaches a cursor and a budget.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mapped {
    pub kind: EventKind,
    pub body: Option<Body>,
}

impl Mapped {
    pub(crate) fn bare(kind: EventKind) -> Mapped {
        Mapped { kind, body: None }
    }

    pub(crate) fn with(kind: EventKind, body: Body) -> Mapped {
        Mapped {
            kind,
            body: Some(body),
        }
    }

    pub(crate) fn unknown(type_name: String) -> Mapped {
        Mapped::bare(EventKind::Unknown { type_name })
    }

    pub(crate) fn bookkeeping(type_name: String) -> Mapped {
        Mapped::bare(EventKind::Bookkeeping { type_name })
    }
}

/// Map one parsed record. Returns the record's timestamp (verbatim, as the store wrote it) and its
/// events in the order they belong in the stream.
pub(crate) fn map_record(store: Store, v: &Value) -> (Option<String>, Vec<Mapped>) {
    match store {
        Store::Claude => (str_at(v, "timestamp"), claude::map(v)),
        Store::Codex => (str_at(v, "timestamp"), codex::map(v)),
        Store::Gemini => (gemini::timestamp(v), gemini::map(v)),
        Store::Pi => (str_at(v, "timestamp"), pi::map(v)),
        // The refused stores never reach an adapter; the reader turns them into a typed refusal
        // before it opens anything.
        Store::Cursor | Store::OpenCode => (None, Vec::new()),
    }
}

/// The session header a store writes (or, for claude, the envelope fields every record carries).
/// `None` when this record is not the one that carries it.
pub(crate) fn session_meta(store: Store, v: &Value) -> Option<SessionMeta> {
    match store {
        Store::Claude => claude::session_meta(v),
        Store::Codex => codex::session_meta(v),
        Store::Gemini => gemini::session_meta(v),
        Store::Pi => pi::session_meta(v),
        Store::Cursor | Store::OpenCode => None,
    }
}

// ------------------------------------------------------------------ shared helpers

/// An owned string field, or `None` when the key is absent or not a string.
pub(crate) fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The compact JSON of a field, and its length. Absent reads as `null`, which is 4 bytes and an
/// empty body rather than a missing one, so a caller never has to distinguish the two.
pub(crate) fn json_of(v: Option<&Value>) -> (String, usize) {
    let text = v.unwrap_or(&Value::Null).to_compact();
    let len = text.len();
    (text, len)
}

/// A `text`-shaped leaf's string, or the empty string.
pub(crate) fn text_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// `is_error`-style booleans, in the direction every store writes them.
pub(crate) fn status_from_error_flag(flag: Option<bool>) -> ResultStatus {
    match flag {
        Some(true) => ResultStatus::Error,
        Some(false) => ResultStatus::Ok,
        None => ResultStatus::Unknown,
    }
}

/// An Anthropic-shaped content block. claude and pi both speak this; pi intercepts its own
/// `toolCall` shape before falling through here.
pub(crate) fn block(it: &Value, role: &str) -> Mapped {
    match it.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" if role == "user" => {
            let text = text_of(it.get("text"));
            Mapped::with(
                EventKind::UserMessage {
                    bytes: text.len(),
                    attachments: 0,
                },
                Body::Text(text),
            )
        }
        "text" => {
            let text = text_of(it.get("text"));
            Mapped::with(
                EventKind::AssistantText { bytes: text.len() },
                Body::Text(text),
            )
        }
        "thinking" => {
            let text = text_of(it.get("thinking"));
            Mapped::with(
                EventKind::Thinking {
                    bytes: text.len(),
                    redacted: false,
                },
                Body::Text(text),
            )
        }
        "redacted_thinking" => Mapped::bare(EventKind::Thinking {
            bytes: 0,
            redacted: true,
        }),
        "tool_use" => {
            let name = str_at(it, "name").unwrap_or_else(|| "?".into());
            // The one call that continues in another file. claude 2.1.2xx writes the child's
            // records to `subagents/agent-*.jsonl` instead of inlining them as `isSidechain`.
            if name == "Task" || name == "Agent" {
                return Mapped::bare(EventKind::SubagentRef {
                    child_id: str_at(it, "id").unwrap_or_default(),
                    external_file: true,
                });
            }
            let input = it.get("input");
            let (json, bytes) = json_of(input);
            Mapped::with(
                EventKind::ToolCall {
                    name,
                    call_id: str_at(it, "id"),
                    arg_keys: input.map(Value::keys).unwrap_or_default(),
                    bytes,
                },
                Body::Json(json),
            )
        }
        "tool_result" => {
            let (json, bytes) = json_of(it.get("content"));
            Mapped::with(
                EventKind::ToolResult {
                    call_id: str_at(it, "tool_use_id"),
                    status: status_from_error_flag(it.get("is_error").and_then(Value::as_bool)),
                    bytes,
                },
                Body::Json(json),
            )
        }
        "image" => {
            let (_, bytes) = json_of(Some(it));
            Mapped::bare(EventKind::Attachment {
                kind: "image".into(),
                bytes,
            })
        }
        other => Mapped::unknown(format!("block/{other}")),
    }
}
