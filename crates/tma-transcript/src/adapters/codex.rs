//! Codex: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<iso>-<session-id>.jsonl`.
//!
//! One file, two parallel channels that restate each other. `event_msg` is the UI stream and
//! `response_item` is the model-API transcript, and a turn appears in both. The split this adapter
//! takes, so a turn collapses to one event rather than two: **prose comes from `event_msg`, tool
//! calls come from `response_item`**, and each channel's mirror of the other is bookkeeping.
//!
//! The tool half is the load-bearing choice. On 0.146.0 a driven approval produced
//! `response_item/function_call` with no `exec_command_begin` beside it, so `response_item` is the
//! channel that is always written; `event_msg`'s exec and patch events are the ones that may be
//! absent. A future build that inverts that would show up as tool calls going missing, not as
//! doubled ones.

use crate::adapters::{json_of, str_at, text_of, Mapped};
use crate::json::Value;
use crate::model::{Body, EventKind, ResultStatus, SessionMeta, TurnKind};

pub(super) fn map(v: &Value) -> Vec<Mapped> {
    let outer = str_at(v, "type").unwrap_or_default();
    let null = Value::Null;
    let p = v.get("payload").unwrap_or(&null);
    let inner = str_at(p, "type").unwrap_or_default();
    vec![match (outer.as_str(), inner.as_str()) {
        ("session_meta", _) => Mapped::bare(EventKind::SessionMeta(meta_from(p))),
        ("turn_context" | "world_state", _) => Mapped::bookkeeping(outer.clone()),
        ("compacted", _) => Mapped::bare(EventKind::Compaction {
            kind: "compacted".into(),
        }),

        // The UI channel: prose, turn shape, and usage.
        ("event_msg", "task_started") => Mapped::bare(EventKind::TurnBoundary {
            kind: TurnKind::Start,
            reason: None,
        }),
        ("event_msg", "task_complete") => Mapped::bare(EventKind::TurnBoundary {
            kind: TurnKind::End,
            reason: None,
        }),
        ("event_msg", "user_message") => {
            let text = text_of(p.get("message"));
            Mapped::with(
                EventKind::UserMessage {
                    bytes: text.len(),
                    attachments: p
                        .get("images")
                        .and_then(Value::as_arr)
                        .map_or(0, <[_]>::len),
                },
                Body::Text(text),
            )
        }
        ("event_msg", "agent_message") => {
            let text = text_of(p.get("message"));
            Mapped::with(
                EventKind::AssistantText { bytes: text.len() },
                Body::Text(text),
            )
        }
        ("event_msg", "agent_reasoning" | "agent_reasoning_raw_content") => {
            let text = text_of(p.get("text"));
            Mapped::with(
                EventKind::Thinking {
                    bytes: text.len(),
                    redacted: false,
                },
                Body::Text(text),
            )
        }
        ("event_msg", "token_count") => Mapped::bare(usage_from(p)),
        ("event_msg", "exec_approval_request" | "apply_patch_approval_request") => {
            Mapped::bare(EventKind::PermissionRequest {
                tool: inner.clone(),
                call_id: str_at(p, "call_id"),
            })
        }
        // The exec/patch mirror of `response_item`'s call. Named, not drawn: drawing both is the
        // double-count this adapter exists to avoid.
        (
            "event_msg",
            "exec_command_begin" | "exec_command_end" | "patch_apply_begin" | "patch_apply_end",
        ) => Mapped::bookkeeping(format!("event_msg/{inner}")),

        // The API channel: tool calls, and a mirror of the prose above.
        ("response_item", "message" | "reasoning") => {
            Mapped::bookkeeping(format!("response_item/{inner}"))
        }
        ("response_item", "function_call" | "local_shell_call" | "custom_tool_call") => {
            let args = text_of(p.get("arguments"));
            Mapped::with(
                EventKind::ToolCall {
                    name: str_at(p, "name").unwrap_or_else(|| inner.clone()),
                    call_id: str_at(p, "call_id"),
                    // `arguments` is a JSON *string*, so the key list is one level down.
                    arg_keys: crate::json::parse(&args)
                        .map(|v| v.keys())
                        .unwrap_or_default(),
                    bytes: args.len(),
                },
                Body::Json(args),
            )
        }
        (
            "response_item",
            "function_call_output" | "local_shell_call_output" | "custom_tool_call_output",
        ) => {
            let (json, bytes) = json_of(p.get("output"));
            Mapped::with(
                EventKind::ToolResult {
                    call_id: str_at(p, "call_id"),
                    status: ResultStatus::Unknown,
                    bytes,
                },
                Body::Json(json),
            )
        }

        ("event_msg", _) => Mapped::bookkeeping(format!("event_msg/{inner}")),
        (o, i) => Mapped::unknown(format!("codex/{o}/{i}")),
    }]
}

pub(super) fn session_meta(v: &Value) -> Option<SessionMeta> {
    if str_at(v, "type").as_deref() != Some("session_meta") {
        return None;
    }
    Some(meta_from(v.get("payload")?))
}

fn meta_from(p: &Value) -> SessionMeta {
    SessionMeta {
        agent: "codex".into(),
        session_id: str_at(p, "session_id").or_else(|| str_at(p, "id")),
        cwd: str_at(p, "cwd"),
        version: str_at(p, "cli_version"),
        model: str_at(p, "model"),
    }
}

/// `event_msg/token_count`: the turn's own usage from `last_token_usage`, the session total and the
/// window from their own fields. Codex writes no cost.
fn usage_from(p: &Value) -> EventKind {
    let null = Value::Null;
    let info = p.get("info").unwrap_or(&null);
    let last = info.get("last_token_usage").unwrap_or(&null);
    EventKind::Usage {
        input: last.get("input_tokens").and_then(Value::as_u64),
        output: last.get("output_tokens").and_then(Value::as_u64),
        total: info
            .path(&["total_token_usage", "total_tokens"])
            .and_then(Value::as_u64),
        context_window: info.get("model_context_window").and_then(Value::as_u64),
        cost_usd: None,
    }
}
