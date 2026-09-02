use tma_core::render;
use tma_core::stamp::opt;
use tma_core::{AgentState, StampedState};
use tma_tmux::tmux::Tmux;

use super::mapping::{json_object_field, json_string_field, EventPlan};
use super::PERMISSION_REPLIED;

/// Byte cap on [`opt::PENDING_SUMMARY`]. A pane option other people's status lines interpolate, so
/// it stays short enough to sit in one; the `…` that marks a truncation is inside the budget.
const SUMMARY_MAX_BYTES: usize = 120;
const TRUNCATION_MARK: &str = "…";

/// The permission-request effect for one event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PermReq {
    /// Stamp `@agent_permission_request` with this pending id.
    Set(String),
    /// Clear `@agent_permission_request` (the prompt ended).
    Clear,
    /// No change.
    None,
}

/// The pure permission-request decision: set when a committed blocked stamp carries a
/// request id, clear on a committed working/idle transition or a `permission.replied` edge (which
/// carries no state claim), else no change. Ownership for the committed-stamp cases already passed
/// `decide`'s guard; the replied edge is checked here against the stored owner.
pub(crate) fn permission_request_effect(
    kind: &str,
    plan: &EventPlan,
    stored_owner: Option<&str>,
    event_session: Option<&str>,
    payload: &str,
) -> PermReq {
    if kind == PERMISSION_REPLIED {
        // Only the owner (or an unattributable event) clears; a foreign session must not.
        return match (stored_owner, event_session) {
            (Some(owner), Some(ev)) if owner != ev => PermReq::None,
            _ => PermReq::Clear,
        };
    }
    match plan {
        EventPlan::Stamp { state, .. } => match state {
            AgentState::Blocked => match json_string_field(payload, "request_id") {
                Some(id) if !id.is_empty() => PermReq::Set(id),
                _ => PermReq::None,
            },
            AgentState::Working | AgentState::Idle => PermReq::Clear,
            AgentState::Unknown => PermReq::None,
        },
        _ => PermReq::None,
    }
}

/// The pending-call effect for one event: what happens to the `@agent_pending_*` trio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingCall {
    /// Stamp the trio for a permission prompt that just opened.
    Set {
        tool: String,
        call: String,
        summary: String,
    },
    /// Clear all three (the prompt ended).
    Clear,
    /// No change.
    None,
}

/// The pure pending-call decision, keyed on the same edges as [`permission_request_effect`]: a
/// committed blocked stamp carrying a `tool_name` sets, a committed working/idle transition or a
/// `permission.replied` clears, everything else leaves the trio alone.
///
/// Leaving it alone is what keeps the late `Notification permission_prompt` from wiping the stamp
/// `PermissionRequest` made six seconds earlier: it is a blocked stamp with no `tool_name`, so it
/// falls through to `None` rather than clearing.
pub(crate) fn pending_call_effect(
    kind: &str,
    plan: &EventPlan,
    stored_owner: Option<&str>,
    event_session: Option<&str>,
    payload: &str,
) -> PendingCall {
    if kind == PERMISSION_REPLIED {
        return match (stored_owner, event_session) {
            (Some(owner), Some(ev)) if owner != ev => PendingCall::None,
            _ => PendingCall::Clear,
        };
    }
    match plan {
        EventPlan::Stamp { state, .. } => match state {
            AgentState::Blocked => match json_string_field(payload, "tool_name") {
                Some(tool) if !tool.is_empty() => PendingCall::Set {
                    summary: pending_summary(&tool, payload),
                    call: json_string_field(payload, "tool_use_id").unwrap_or_default(),
                    tool,
                },
                _ => PendingCall::None,
            },
            AgentState::Working | AgentState::Idle => PendingCall::Clear,
            AgentState::Unknown => PendingCall::None,
        },
        _ => PendingCall::None,
    }
}

