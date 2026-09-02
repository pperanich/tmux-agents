use regex::Regex;

use tma_core::evidence::{Claim, Lifecycle};
use tma_core::manifest::{HookMap, Manifest};
use tma_core::{AgentState, Detail, StampedState};

// The hook-event vocabulary lives in `tma_runtime::manifests` (agent description). `map_event` keys
// the two normative subagent events by name, so it imports those.
use crate::manifests::{SUBAGENT_START, SUBAGENT_STOP};

/// What a hook event resolves to before the subagent guard and transition logic (the pure output of
/// [`map_event`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mapped {
    /// A state claim from a `[[hooks.map]]` state entry.
    State {
        state: AgentState,
        detail: Option<Detail>,
        /// The manifest's `turn_end` flag for the entry that matched: this event MEANS a turn
        /// ended. Carried instead of re-derived, because it is knowledge only the hook has —
        /// `decide` would otherwise reconstruct the done edge from `prev == Working` exactly as
        /// the fold does, and miss every completion the fold cannot see either.
        turn_end: bool,
    },
    /// Registration (SessionStart-class lifecycle): mark the pane an agent pane, state idle.
    Register,
    /// Deregistration (SessionEnd-class lifecycle): remove every `@agent_*` option.
    Deregister,
    /// Subagent started — append the firing session id to `@agent_subagents`.
    SubagentStart,
    /// Subagent stopped — remove the firing session id from `@agent_subagents`.
    SubagentStop,
    /// The event is not one this agent maps (idle-reminder `Notification`, unknown event).
    Unmapped,
}

/// Resolve a hook event to a [`Mapped`] via the manifest `[[hooks.map]]`. A matching `matcher` wins;
/// a non-matching one is skipped; a matcher-less entry is the fallback.
pub fn map_event(kind: &str, payload: &str, manifest: &Manifest) -> Mapped {
    if kind == SUBAGENT_START {
        return Mapped::SubagentStart;
    }
    if kind == SUBAGENT_STOP {
        return Mapped::SubagentStop;
    }
    let Some(hooks) = &manifest.hooks else {
        return Mapped::Unmapped;
    };

    let mut fallback: Option<&HookMap> = None;
    for m in hooks.map.iter().filter(|m| m.event == kind) {
        match &m.matcher {
            None => {
                if fallback.is_none() {
                    fallback = Some(m);
                }
            }
            Some(rx) => {
                if matcher_matches(rx, payload) {
                    return mapped_from_entry(m);
                }
            }
        }
    }
    match fallback {
        Some(m) => mapped_from_entry(m),
        None => Mapped::Unmapped,
    }
}

fn mapped_from_entry(entry: &HookMap) -> Mapped {
    match &entry.claim {
        Claim::State(sc) => Mapped::State {
            state: sc.state,
            detail: sc.detail.clone(),
            turn_end: entry.turn_end,
        },
        Claim::Lifecycle {
            lifecycle: Lifecycle::Start,
        } => Mapped::Register,
        Claim::Lifecycle {
            lifecycle: Lifecycle::End,
        } => Mapped::Deregister,
    }
}

/// Apply a manifest `matcher` regex against the raw payload text. Matching the whole JSON blob (not
/// a field) is a v1 simplification, correct because the discriminator is a dedicated
/// `notification_type` field (verified Claude Code 2.1.212). A malformed regex fails safe to no
/// match (the hook must not error).
fn matcher_matches(rx: &str, payload: &str) -> bool {
    Regex::new(rx)
        .map(|re| re.is_match(payload))
        .unwrap_or(false)
}

/// The concrete write/fire plan for a hook event, after the subagent guard and transition
/// analysis. Pure output of [`decide`]; executed by [`execute`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EventPlan {
    /// The manifests in hand map this event to nothing (an idle-reminder `Notification`, an event
    /// this agent does not carry). NOT a decision about the pane — a peer holding a manifest that
    /// does map it should still apply it, which is why the daemon NAKs this rather than acking.
    Unmapped,
    /// A decision to write nothing: a foreign-session event while subagents are live. The pane was
    /// deliberately left alone, so this is a real verdict and the daemon acks it.
    Ignore,
    /// Remove every `@agent_*` option (SessionEnd-class lifecycle).
    Deregister,
    /// Rewrite `@agent_subagents` to this exact set (bookkeeping only, never touches state).
    Subagents(Vec<String>),
    /// Publish a hook-sourced state stamp (a fresh hook wins the fold), optionally recording the
    /// owning session and firing the blocked notification.
    Stamp {
        state: AgentState,
        detail: Option<Detail>,
        set_attention: bool,
        /// Record `@agent_session` (SessionStart, or a first hook on an observed-only pane).
        register_session: Option<String>,
        /// Record `@agent_turn_at`: this event ended a turn AND raised the done marker. Set with
        /// `set_attention`, never without it, so the two channels that report one codex turn end
        /// (`Stop` and `notify`) record one turn between them.
        record_turn: bool,
        /// Fire the blocked notification (write `@agent_notified_at` then display).
        notify: bool,
    },
}

impl EventPlan {
    /// Whether this plan is a decision about the pane rather than "these manifests know nothing
    /// about this event". The daemon's delivery ack keys on it.
    pub(crate) fn is_verdict(&self) -> bool {
        !matches!(self, EventPlan::Unmapped)
    }

    /// Whether the plan commits a state write, so the companion stamps (model, API endpoint) that
    /// ride the same suppression guard may land.
    pub(crate) fn commits(&self) -> bool {
        !matches!(self, EventPlan::Unmapped | EventPlan::Ignore)
    }
}

