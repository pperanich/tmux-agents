//! OpenCode: `part` rows belonging to a `message`, in `~/.local/share/opencode/opencode.db`.
//!
//! The role rule the other adapters obey holds here too, and it is sharper: there is no
//! `message.role` **column** at all. `message` is `(id, session_id, time_created, time_updated,
//! data)` and the role lives at `json_extract(data, '$.role')`, so a query written from the
//! transcript spike's sentence ("`part` rows joined to their `message` for the role") fails with
//! `no such column: m.role`. The role is read out of the JSON and passed in here.
//!
//! The other structural difference from a JSONL store: **a tool call and its result are the same
//! row**. `state.status` mutates in place from `pending` to `running` to `completed` or `error`, so
//! the call event is minted on sight and the result event only once the row settles. A live
//! permission prompt reads as `running`, not `pending`, so an adapter that waited for a `pending`
//! status would wait forever.

use crate::adapters::{json_of, str_at, text_of, Mapped};
use crate::json::Value;
use crate::model::{Body, EventKind, ResultStatus, SessionMeta, TurnKind};

/// One `part` row to its events, in the order they belong in the stream.
pub(crate) fn map_part(role: &str, part: &Value) -> Vec<Mapped> {
    let ty = str_at(part, "type").unwrap_or_default();
    match ty.as_str() {
        "text" => {
            let text = text_of(part.get("text"));
            let kind = if role == "user" {
                EventKind::UserMessage {
                    bytes: text.len(),
                    attachments: 0,
                }
            } else {
                EventKind::AssistantText { bytes: text.len() }
            };
            vec![Mapped::with(kind, Body::Text(text))]
        }
        "reasoning" => {
            let text = text_of(part.get("text"));
            vec![Mapped::with(
                EventKind::Thinking {
                    bytes: text.len(),
                    redacted: false,
                },
                Body::Text(text),
            )]
        }
        "tool" => tool(part),
        "step-start" => vec![Mapped::bare(EventKind::TurnBoundary {
            kind: TurnKind::Start,
            reason: None,
        })],
        "step-finish" => step_finish(part),
        "file" | "patch" => {
            let (_, bytes) = json_of(Some(part));
            vec![Mapped::bare(EventKind::Attachment { kind: ty, bytes })]
        }
        "compaction" => vec![Mapped::bare(EventKind::Compaction { kind: ty })],
        "agent" | "snapshot" => vec![Mapped::bookkeeping(format!("part/{ty}"))],
        other => vec![Mapped::unknown(format!("opencode/part/{other}"))],
    }
}

/// The call, and the result once the row has settled.
fn tool(part: &Value) -> Vec<Mapped> {
    let null = Value::Null;
    let state = part.get("state").unwrap_or(&null);
    let name = str_at(part, "tool").unwrap_or_else(|| "?".into());
    // opencode writes a nested agent into a child row of the same `session` table rather than into
    // a file of its own, so the reference is real but not yet followable.
    if name == "task" {
        return vec![Mapped::bare(EventKind::SubagentRef {
            child_id: state
                .path(&["metadata", "sessionID"])
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            external_file: false,
        })];
    }
    let call_id = str_at(part, "callID");
    let input = state.get("input");
    let (args, bytes) = json_of(input);
    let call = Mapped::with(
        EventKind::ToolCall {
            name,
            call_id: call_id.clone(),
            arg_keys: input.map(Value::keys).unwrap_or_default(),
            bytes,
        },
        Body::Json(args),
    );
    let (status, payload) = match str_at(state, "status").unwrap_or_default().as_str() {
        "completed" => (ResultStatus::Ok, state.get("output")),
        // A denied call settles here, with the user's own feedback quoted in `state.error`.
        "error" => (ResultStatus::Error, state.get("error")),
        // `pending` and `running` are both in flight, and a call awaiting a permission reads as
        // `running`. In flight means no result event yet, which is what every other store says too.
        _ => return vec![call],
    };
    let (body, bytes) = settled(payload);
    vec![
        call,
        Mapped::with(
            EventKind::ToolResult {
                call_id,
                status,
                bytes,
            },
            body,
        ),
    ]
}

/// A settled tool's payload. `output` and `error` are both prose, so a bare string stays text
/// rather than being re-quoted as a JSON literal.
fn settled(v: Option<&Value>) -> (Body, usize) {
    match v {
        Some(Value::Str(s)) => (Body::Text(s.clone()), s.len()),
        other => {
            let (json, bytes) = json_of(other);
            (Body::Json(json), bytes)
        }
    }
}

/// The end of a step, plus that step's usage when the row carries one.
fn step_finish(part: &Value) -> Vec<Mapped> {
    let mut out = vec![Mapped::bare(EventKind::TurnBoundary {
        kind: TurnKind::End,
        reason: str_at(part, "finishReason"),
    })];
    if let Some(tokens) = part.get("tokens") {
        out.push(Mapped::bare(EventKind::Usage {
            input: tokens.get("input").and_then(Value::as_u64),
            output: tokens.get("output").and_then(Value::as_u64),
            total: None,
            context_window: None,
            cost_usd: cost(part.get("cost")),
        }));
    }
    out
}

fn cost(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Num(raw)) => raw.parse::<f64>().ok().filter(|c| c.is_finite()),
        _ => None,
    }
}

/// The header opencode writes as a row rather than as a record: the session's own fields, plus the
/// model named by the newest message that named one.
pub(crate) fn session_meta(id: &str, data: &Value, model: Option<String>) -> SessionMeta {
    SessionMeta {
        agent: "opencode".into(),
        session_id: Some(id.to_string()),
        cwd: str_at(data, "directory"),
        version: str_at(data, "version"),
        model,
    }
}
