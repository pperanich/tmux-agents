//! Gemini: `~/.gemini/tmp/<projectHash>/chats/session-<iso>-<short-id>.jsonl`.
//!
//! Append-only, but not an event log. A header record, then per-message records, interleaved with
//! `$set` records that restate the **entire** `messages` array. The file only grows, so byte-offset
//! paging works; what does not work is treating a `$set` as content. Six sessions on the spike
//! machine held 19 real message records against 25 `$set` restatements of them, so a reader that
//! renders both shows every message several times.
//!
//! The rule here is the cheaper of the two the spike named: **drop `$set` entirely** (as
//! bookkeeping, so it is counted and not drawn) and render the per-message records. Deduping by
//! message id instead would need cross-record state, which would break end-anchored paging.

use crate::adapters::{json_of, str_at, Mapped};
use crate::json::Value;
use crate::model::{Body, EventKind, ResultStatus, SessionMeta};

pub(super) fn map(v: &Value) -> Vec<Mapped> {
    if v.get("$set").is_some() {
        return vec![Mapped::bookkeeping("gemini/$set".into())];
    }
    if v.get("kind").and_then(Value::as_str) == Some("main") {
        return vec![Mapped::bare(EventKind::SessionMeta(meta_from(v)))];
    }
    message(v)
}

fn message(v: &Value) -> Vec<Mapped> {
    let ty = str_at(v, "type").unwrap_or_default();
    let (content, content_bytes) = json_of(v.get("content"));
    match ty.as_str() {
        "user" => vec![Mapped::with(
            EventKind::UserMessage {
                bytes: content_bytes,
                attachments: 0,
            },
            Body::Json(content),
        )],
        "gemini" => {
            let mut out = Vec::new();
            if v.get("thoughts")
                .and_then(Value::as_arr)
                .is_some_and(|a| !a.is_empty())
            {
                let (thoughts, bytes) = json_of(v.get("thoughts"));
                out.push(Mapped::with(
                    EventKind::Thinking {
                        bytes,
                        redacted: false,
                    },
                    Body::Json(thoughts),
                ));
            }
            out.push(Mapped::with(
                EventKind::AssistantText {
                    bytes: content_bytes,
                },
                Body::Json(content),
            ));
            for c in v.get("toolCalls").and_then(Value::as_arr).unwrap_or(&[]) {
                out.extend(tool_call(c));
            }
            if let Some(t) = v.get("tokens") {
                out.push(Mapped::bare(EventKind::Usage {
                    input: t.get("input").and_then(Value::as_u64),
                    output: t.get("output").and_then(Value::as_u64),
                    total: t.get("total").and_then(Value::as_u64),
                    context_window: None,
                    cost_usd: None,
                }));
            }
            out
        }
        "info" | "error" | "compression" => vec![Mapped::bookkeeping(format!("gemini/{ty}"))],
        other => vec![Mapped::unknown(format!("gemini/{other}"))],
    }
}

/// One entry of `toolCalls[]`: gemini writes the call and its settled result in the same object.
fn tool_call(c: &Value) -> Vec<Mapped> {
    let args = c.get("args");
    let (args_json, args_bytes) = json_of(args);
    let (result_json, result_bytes) = json_of(c.get("result"));
    vec![
        Mapped::with(
            EventKind::ToolCall {
                name: str_at(c, "name").unwrap_or_else(|| "?".into()),
                call_id: str_at(c, "id"),
                arg_keys: args.map(Value::keys).unwrap_or_default(),
                bytes: args_bytes,
            },
            Body::Json(args_json),
        ),
        Mapped::with(
            EventKind::ToolResult {
                call_id: str_at(c, "id"),
                status: match str_at(c, "status").as_deref() {
                    Some("success") => ResultStatus::Ok,
                    Some("error") => ResultStatus::Error,
                    _ => ResultStatus::Unknown,
                },
                bytes: result_bytes,
            },
            Body::Json(result_json),
        ),
    ]
}

/// The header carries `startTime`, a message carries `timestamp`, and a `$set` carries neither at
/// top level.
pub(super) fn timestamp(v: &Value) -> Option<String> {
    str_at(v, "timestamp")
        .or_else(|| str_at(v, "startTime"))
        .or_else(|| {
            v.path(&["$set", "lastUpdated"])
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub(super) fn session_meta(v: &Value) -> Option<SessionMeta> {
    (v.get("kind").and_then(Value::as_str) == Some("main")).then(|| meta_from(v))
}

fn meta_from(v: &Value) -> SessionMeta {
    SessionMeta {
        agent: "gemini".into(),
        session_id: str_at(v, "sessionId"),
        // The header names a `projectHash`, never the working directory it hashes.
        cwd: None,
        version: str_at(v, "cliVersion"),
        model: str_at(v, "model"),
    }
}