/// Fold a mapped event, firing session id, and stored stamp into an [`EventPlan`]. Pure: the
/// subagent ownership guard and notify dedup live here so they are unit-testable without tmux.
pub(crate) fn decide(
    mapped: Mapped,
    event_session: Option<&str>,
    stored: Option<&StampedState>,
    notify_opt_in: bool,
    notify_on: &[crate::config::NotifyTrigger],
    now: u64,
) -> EventPlan {
    let owner = stored.and_then(|s| s.session.as_deref());
    let subagents = stored.map(|s| s.subagents.clone()).unwrap_or_default();

    // Subagent bookkeeping fires regardless of ownership — the whole point is to track
    // foreign-session lifecycles on the parent pane.
    match mapped {
        Mapped::SubagentStart => {
            let mut v = subagents;
            if let Some(sid) = event_session {
                if !v.iter().any(|x| x == sid) {
                    v.push(sid.to_string());
                }
            }
            return EventPlan::Subagents(v);
        }
        Mapped::SubagentStop => {
            let mut v = subagents;
            if let Some(sid) = event_session {
                v.retain(|x| x != sid);
            }
            return EventPlan::Subagents(v);
        }
        Mapped::Unmapped => return EventPlan::Unmapped,
        _ => {}
    }

    // Subagent ownership guard: while a pane has live subagents, only the owning session may touch
    // state. With no recorded owner (an observed-only pane whose first hook was a SubagentStart)
    // nothing on this pane is attributable, so nothing may write: a subagent's own Stop would
    // otherwise stamp the parent idle while it is still working. Screen detection still covers the
    // pane, and the next SubagentStop empties the set.
    if !subagents.is_empty() {
        match (owner, event_session) {
            (Some(owner), Some(ev)) if ev == owner => {}
            _ => return EventPlan::Ignore,
        }
    }

    match mapped {
        Mapped::Deregister => EventPlan::Deregister,
        Mapped::Register => EventPlan::Stamp {
            // Registration marks the pane an agent pane at idle; idle-on-register is not a
            // noteworthy transition, so no attention.
            state: AgentState::Idle,
            detail: None,
            set_attention: false,
            register_session: event_session.map(str::to_string),
            record_turn: false,
            notify: false,
        },
        Mapped::State {
            state,
            detail,
            turn_end,
        } => {
            let prev = stored.map(|s| s.state);
            // A marker still standing means the pane carries an unacknowledged mark, so a turn end
            // has nothing new to say: it neither re-raises nor records a turn. That is what keeps
            // the two channels reporting ONE codex turn end (`Stop` then `notify`, milliseconds
            // apart) down to one raise and one notification — the only thing separating them from
            // two genuine turns is that the user cleared the marker in between.
            //
            // It is read off `s.attention` alone, so a pane sitting BLOCKED with the mark up is
            // covered by the same arm: a turn end arriving there does not record a turn either.
            // Not a regression (pre-`turn_end` the raise was `prev == Some(Working)`, which a
            // blocked pane also failed), and the mark is up and unacknowledged either way.
            let standing = stored.is_some_and(|s| s.attention);
            let set_attention = match state {
                AgentState::Blocked => prev != Some(AgentState::Blocked),
                // `turn_end` is the whole point: an idle→idle edge is invisible to the fold, which
                // sees only states and cannot tell a second completion from a quiet idle pane.
                AgentState::Idle => prev == Some(AgentState::Working) || (turn_end && !standing),
                _ => false,
            };
            // `@agent_since` is write-once per state run, so it does not move on idle→idle and
            // cannot carry the second completion's instant. `@agent_turn_at` does, and only for a
            // turn end that actually raised the marker.
            let record_turn = turn_end && state == AgentState::Idle && set_attention;
            // Notify dedup: fire once on a configured noteworthy transition. `set_attention` is that
            // signal, mapped by `notify::trigger_for` to a token `notify.on` gates; a stored
            // `notified_at >= now` means this state-run is already notified (cold-start rule).
            let trigger = crate::notify::trigger_for(state, set_attention);
            let already_notified = stored.and_then(|s| s.notified_at).is_some_and(|n| n >= now);
            let notify = notify_opt_in
                && trigger.is_some_and(|t| crate::config::trigger_enabled(notify_on, t))
                && !already_notified;
            // Record the owner from the event when the pane has none stored yet (a hook on an
            // observed-only pane registers its session); never overwrite an existing one.
            let register_session = match (owner, event_session) {
                (None, Some(ev)) => Some(ev.to_string()),
                _ => None,
            };
            EventPlan::Stamp {
                state,
                detail,
                set_attention,
                register_session,
                record_turn,
                notify,
            }
        }
        Mapped::SubagentStart | Mapped::SubagentStop | Mapped::Unmapped => {
            unreachable!("SubagentStart/SubagentStop/Unmapped were handled by the earlier match")
        }
    }
}

/// Extract the top-level `"session_id"` string from a hook payload. A hand-rolled read: the envelope
/// is verified from real captures, so a dependency-free extractor suffices.
pub(crate) fn parse_session_id(payload: &str) -> Option<String> {
    json_string_field(payload, "session_id")
}

/// The source text of a JSON **object**-valued field, braces included (`{"command":"ls"}`), for the
/// one payload field tma reads inside: Claude's `PermissionRequest` `tool_input`. Scans for the
/// matching close brace, tracking string literals so a `}` inside a value does not end it early.
/// `None` when the field is absent or is not an object.
pub(super) fn json_object_field<'a>(payload: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = payload[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in rest.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract a top-level JSON string field's value from a hook payload (a hand-rolled read, matching
/// [`parse_session_id`]'s discipline). Handles the common `\n`/`\t`/`\"` escapes; a value with
/// nested objects or non-string types is not parsed (returns `None`).
pub(super) fn json_string_field(payload: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = payload[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => return None,
            },
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}
