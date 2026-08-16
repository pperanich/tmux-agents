use tma_core::render;
use tma_core::stamp::opt;
use tma_core::{AgentState, StampedState};
use tma_tmux::tmux::Tmux;

use super::mapping::{json_string_field, EventPlan};
use super::PERMISSION_REPLIED;

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
