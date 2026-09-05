//! pi: `~/.pi/agent/sessions/--<cwd-slug>--/<iso>_<session-uuid>.jsonl`.
//!
//! pi is the store that forced the role-before-block rule. Its content blocks are Anthropic-shaped
//! for `text` and `thinking`, but its tool call is not (`{type: "toolCall", id, name, arguments}`,
//! camelCase, `arguments` where Anthropic writes `input`), and its tool **result** is hoisted to a
//! fourth message role with the correlation id at message level. A reader that runs the block loop
//! for every role maps that result's plain `text` block to assistant prose: the tool's output
//! rendered as the model's own words, at 100% mapped and zero `Unknown`. Hence the interception
//! below, before any block is looked at.

use crate::adapters::{block, json_of, status_from_error_flag, str_at, Mapped};
use crate::json::Value;
use crate::model::{Body, EventKind, SessionMeta};

pub(super) fn map(v: &Value) -> Vec<Mapped> {
    let ty = str_at(v, "type").unwrap_or_default();
    match ty.as_str() {
        "session" => vec![Mapped::bare(EventKind::SessionMeta(meta_from(v)))],
        "message" => message(v),
        "model_change" | "thinking_level_change" | "title" | "compaction" => {
            vec![Mapped::bookkeeping(ty)]
        }
        other => vec![Mapped::unknown(format!("pi/{other}"))],
    }
}

fn message(v: &Value) -> Vec<Mapped> {
    let Some(msg) = v.get("message") else {
        return vec![Mapped::unknown("pi/message/no-message".into())];
    };
    let role = str_at(msg, "role").unwrap_or_default();
    let mut out = Vec::new();
    if role == "toolResult" {
        let (json, bytes) = json_of(msg.get("content"));
        out.push(Mapped::with(
            EventKind::ToolResult {
                call_id: str_at(msg, "toolCallId"),
                status: status_from_error_flag(msg.get("isError").and_then(Value::as_bool)),
                bytes,
            },
            Body::Json(json),
        ));
    } else if let Some(items) = msg.get("content").and_then(Value::as_arr) {
        out.extend(items.iter().map(|it| pi_block(it, &role)));
    }
    if let Some(u) = msg.get("usage") {
        out.push(Mapped::bare(usage_from(u)));
    }
    if out.is_empty() {
        out.push(Mapped::unknown(format!("pi/message/{role}")));
    }
    out
}

/// pi's own tool-call block, then the shared Anthropic shape for everything else.
fn pi_block(it: &Value, role: &str) -> Mapped {
    if it.get("type").and_then(Value::as_str) != Some("toolCall") {
        return block(it, role);
    }
    let args = it.get("arguments");
    let (json, bytes) = json_of(args);
    Mapped::with(
        EventKind::ToolCall {
            name: str_at(it, "name").unwrap_or_else(|| "?".into()),
            call_id: str_at(it, "id"),
            arg_keys: args.map(Value::keys).unwrap_or_default(),
            bytes,
        },
        Body::Json(json),
    )
}

/// pi is the only store that writes cost, so the usage event carries it.
fn usage_from(u: &Value) -> EventKind {
    EventKind::Usage {
        input: u.get("input").and_then(Value::as_u64),
        output: u.get("output").and_then(Value::as_u64),
        total: u.get("totalTokens").and_then(Value::as_u64),
        context_window: None,
        cost_usd: u
            .path(&["cost", "total"])
            .and_then(|c| match c {
                Value::Num(raw) => raw.parse::<f64>().ok(),
                _ => None,
            })
            .filter(|c| c.is_finite()),
    }
}

pub(super) fn session_meta(v: &Value) -> Option<SessionMeta> {
    (str_at(v, "type").as_deref() == Some("session")).then(|| meta_from(v))
}

/// pi's `version` is a number, not a string, so it is read through the compact form rather than
/// `as_str`; a fixture whose filename claims 0.84.2 pins the schema version beside it.
fn meta_from(v: &Value) -> SessionMeta {
    SessionMeta {
        agent: "pi".into(),
        session_id: str_at(v, "id"),
        cwd: str_at(v, "cwd"),
        version: v
            .get("version")
            .map(Value::to_compact)
            .map(|s| s.trim_matches('"').to_string()),
        model: str_at(v, "modelId"),
    }
}