/// The one-line summary of a pending call, from the hook's `tool_input`: the command for `Bash`,
/// the file path for the file tools, otherwise the first string-valued field (a tool tma has no
/// shape for still says something). Empty when the payload carries no usable string.
fn pending_summary(tool: &str, payload: &str) -> String {
    let Some(input) = json_object_field(payload, "tool_input") else {
        return String::new();
    };
    let raw = match tool {
        "Bash" => json_string_field(input, "command"),
        "Edit" | "Write" | "Read" => json_string_field(input, "file_path"),
        _ => None,
    }
    .or_else(|| first_string_field(input))
    .unwrap_or_default();
    cap_summary(one_line(&raw))
}

/// The first string-valued field of a JSON object's own level, in source order. A string at depth 1
/// is a key when a `:` follows it and a value otherwise, which is the whole discriminator.
fn first_string_field(obj: &str) -> Option<String> {
    let (mut depth, mut rest) = (0usize, obj);
    while let Some(c) = rest.chars().next() {
        let consumed = match c {
            '{' | '[' => {
                depth += 1;
                c.len_utf8()
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                c.len_utf8()
            }
            '"' => {
                let (value, after) = read_json_string(&rest[1..])?;
                if depth == 1 && !rest[1 + after..].trim_start().starts_with(':') {
                    return Some(value);
                }
                1 + after
            }
            _ => c.len_utf8(),
        };
        rest = &rest[consumed..];
    }
    None
}

/// Read one JSON string body (`rest` starts just past the opening quote), returning its unescaped
/// value and the byte length consumed including the closing quote.
fn read_json_string(rest: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, other)) => out.push(other),
                None => return None,
            },
            '"' => return Some((out, i + 1)),
            c => out.push(c),
        }
    }
    None
}

/// Flatten agent-supplied text to one line: every control character (newlines and tabs included)
/// becomes a space, runs of whitespace collapse, and the ends are trimmed.
fn one_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_control() || c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// Truncate to [`SUMMARY_MAX_BYTES`] on a char boundary, marking the cut with `…`.
fn cap_summary(s: String) -> String {
    if s.len() <= SUMMARY_MAX_BYTES {
        return s;
    }
    let mut end = SUMMARY_MAX_BYTES - TRUNCATION_MARK.len();
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_MARK}", &s[..end])
}

/// Apply the [`pending_call_effect`] to the pane's `@agent_pending_*` trio, in one chained
/// invocation so a reader never sees a tool without its summary.
pub(super) fn apply_pending_call(
    tmux: &Tmux,
    pane: &str,
    kind: &str,
    plan: &EventPlan,
    stored: Option<&StampedState>,
    event_session: Option<&str>,
    payload: &str,
) -> bool {
    let owner = stored.and_then(|s| s.session.as_deref());
    let cmds = match pending_call_effect(kind, plan, owner, event_session, payload) {
        PendingCall::Set {
            tool,
            call,
            summary,
        } => vec![
            render::set_pane_option(pane, opt::PENDING_TOOL, &tool),
            render::set_pane_option(pane, opt::PENDING_CALL, &call),
            render::set_pane_option(pane, opt::PENDING_SUMMARY, &summary),
        ],
        PendingCall::Clear => vec![
            render::unset_pane_option(pane, opt::PENDING_TOOL),
            render::unset_pane_option(pane, opt::PENDING_CALL),
            render::unset_pane_option(pane, opt::PENDING_SUMMARY),
        ],
        PendingCall::None => return false,
    };
    let _ = tmux.apply(&cmds);
    true
}

/// Apply the [`permission_request_effect`] to the pane's `@agent_permission_request` option.
/// Returns whether a write was issued: `permission.replied` maps to no state claim yet still clears
/// the option, so this is the one way an otherwise-unmapped event counts as work actually applied.
pub(super) fn apply_permission_request(
    tmux: &Tmux,
    pane: &str,
    kind: &str,
    plan: &EventPlan,
    stored: Option<&StampedState>,
    event_session: Option<&str>,
    payload: &str,
) -> bool {
    let owner = stored.and_then(|s| s.session.as_deref());
    let cmd = match permission_request_effect(kind, plan, owner, event_session, payload) {
        PermReq::Set(id) => render::set_pane_option(pane, opt::PERMISSION_REQUEST, &id),
        PermReq::Clear => render::unset_pane_option(pane, opt::PERMISSION_REQUEST),
        PermReq::None => return false,
    };
    let _ = tmux.apply(&[cmd]);
    true
}
