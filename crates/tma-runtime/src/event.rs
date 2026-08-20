//! `tma event`: the bridge from an agent's hook command to a stamped tmux option.
//!
//! Agent configs reference the `tma-hook` wrapper (which owns the frozen `<agent> <event-name>`
//! contract), not this internal CLI. `run` resolves the pane from `$TMUX_PANE`, maps the event to a
//! claim via the manifest's `[[hooks.map]]`, applies the subagent ownership guard, then attempts
//! daemon delivery, falling through to a direct guarded stamp + `@agent_summary` recompute when none
//! answers. It optionally fires the blocked notification (`TMA_NOTIFY_FROM_EVENT=1`, write-before-
//! fire). The event→decision logic is pure and unit-tested; only [`run`] touches tmux.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use tma_core::render::{self, Guard, Publish};
use tma_core::stamp::opt;
use tma_core::{Provenance, ReadResult, StampedState};

use tma_tmux::stamp::{self, StampPlan};
use tma_tmux::tmux::{PaneRecord, Tmux};

use crate::ipc::{self, DaemonSink};
use crate::manifests::{self, LoadedManifest};

mod context;
mod mapping;
mod permission;

use context::run_context;
use mapping::{decide, json_string_field, parse_session_id, EventPlan};
pub use mapping::{map_event, Mapped};
use permission::apply_permission_request;

/// Parsed `tma event` arguments (internal, unstable — the wrapper builds this).
pub struct EventArgs {
    pub agent: String,
    pub kind: String,
    /// Explicit target pane (the statusline shim passes `$TMUX_PANE` here); `None` falls back
    /// to the `$TMUX_PANE` env like the hook path. Only consulted by the `context` intake today.
    pub pane: Option<String>,
    /// `-` reads stdin; a path reads that file; `None` means no payload.
    pub payload: Option<String>,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    /// `notify.from_event`: daemonless direct-fire opt-in (`TMA_NOTIFY_FROM_EVENT` overrides it).
    pub notify_from_event: bool,
    /// `notify.command` plus the per-trigger `[notify.<trigger>]` overrides. `TMA_NOTIFY_CMD`
    /// replaces every one of them.
    pub notify_commands: crate::config::NotifyCommands,
    /// `notify.on`: which transitions fire the daemonless direct-fire (default `["blocked"]`). No
    /// env override.
    pub notify_on: Vec<crate::config::NotifyTrigger>,
    /// `notify.bell` / `notify.osc`: the tty sinks the direct-fire rides.
    pub notify_sinks: crate::config::NotifySinks,
    /// `notify.context_high.threshold`: the daemonless context-high notify threshold, `None`
    /// when unconfigured. Fires from the `context` intake under the same `from_event` opt-in as state.
    pub notify_context_high: Option<u8>,
    /// `[[agent]]` config: enable/disable + custom process-name maps.
    pub agents: Vec<crate::config::AgentConfig>,
}

/// Run `tma event`. A hook must never fail loudly: every error path exits 0 (the wrapper
/// suppresses output anyway). The observable effect is the stamped tmux options.
/// The reserved event kind for the context-telemetry intake, distinct from any agent hook
/// event: `tma event --kind context` reads a telemetry payload rather than mapping a hook event.
pub const CONTEXT_KIND: &str = "context";

/// The OpenCode `permission.replied` edge, forwarded by the plugin as a clear signal for
/// `@agent_permission_request`. It carries no state (maps to `Unmapped`), so the intake keys the
/// request-option clear on this name directly.
pub const PERMISSION_REPLIED: &str = "permission-replied";

pub fn run(args: EventArgs) -> ExitCode {
    // The context-telemetry intake is a separate lane: it resolves its own pane (explicit
    // `--pane` from the shim, else `$TMUX_PANE`), parses the metric, and stamps under the metric guards.
    if args.kind == CONTEXT_KIND {
        return run_context(args);
    }
    // Resolve the pane from `$TMUX_PANE`. Absent ⇒ not inside a pane; nothing to do.
    let pane = match std::env::var("TMUX_PANE") {
        Ok(p) if !p.is_empty() => p,
        _ => return ExitCode::SUCCESS,
    };

    // A skipped user manifest is silent here: a hook must never speak. The rest of the set still
    // loads, so one broken file no longer turns every hook event into a no-op.
    let manifests = match manifests::load(args.manifest_dir.as_deref(), &args.agents) {
        Ok(set) => set.manifests,
        Err(_) => return ExitCode::SUCCESS,
    };
    let Some(lm) = manifests.iter().find(|m| m.name == args.agent) else {
        // Unknown agent: no mapping to apply. Silent success (an unknown agent can be wired by
        // hand, but tma has no manifest to map it yet).
        return ExitCode::SUCCESS;
    };

    let payload = read_payload(args.payload.as_deref());
    let tmux = Tmux::connect(&args.server);

    // Daemon delivery first: hand the raw event to a running daemon over its per-server socket,
    // keyed by the same `#{socket_path}` `tma daemon` bound.
    if let Some(socket_path) = ipc::resolve_socket_path(&tmux) {
        let sink = DaemonSink {
            path: ipc::paths_for(&socket_path).socket,
        };
        if sink.deliver(&pane, &args.agent, &args.kind, &payload) {
            return ExitCode::SUCCESS;
        }
    }

    // No daemon (or delivery failed): direct guarded stamp through the shared adapter. Config
    // is canonical for both notify knobs; the env vars override it (documented test/CI seam).
    let notify_opt_in = match std::env::var("TMA_NOTIFY_FROM_EVENT") {
        Ok(v) => v == "1",
        Err(_) => args.notify_from_event,
    };
    let commands = args
        .notify_commands
        .overridden_by(crate::config::notify_cmd_env());
    // The outcome only matters to a relaying peer; this IS the last hop, so it is discarded.
    let _ = apply_event(
        &tmux,
        lm,
        &pane,
        &args.kind,
        &payload,
        &NotifyPolicy {
            opt_in: notify_opt_in,
            on: &args.notify_on,
            commands: &commands,
            sinks: &args.notify_sinks,
        },
        crate::now_ms(),
    );
    ExitCode::SUCCESS
}

/// What one event resolved to here, so a relaying peer knows whether to apply it itself. The daemon
/// acks only [`EventOutcome::Applied`]; on [`EventOutcome::Declined`] it NAKs and the client
/// direct-stamps with its own manifests, which is what keeps upgrade skew (a new CLI talking to a
/// resident daemon compiled against older manifests) from silently dropping transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOutcome {
    /// A decision was reached: a write plan, or a deliberate no-write verdict (the subagent
    /// ownership guard refusing a foreign session — re-applying that would double-write exactly
    /// what was correctly refused).
    Applied,
    /// Nothing was resolved: these manifests map the event to nothing, or the pane could not be read.
    Declined,
}

/// Whether (and how) [`apply_event`] fires the notification inline, on the daemonless path. The
/// daemon passes `opt_in: false` and dispatches from its own reconcile pass instead.
pub struct NotifyPolicy<'a> {
    /// `notify.from_event`, after the `TMA_NOTIFY_FROM_EVENT` override.
    pub opt_in: bool,
    /// `notify.on`: which transitions fire.
    pub on: &'a [crate::config::NotifyTrigger],
    pub commands: &'a crate::config::NotifyCommands,
    pub sinks: &'a crate::config::NotifySinks,
}

/// Map → decide → execute one hook event against tmux: the single path shared by the daemonless
/// fall-through and the daemon's socket handler, so a daemon-applied stamp is byte-for-byte
/// identical to a direct one. A gone/unreadable server is a clean no-op (a hook must never error).
pub fn apply_event(
    tmux: &Tmux,
    manifest: &LoadedManifest,
    pane: &str,
    kind: &str,
    payload: &str,
    notify: &NotifyPolicy<'_>,
    now: u64,
) -> EventOutcome {
    let panes = match tmux.list_panes() {
        Ok(p) => p,
        // Server gone or unreadable: nothing was applied, so a relaying peer should try itself.
        Err(_) => return EventOutcome::Declined,
    };
    let Some(rec) = panes.iter().find(|r| r.pane_id == pane) else {
        return EventOutcome::Declined;
    };
    let stored = StampedState::from_options(&rec.options)
        .ok()
        .flatten()
        .map(ReadResult::into_inner);

    let event_session = parse_session_id(payload);
    let mapped = map_event(kind, payload, &manifest.manifest);
    // Model identity: registration-class payloads carry the agent's model as a top-level
    // string (Claude `SessionStart`, Codex/Cursor session-start). Stamp `@agent_model` last-write-wins
    // — a pane's model changes only via the agent's own switcher — as a plain unguarded write, the same
    // one the Codex rollout tail uses (rollout.rs), so hook and tail writing the same value never fight.
    let register_model = matches!(mapped, Mapped::Register)
        .then(|| tma_core::hook_payload_model(payload))
        .flatten();
    // OpenCode stamps its serving base URL at registration (the plugin forwards
    // `PluginInput.serverUrl` as `api_endpoint`). Gated below on the plan committing, like the model.
    let register_endpoint = matches!(mapped, Mapped::Register)
        .then(|| json_string_field(payload, "api_endpoint"))
        .flatten();
    let plan = decide(
        mapped,
        event_session.as_deref(),
        stored.as_ref(),
        notify.opt_in,
        notify.on,
        now,
    );
    execute(
        tmux,
        &panes,
        pane,
        &manifest.name,
        stored.as_ref(),
        &plan,
        notify,
        now,
    );
    // Gated on the plan: a registration `decide` ignored (a foreign session's SessionStart under the
    // subagent ownership guard) must not stamp its model over the owner's either.
    if let Some(model) = register_model {
        if plan.commits() {
            let _ = tmux.apply(&[render::set_pane_option(pane, opt::MODEL, &model)]);
        }
    }
    // The endpoint stamp, gated the same way as the model.
    if let Some(endpoint) = register_endpoint {
        if plan.commits() {
            let _ = tmux.apply(&[render::set_pane_option(pane, opt::API_ENDPOINT, &endpoint)]);
        }
    }
    // Permission-request tracking: stamp the pending id when a permission prompt opens, clear
    // it on the edges that end the prompt (a working/idle transition, or a `permission.replied`).
    // `permission.replied` maps to nothing yet still writes, so its clear counts as work applied.
    let permission_wrote = apply_permission_request(
        tmux,
        pane,
        kind,
        &plan,
        stored.as_ref(),
        event_session.as_deref(),
        payload,
    );
    if plan.is_verdict() || permission_wrote {
        EventOutcome::Applied
    } else {
        EventOutcome::Declined
    }
}

/// Execute an [`EventPlan`] against tmux: one chained invocation per plan, plus the notification
/// display fired *after* the marker write commits (write-before-fire).
#[allow(clippy::too_many_arguments)]
fn execute(
    tmux: &Tmux,
    panes: &[PaneRecord],
    pane: &str,
    agent: &str,
    stored: Option<&StampedState>,
    plan: &EventPlan,
    policy: &NotifyPolicy<'_>,
    now: u64,
) {
    match plan {
        EventPlan::Unmapped | EventPlan::Ignore => {}
        EventPlan::Deregister => {
            let _ = stamp::apply(tmux, panes, pane, &StampPlan::Remove, true);
        }
        EventPlan::Subagents(ids) => {
            // Bookkeeping only: no state, no summary recompute (state is unchanged).
            let cmd = if ids.is_empty() {
                render::unset_pane_option(pane, opt::SUBAGENTS)
            } else {
                render::set_pane_option(pane, opt::SUBAGENTS, &ids.join(" "))
            };
            let _ = tmux.apply(&[cmd]);
        }
        EventPlan::Stamp {
            state,
            detail,
            set_attention,
            register_session,
            record_turn,
            notify,
        } => {
            let publish = Publish {
                pane_id: pane.to_string(),
                state: *state,
                detail: detail.clone(),
                source: Provenance::Hook,
                evidence_at: now,
                since: now,
                stamped_at: now,
                hash: stored.and_then(|s| s.hash),
                pid: stored.map(|s| s.pid).unwrap_or(0),
                name: agent.to_string(),
                set_attention: *set_attention,
                episode_reset: false,
                // A fresh hook event normally writes; the arbitration guard resolves races (two
                // daemonless hooks, or a late ack-timeout direct-stamp trailing a newer daemon
                // event) by evidence time, not finish order, so an older-fired event cannot clobber
                // a strictly-newer stored one. A first or equal-age event still writes.
                guard: Guard::HookArbitrate { evidence_at: now },
            };
            let mut cmds = render::render_publish(&publish);
            // Store-chain rule: the companions ride the same suppression guard as the state tuple,
            // so an event that loses arbitration commits none of them. Appended unguarded, a loser
            // would overwrite the winner's session, re-arm the notify marker, and roll a summary
            // from a state that never committed.
            if let Some(sess) = register_session {
                cmds.push(render::set_pane_option_guarded(
                    pane,
                    opt::SESSION,
                    publish.guard,
                    sess,
                ));
            }
            // The turn-end instant, on the same guard as the state tuple: `@agent_since` is
            // write-once per state run and cannot move on the idle→idle edge a second completion
            // draws, so this is what the notify dedup and `wait --since` compare against
            // (`StampedState::episode_at`). Written only when the turn end raised the marker.
            if *record_turn {
                cmds.push(render::set_pane_option_guarded(
                    pane,
                    opt::TURN_AT,
                    publish.guard,
                    &now.to_string(),
                ));
            }
            // Clamp the marker past the episode instant so a backward wall-clock step never writes
            // a marker that predates the episode it dedups (which would re-fire). Under a monotone
            // clock this is exactly `now`, so tested dedup is unchanged.
            let mark_at = now.max(
                stored
                    .filter(|s| s.state == *state)
                    .map_or(now, StampedState::episode_at),
            );
            // Write-before-fire: the guarded marker commits strictly before the display below, so a
            // crash between them drops at most one notification. It commits iff the state write won.
            if *notify {
                cmds.push(render::set_pane_option_guarded(
                    pane,
                    opt::NOTIFIED_AT,
                    publish.guard,
                    &mark_at.to_string(),
                ));
            }
            // Recompute the window and session rollups off the post-write state, guarded so a
            // suppressed stamp holds the winner's summaries rather than clobbering them.
            cmds.push(stamp::window_summary_command_guarded(
                panes,
                pane,
                Some(*state),
                publish.guard,
            ));
            cmds.push(stamp::session_summary_command_guarded(
                panes,
                pane,
                Some(*state),
                publish.guard,
            ));
            let _ = tmux.apply(&cmds);

            // Fire iff the guarded marker committed (this event won arbitration). The guard is a
            // server-side `-F` conditional, so "did the write win" is only knowable by reading back
            // the store (one extra round-trip on the notify path). This matches the daemon's dispatch
            // for at-most-once: a loser reads the winner's older value (`!= mark_at`) and stays
            // silent. Compare against the clamped `mark_at`, not raw `now`.
            let marker_won = *notify
                && tmux
                    .get_pane_option(pane, opt::NOTIFIED_AT)
                    .ok()
                    .flatten()
                    .is_some_and(|m| m == mark_at.to_string());

            // The caller resolved this pane out of the same `panes` read, so the lookup holds.
            let rec = panes.iter().find(|r| r.pane_id == pane);
            // `tma mute` suppresses the fire, never the write above: the episode is stamped and
            // marked notified exactly as it would be unmuted, so nothing replays when the mute ends.
            if let (true, Some(rec)) = (marker_won, rec.filter(|r| !crate::notify::muted(r, now))) {
                // Daemonless direct-fire: the same [`crate::notify::fire`] the daemon uses
                // (`display-message` + the resolved `notify.command`), strictly after the marker.
                // The payload carries the trigger word (`blocked`/`done`), not the landing token
                // (`idle` for a done fire); `notify` is only set for a notifiable transition.
                let trigger = crate::notify::trigger_for(*state, *set_attention);
                let word = trigger.map(|t| t.word()).unwrap_or_else(|| state.token());
                // The trigger picks the command, so a `[notify.blocked]`/`[notify.done]` override
                // routes here exactly as it does in the daemon's dispatch.
                let command = trigger.and_then(|t| policy.commands.for_trigger(t));
                // `since` is this event's own `now`: the hook fires on the transition it just wrote.
                let n = crate::notify::notification_for(
                    rec,
                    agent,
                    word,
                    detail.as_ref().map(|d| d.as_str().to_string()),
                    stored.and_then(|s| s.session.clone()),
                    now,
                    now,
                );
                if let Some(mut child) = crate::notify::fire(tmux, &n, command, policy.sinks) {
                    // Wait so the notification is delivered before the one-shot process exits. The
                    // status is already in hand here, so record a failing sink rather than discard it.
                    if let Ok(status) = child.wait() {
                        if let Some(cmd) = command {
                            crate::notify::failure::record_exit(cmd, &status, now);
                        }
                    }
                }
            }
        }
    }
}

/// Read the payload from stdin (`-`), a file path, or none. Any read failure yields an empty
/// payload — the mapping then falls back to matcher-less entries (never errors the hook).
fn read_payload(spec: Option<&str>) -> String {
    match spec {
        Some("-") => {
            let mut buf = String::new();
            let _ = std::io::stdin().read_to_string(&mut buf);
            buf
        }
        Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
        None => String::new(),
    }
}

/// Daemon cold-start dedup rule: a daemon starting mid-episode treats a `@agent_notified_at` at or
/// past the episode instant as already-notified and does not re-fire (strict predates: `<` fires,
/// equal does not). The daemonless path enforces the same invariant structurally; keeping the rule
/// here means the marker's meaning lives in one place.
///
/// `episode_at` is [`StampedState::episode_at`], not `@agent_since` alone: a second completion
/// inside one unchanged idle run moves `@agent_turn_at` and nothing else, and comparing against
/// `since` would dedup it away as the episode the first completion already notified.
pub fn episode_already_notified(notified_at: Option<u64>, episode_at: u64) -> bool {
    notified_at.is_some_and(|n| n >= episode_at)
}

#[cfg(test)]
mod tests {
    use super::permission::{permission_request_effect, PermReq};
    use super::*;
    use tma_core::manifest::Manifest;
    use tma_core::{AgentState, Detail};
    // The drift assertion cross-checks the relocated hook-event vocabulary.
    use crate::config::NotifyTrigger;
    use crate::manifests::{hook_events, CLAUDE_PARSER_COVERAGE};

    /// The default trigger set (blocked-only) and the opt-in set, for the notify tests.
    const BLOCKED_ONLY: &[NotifyTrigger] = &[NotifyTrigger::Blocked];
    const BLOCKED_AND_DONE: &[NotifyTrigger] = &[NotifyTrigger::Blocked, NotifyTrigger::Done];

    // ---- Captured Claude Code hook payloads: REAL captures via a logging hook in an isolated
    // throwaway config, paths redacted; the common envelope is verified on the wire. -------------

    const SESSION_START: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionStart","source":"startup"}"#;
    const USER_PROMPT_SUBMIT: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","prompt_id":"a77f1d82","permission_mode":"acceptEdits","hook_event_name":"UserPromptSubmit","prompt":"Run exactly this shell command and nothing else: echo hello-from-tma"}"#;
    const PRE_TOOL_USE: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","prompt_id":"a77f1d82","permission_mode":"acceptEdits","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hello-from-tma"},"tool_use_id":"toolu_01J6mvR1uRR2iito7xateEE6"}"#;
    const POST_TOOL_USE: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"PostToolUse","tool_name":"Bash","tool_response":{"stdout":"hello-from-tma","interrupted":false}}"#;
    const STOP: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Done."}"#;
    const SESSION_END: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionEnd","reason":"other"}"#;
    // Claude's SessionStart carries `model` as an OPTIONAL top-level string: present on a fresh
    // startup, omitted after `/clear` or a session restore (per the hooks docs — only SessionStart
    // can carry it, and it is not guaranteed). `SESSION_START` above is the absent-field case; this
    // is the model-bearing startup shape.
    const SESSION_START_MODEL: &str = r#"{"session_id":"65ced290-2a08-43de-aa80-d0b049d7ce30","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionStart","source":"startup","model":"claude-sonnet-5"}"#;

    // REAL captures (Claude Code 2.1.212, paths redacted): the permission Notification from driving
    // an isolated TUI to a live prompt, SubagentStop from a real `claude -p` with one subagent. The
    // discriminator is a dedicated `notification_type` field.
    const NOTIFICATION_PERMISSION: &str = r#"{"session_id":"53d5f99e-eb85-4727-b29d-b4b902688714","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","prompt_id":"38838ac5-1216-43b7-b9bc-fc9627d0b4f7","hook_event_name":"Notification","message":"Claude needs your permission","notification_type":"permission_prompt"}"#;
    // The idle-reminder variant: same envelope, different notification_type/message (still
    // constructed from the verified envelope — the idle Notification was not separately driven).
    const NOTIFICATION_IDLE: &str = r#"{"session_id":"53d5f99e-eb85-4727-b29d-b4b902688714","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"Notification","notification_type":"idle","message":"Claude is waiting for your input"}"#;
    const SUBAGENT_STOP_PAYLOAD: &str = r#"{"session_id":"0b03d2a0-d44c-4c51-8de3-57f2c043e737","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","prompt_id":"c92bf00f-9917-4ffc-b9c8-c5bb20721e81","permission_mode":"bypassPermissions","agent_id":"a40cffb8969ddc650","agent_type":"general-purpose","effort":{"level":"high"},"hook_event_name":"SubagentStop","stop_hook_active":false,"agent_transcript_path":"<AGENT_TRANSCRIPT>","last_assistant_message":"OK","background_tasks":[],"session_crons":[]}"#;

    fn claude() -> Manifest {
        Manifest::parse(
            include_str!("../../tma-core/manifests/claude.toml"),
            "claude.toml",
        )
        .unwrap()
    }

    fn stamp(state: AgentState, source: Provenance, now: u64) -> StampedState {
        StampedState {
            state,
            detail: None,
            source,
            evidence_at: now,
            since: now,
            turn_at: 0,
            stamped_at: now,
            attention: false,
            notified_at: None,
            hash: None,
            pid: 4242,
            session: Some("65ced290-2a08-43de-aa80-d0b049d7ce30".to_string()),
            subagents: vec![],
        }
    }

    // ---- session id extraction --------------------------------------------------

    #[test]
    fn extracts_session_id_from_real_payloads() {
        for p in [
            SESSION_START,
            USER_PROMPT_SUBMIT,
            PRE_TOOL_USE,
            STOP,
            SESSION_END,
        ] {
            assert_eq!(
                parse_session_id(p).as_deref(),
                Some("65ced290-2a08-43de-aa80-d0b049d7ce30"),
                "payload: {p}"
            );
        }
        assert_eq!(parse_session_id("{}"), None);
    }

    // ---- event mapping (against the bundled claude manifest) --------------------

    #[test]
    fn maps_lifecycle_and_state_events() {
        let m = claude();
        assert_eq!(
            map_event("SessionStart", SESSION_START, &m),
            Mapped::Register
        );
        assert_eq!(map_event("SessionEnd", SESSION_END, &m), Mapped::Deregister);
        assert_eq!(
            map_event("UserPromptSubmit", USER_PROMPT_SUBMIT, &m),
            Mapped::State {
                state: AgentState::Working,
                detail: None,
                turn_end: false,
            }
        );
        let working = Mapped::State {
            state: AgentState::Working,
            detail: None,
            turn_end: false,
        };
        assert_eq!(map_event("PreToolUse", PRE_TOOL_USE, &m), working);
        assert_eq!(map_event("PostToolUse", POST_TOOL_USE, &m), working);
        assert_eq!(
            map_event("Stop", STOP, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                // Claude's `Stop` is the event that MEANS a turn ended; the working-claiming
                // events above are not, so the flag is carried, never derived from the state.
                turn_end: true,
            }
        );
    }

    #[test]
    fn notification_matcher_distinguishes_permission_from_idle() {
        let m = claude();
        assert_eq!(
            map_event("Notification", NOTIFICATION_PERMISSION, &m),
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: false,
            }
        );
        // An idle-reminder Notification does not match the matcher ⇒ no state change.
        assert_eq!(
            map_event("Notification", NOTIFICATION_IDLE, &m),
            Mapped::Unmapped
        );
    }

    #[test]
    fn subagent_events_map_by_name() {
        let m = claude();
        assert_eq!(map_event("SubagentStart", "{}", &m), Mapped::SubagentStart);
        assert_eq!(
            map_event("SubagentStop", SUBAGENT_STOP_PAYLOAD, &m),
            Mapped::SubagentStop
        );
    }

    #[test]
    fn unknown_event_is_unmapped() {
        assert_eq!(map_event("PreCompact", "{}", &claude()), Mapped::Unmapped);
    }

    // ---- decision: transitions & attention -------------------------------------

    #[test]
    fn blocked_transition_sets_attention() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: false,
            },
            Some("65ced290-2a08-43de-aa80-d0b049d7ce30"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        match plan {
            EventPlan::Stamp {
                state,
                set_attention,
                notify,
                ..
            } => {
                assert_eq!(state, AgentState::Blocked);
                assert!(set_attention, "entering blocked is noteworthy");
                assert!(!notify, "notify opt-in off");
            }
            other => panic!("expected Stamp, got {other:?}"),
        }
    }

    #[test]
    fn working_to_idle_sets_attention() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: false,
            },
            None,
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert!(matches!(
            plan,
            EventPlan::Stamp {
                set_attention: true,
                ..
            }
        ));
    }

    /// The E1 regression. A second real completion with no observed `Working` in between: the
    /// pane is idle, the first marker was cleared (the user saw it), and the turn-end hook fires
    /// again. Nothing but `turn_end` can raise here — `prev == Working` is false, and the fold
    /// sees an idle→idle edge it cannot tell from a quiet idle pane.
    ///
    /// Reachable today on codex: `notify` (config.toml) needs no in-TUI trust while the
    /// working-claiming `hooks.json` events do, so an untrusted pane's every completion is this
    /// edge. Delete `turn_end` from the manifest entry, or fold it back into `prev == Working`,
    /// and this fails.
    #[test]
    fn a_second_turn_end_re_raises_a_cleared_done_marker() {
        let mut prev = stamp(AgentState::Idle, Provenance::Hook, 100);
        prev.attention = false; // the user saw the first completion; the marker came down
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            },
            None,
            Some(&prev),
            false,
            BLOCKED_ONLY,
            200,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    set_attention: true,
                    record_turn: true,
                    ..
                }
            ),
            "got {plan:?}"
        );
    }

    /// The control arm: the same idle→idle edge from an event the manifest does NOT call a turn
    /// end raises nothing. Without this the flag would be free to spread to any idle claim — an
    /// idle-reminder notification would put the marker straight back every time it fired, and the
    /// user could never clear it.
    #[test]
    fn an_idle_claim_that_is_not_a_turn_end_never_re_raises() {
        let prev = stamp(AgentState::Idle, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: false,
            },
            None,
            Some(&prev),
            false,
            BLOCKED_ONLY,
            200,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    set_attention: false,
                    record_turn: false,
                    ..
                }
            ),
            "got {plan:?}"
        );
    }

    /// One turn end reported twice (codex fires `Stop` from hooks.json and `notify` from
    /// config.toml, milliseconds apart) stays one raise and one recorded turn: the marker is still
    /// standing from the first, and an unacknowledged completion has nothing to add. This is the
    /// only thing separating the pair from two genuine turns, whose marker the user cleared.
    #[test]
    fn a_second_channel_reporting_the_same_turn_end_records_nothing() {
        let mut prev = stamp(AgentState::Idle, Provenance::Hook, 100);
        prev.attention = true; // raised microseconds ago by the first channel
        prev.turn_at = 100;
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            },
            None,
            Some(&prev),
            true, // notify opt-in: a second fire here would be a duplicate desktop notification
            BLOCKED_AND_DONE,
            101,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    set_attention: false,
                    record_turn: false,
                    notify: false,
                    ..
                }
            ),
            "got {plan:?}"
        );
    }

    /// A completion the ordinary way (`working` observed, marker down) records its turn too, so
    /// `wait --until done --since T` and the notify dedup read one basis whatever path raised the
    /// marker.
    #[test]
    fn an_ordinary_working_to_idle_completion_records_its_turn() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            },
            None,
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    set_attention: true,
                    record_turn: true,
                    ..
                }
            ),
            "got {plan:?}"
        );
    }

    /// A blocked event never records a turn, whatever the flag says: `@agent_turn_at` is the
    /// completion clock, and a blocked episode already re-arms the notify through `@agent_since`.
    #[test]
    fn a_blocked_event_records_no_turn() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: true,
            },
            None,
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    set_attention: true,
                    record_turn: false,
                    ..
                }
            ),
            "got {plan:?}"
        );
    }

    #[test]
    fn register_records_session_and_idle_without_attention() {
        let plan = decide(
            Mapped::Register,
            Some("sess-xyz"),
            None,
            false,
            BLOCKED_ONLY,
            100,
        );
        assert_eq!(
            plan,
            EventPlan::Stamp {
                state: AgentState::Idle,
                detail: None,
                set_attention: false,
                register_session: Some("sess-xyz".to_string()),
                notify: false,
                record_turn: false,
            }
        );
    }

    // ---- notify.from_event dedup ----------------------------------------

    #[test]
    fn notify_fires_once_on_blocked_transition() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Blocked,
                detail: None,
                turn_end: false,
            },
            None,
            Some(&prev),
            true, // opt-in
            BLOCKED_ONLY,
            110,
        );
        assert!(matches!(plan, EventPlan::Stamp { notify: true, .. }));
    }

    #[test]
    fn notify_suppressed_when_already_blocked() {
        // Second identical blocked event: prev already blocked ⇒ not a transition ⇒ no fire,
        // no attention re-set (dedup: "no double notify-marker bump").
        let mut prev = stamp(AgentState::Blocked, Provenance::Hook, 100);
        prev.notified_at = Some(100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Blocked,
                detail: None,
                turn_end: false,
            },
            None,
            Some(&prev),
            true,
            BLOCKED_ONLY,
            110,
        );
        assert!(matches!(
            plan,
            EventPlan::Stamp {
                notify: false,
                set_attention: false,
                ..
            }
        ));
    }

    #[test]
    fn done_notify_fires_only_when_opted_in() {
        // A working→idle completion ("done"). With the default blocked-only set it does NOT
        // notify (existing users unaffected); with `["blocked","done"]` it fires.
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let idle = Mapped::State {
            state: AgentState::Idle,
            detail: None,
            turn_end: false,
        };
        let default_set = decide(idle.clone(), None, Some(&prev), true, BLOCKED_ONLY, 110);
        assert!(
            matches!(
                default_set,
                EventPlan::Stamp {
                    set_attention: true,
                    notify: false,
                    ..
                }
            ),
            "done is noteworthy (attention) but not in the default trigger set"
        );
        let opted_in = decide(idle, None, Some(&prev), true, BLOCKED_AND_DONE, 110);
        assert!(matches!(opted_in, EventPlan::Stamp { notify: true, .. }));
    }

    #[test]
    fn done_notify_suppressed_for_plain_idle() {
        // idle→idle (not a working→idle completion): not noteworthy ⇒ no done fire even opted in.
        let prev = stamp(AgentState::Idle, Provenance::Hook, 100);
        let plan = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: false,
            },
            None,
            Some(&prev),
            true,
            BLOCKED_AND_DONE,
            110,
        );
        assert!(matches!(
            plan,
            EventPlan::Stamp {
                notify: false,
                set_attention: false,
                ..
            }
        ));
    }

    // ---- subagent guard -----------------------------------------------------

    #[test]
    fn subagent_start_appends_foreign_session() {
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            Mapped::SubagentStart,
            Some("sub-1"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert_eq!(plan, EventPlan::Subagents(vec!["sub-1".to_string()]));
    }

    #[test]
    fn subagent_stop_removes_session() {
        let mut prev = stamp(AgentState::Working, Provenance::Hook, 100);
        prev.subagents = vec!["sub-1".to_string(), "sub-2".to_string()];
        let plan = decide(
            Mapped::SubagentStop,
            Some("sub-1"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert_eq!(plan, EventPlan::Subagents(vec!["sub-2".to_string()]));
    }

    #[test]
    fn foreign_session_event_ignored_while_subagents_live() {
        // A subagent (foreign session) fires UserPromptSubmit while it is live: the parent
        // pane's state must not change.
        let mut prev = stamp(AgentState::Blocked, Provenance::Hook, 100);
        prev.subagents = vec!["sub-1".to_string()];
        let plan = decide(
            Mapped::State {
                state: AgentState::Working,
                detail: None,
                turn_end: false,
            },
            Some("sub-1"), // the subagent's own session, ≠ @agent_session
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert_eq!(
            plan,
            EventPlan::Ignore,
            "subagent must not clobber parent state"
        );
    }

    #[test]
    fn owning_session_event_honored_while_subagents_live() {
        let mut prev = stamp(AgentState::Working, Provenance::Hook, 100);
        prev.subagents = vec!["sub-1".to_string()];
        let plan = decide(
            Mapped::State {
                state: AgentState::Blocked,
                detail: None,
                turn_end: false,
            },
            Some("65ced290-2a08-43de-aa80-d0b049d7ce30"), // the owner
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert!(
            matches!(
                plan,
                EventPlan::Stamp {
                    state: AgentState::Blocked,
                    ..
                }
            ),
            "the owning session still drives state"
        );
    }

    #[test]
    fn subagent_stop_cannot_stamp_an_unowned_pane_idle() {
        // Observed-only pane (no `@agent_session`) whose FIRST hook is a SubagentStart: the
        // bookkeeping lands, but the pane still has no owner. The subagent's own Stop must not be
        // read as the parent finishing — that is a premature done glyph and a stray notification.
        let mut prev = stamp(AgentState::Working, Provenance::Capture, 100);
        prev.session = None;

        let start = decide(
            Mapped::SubagentStart,
            Some("sub-1"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert_eq!(start, EventPlan::Subagents(vec!["sub-1".to_string()]));

        let EventPlan::Subagents(live) = start else {
            unreachable!("SubagentStart plans bookkeeping")
        };
        prev.subagents = live;
        let stop = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: false,
            },
            Some("sub-1"),
            Some(&prev),
            true,
            BLOCKED_AND_DONE,
            120,
        );
        assert_eq!(
            stop,
            EventPlan::Ignore,
            "an unattributable event must not flip the parent while subagents are live"
        );

        // Once the set empties, the pane takes hook state again (and records the owner).
        prev.subagents.clear();
        let after = decide(
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: false,
            },
            Some("sub-1"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            130,
        );
        assert!(matches!(
            after,
            EventPlan::Stamp {
                state: AgentState::Idle,
                ..
            }
        ));
    }

    /// The distinction the daemon's delivery ack rests on: "these manifests map nothing here" is
    /// NOT the same as "I decided to write nothing". Only the first may be re-applied by the peer.
    #[test]
    fn unmapped_is_distinguishable_from_a_no_write_verdict() {
        // An idle-reminder Notification maps to nothing ⇒ Unmapped, so the daemon NAKs and the
        // client (whose manifests may map it) applies the event itself.
        let unmapped = decide(Mapped::Unmapped, None, None, false, BLOCKED_ONLY, 100);
        assert_eq!(unmapped, EventPlan::Unmapped);
        assert!(!unmapped.is_verdict());
        assert!(!unmapped.commits());

        // The subagent ownership guard refusing a foreign session IS a decision: it must be acked,
        // or the client would double-write exactly the state the daemon correctly refused.
        let mut prev = stamp(AgentState::Blocked, Provenance::Hook, 100);
        prev.subagents = vec!["sub-1".to_string()];
        let guarded = decide(
            Mapped::State {
                state: AgentState::Working,
                detail: None,
                turn_end: false,
            },
            Some("sub-1"),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert_eq!(guarded, EventPlan::Ignore);
        assert!(guarded.is_verdict(), "a refusal is still a verdict");
        assert!(!guarded.commits(), "but it writes nothing");

        // A plan that writes is both.
        let stamped = decide(Mapped::Register, Some("s"), None, false, BLOCKED_ONLY, 100);
        assert!(stamped.is_verdict() && stamped.commits());
    }

    // ---- daemon cold-start rule -----------------------------

    #[test]
    fn cold_start_already_notified_is_strict_predate() {
        // notified_at < since ⇒ fire (not yet notified this episode).
        assert!(!episode_already_notified(Some(90), 100));
        // equal ⇒ already notified (strict predates).
        assert!(episode_already_notified(Some(100), 100));
        assert!(episode_already_notified(Some(120), 100));
        // no marker ⇒ never notified.
        assert!(!episode_already_notified(None, 100));
    }

    // ---- OpenCode: the plugin emits its own JSON envelope (`session_id` snake + an event field);
    // the `ses_…` ids are real captures from 1.17.15, resolved via the bundled manifest, no OC code.

    const OC_SESSION_START: &str = r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X"}"#;
    const OC_USER_PROMPT: &str = r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X"}"#;
    const OC_STOP: &str = r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X"}"#;
    const OC_PERMISSION: &str =
        r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X","permission":"bash"}"#;

    fn opencode() -> Manifest {
        Manifest::parse(
            include_str!("../../tma-core/manifests/opencode.toml"),
            "opencode.toml",
        )
        .unwrap()
    }

    #[test]
    fn opencode_events_map_via_the_generic_manifest_path() {
        let m = opencode();
        assert_eq!(
            map_event("session-start", OC_SESSION_START, &m),
            Mapped::Register
        );
        assert_eq!(
            map_event("user-prompt-submit", OC_USER_PROMPT, &m),
            Mapped::State {
                state: AgentState::Working,
                detail: None,
                turn_end: false,
            }
        );
        assert_eq!(
            map_event("stop", OC_STOP, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
        assert_eq!(
            map_event("permission-required", OC_PERMISSION, &m),
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: false,
            }
        );
        // OpenCode has no session-end event, and emits no subagent hooks — an unknown token is
        // simply unmapped (the subagent guard stays inert because @agent_subagents never populates).
        assert_eq!(map_event("session-end", "{}", &m), Mapped::Unmapped);
    }

    #[test]
    fn opencode_session_id_parses_from_the_plugin_envelope() {
        for p in [OC_SESSION_START, OC_PERMISSION] {
            assert_eq!(
                parse_session_id(p).as_deref(),
                Some("ses_0789d5f61ffeW6yCmb3x7wLH1X"),
                "payload: {p}"
            );
        }
    }

    // ---- OpenCode API channel: the plugin forwards `api_endpoint` at session-start and
    // `request_id` on the permission edge (envelope shapes match the shipped OpenCode plugin).

    const OC_SESSION_START_API: &str =
        r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X","api_endpoint":"http://127.0.0.1:4096"}"#;
    const OC_PERMISSION_API: &str = r#"{"session_id":"ses_0789d5f61ffeW6yCmb3x7wLH1X","permission":"bash","request_id":"per_9Xy"}"#;

    #[test]
    fn opencode_api_endpoint_and_request_id_parse() {
        assert_eq!(
            json_string_field(OC_SESSION_START_API, "api_endpoint").as_deref(),
            Some("http://127.0.0.1:4096")
        );
        assert_eq!(
            json_string_field(OC_PERMISSION_API, "request_id").as_deref(),
            Some("per_9Xy")
        );
        // Absent fields (an older plugin's envelope) parse to None, not a stamp.
        assert!(json_string_field(OC_SESSION_START, "api_endpoint").is_none());
        assert!(json_string_field(OC_PERMISSION, "request_id").is_none());
    }

    /// A `blocked` stamp carrying a request id sets it; a stamp without one leaves it alone; a
    /// working/idle stamp and a `permission.replied` edge clear it.
    #[test]
    fn permission_request_effect_sets_and_clears() {
        let blocked = EventPlan::Stamp {
            state: AgentState::Blocked,
            detail: Some(Detail::new("permission")),
            set_attention: true,
            register_session: None,
            notify: false,
            record_turn: false,
        };
        assert_eq!(
            permission_request_effect(
                "permission-required",
                &blocked,
                None,
                None,
                OC_PERMISSION_API
            ),
            PermReq::Set("per_9Xy".to_string())
        );
        // Blocked but no request id in the payload ⇒ leave the option untouched.
        assert_eq!(
            permission_request_effect("permission-required", &blocked, None, None, OC_PERMISSION),
            PermReq::None
        );
        let working = EventPlan::Stamp {
            state: AgentState::Working,
            detail: None,
            set_attention: false,
            register_session: None,
            notify: false,
            record_turn: false,
        };
        assert_eq!(
            permission_request_effect("user-prompt-submit", &working, None, None, "{}"),
            PermReq::Clear
        );
        // permission.replied clears regardless of plan (it carries no state claim, so it maps to
        // Unmapped) — the one case where an unmapped event still writes.
        assert_eq!(
            permission_request_effect(PERMISSION_REPLIED, &EventPlan::Unmapped, None, None, "{}"),
            PermReq::Clear
        );
    }

    /// The `permission.replied` clear respects ownership: a foreign session must not clear the
    /// owner's pending request.
    #[test]
    fn permission_replied_clear_is_ownership_gated() {
        let owner = "ses_owner";
        assert_eq!(
            permission_request_effect(
                PERMISSION_REPLIED,
                &EventPlan::Unmapped,
                Some(owner),
                Some("ses_sub"),
                "{}"
            ),
            PermReq::None,
            "a foreign session does not clear the owner's request"
        );
        assert_eq!(
            permission_request_effect(
                PERMISSION_REPLIED,
                &EventPlan::Unmapped,
                Some(owner),
                Some(owner),
                "{}"
            ),
            PermReq::Clear
        );
    }

    // ---- Codex notify: no per-event hook block; the `notify` program delivers one
    // `agent-turn-complete` JSON as a trailing argv arg. Below is a REAL argv fire, paths trimmed.

    const CODEX_TURN_COMPLETE: &str = r#"{"type":"agent-turn-complete","thread-id":"019f99c3-7c57-7963-98e9-f496a7978257","turn-id":"019f99c4-38c9-7f63-901a-d9910886b99a","cwd":"<CWD>","client":"codex-tui","input-messages":["run the tests"],"last-assistant-message":"All tests pass."}"#;

    fn codex() -> Manifest {
        Manifest::parse(
            include_str!("../../tma-core/manifests/codex.toml"),
            "codex.toml",
        )
        .unwrap()
    }

    #[test]
    fn codex_turn_complete_maps_to_idle() {
        let m = codex();
        // The matcher fires only on the agent-turn-complete type ⇒ idle.
        assert_eq!(
            map_event("notify", CODEX_TURN_COMPLETE, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
    }

    #[test]
    fn codex_non_turn_complete_notify_is_unmapped() {
        let m = codex();
        // Any other notify type misses the matcher and has no fallback ⇒ no state change.
        assert_eq!(
            map_event("notify", r#"{"type":"some-future-notification"}"#, &m),
            Mapped::Unmapped
        );
        // An event codex does NOT map (e.g. PreCompact) is still unmapped.
        assert_eq!(map_event("PreCompact", "{}", &m), Mapped::Unmapped);
    }

    // ---- Codex hooks.json events: captured verbatim from live Codex 0.145.0 (paths redacted).
    // Delivery is one JSON object on STDIN carrying a real `session_id`, so registration is live.

    const CODEX_SESSION_START: &str = r#"{"session_id":"019f8aac-ff01-75d0-9bb1-7f0eab253ce7","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionStart","model":"gpt-5.6-sol","permission_mode":"bypassPermissions","source":"startup"}"#;
    const CODEX_USER_PROMPT_SUBMIT: &str = r#"{"session_id":"019f8aac-ff01-75d0-9bb1-7f0eab253ce7","turn_id":"019f8aac-ff28-7152-9869-f1ff031df848","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"UserPromptSubmit","model":"gpt-5.6-sol","permission_mode":"bypassPermissions","prompt":"Reply with exactly: OK"}"#;
    const CODEX_SESSION_END: &str = r#"{"session_id":"019f8aac-ff01-75d0-9bb1-7f0eab253ce7","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionEnd","reason":"other"}"#;
    // Completed-turn captures — paths redacted.
    const CODEX_PRE_TOOL_USE: &str = r#"{"session_id":"019f99c3-7c57-7963-98e9-f496a7978257","turn_id":"019f99c4-38c9-7f63-901a-d9910886b99a","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"PreToolUse","model":"gpt-5.6-terra","permission_mode":"default","tool_name":"Bash","tool_input":{"command":"echo hello-from-tma"},"tool_use_id":"exec-4db29871-277d-4e22-a534-937704bd3bf6"}"#;
    const CODEX_POST_TOOL_USE: &str = r#"{"session_id":"019f99c3-7c57-7963-98e9-f496a7978257","turn_id":"019f99c4-38c9-7f63-901a-d9910886b99a","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"PostToolUse","model":"gpt-5.6-terra","permission_mode":"default","tool_name":"Bash","tool_input":{"command":"echo hello-from-tma"},"tool_response":"hello-from-tma\n","tool_use_id":"exec-4db29871-277d-4e22-a534-937704bd3bf6"}"#;
    const CODEX_PERMISSION_REQUEST: &str = r#"{"session_id":"019f99c3-7c57-7963-98e9-f496a7978257","turn_id":"019f99c4-bf4f-7b20-9744-526b9e1d65a8","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"PermissionRequest","model":"gpt-5.6-terra","permission_mode":"default","tool_name":"Bash","tool_input":{"command":"touch /tmp/tma-approval-test.txt"}}"#;
    const CODEX_STOP: &str = r#"{"session_id":"019f99c3-7c57-7963-98e9-f496a7978257","turn_id":"019f99c4-38c9-7f63-901a-d9910886b99a","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"Stop","model":"gpt-5.6-terra","permission_mode":"default","stop_hook_active":false,"last_assistant_message":"hello-from-tma"}"#;

    #[test]
    fn codex_session_start_registers_with_session_id() {
        let m = codex();
        assert_eq!(
            map_event("SessionStart", CODEX_SESSION_START, &m),
            Mapped::Register
        );
        // Unlike notify (thread-id only), hooks.json payloads carry a real session_id.
        assert_eq!(
            parse_session_id(CODEX_SESSION_START).as_deref(),
            Some("019f8aac-ff01-75d0-9bb1-7f0eab253ce7")
        );
    }

    #[test]
    fn codex_user_prompt_submit_maps_to_working() {
        // Fires on submit, BEFORE the model responds — verified live even on a turn that
        // then failed on quota, so working lands regardless of the turn's fate.
        assert_eq!(
            map_event("UserPromptSubmit", CODEX_USER_PROMPT_SUBMIT, &codex()),
            Mapped::State {
                state: AgentState::Working,
                detail: None,
                turn_end: false,
            }
        );
    }

    #[test]
    fn codex_session_end_deregisters() {
        // Verified to fire even when the turn errors out (reason "other").
        assert_eq!(
            map_event("SessionEnd", CODEX_SESSION_END, &codex()),
            Mapped::Deregister
        );
    }

    #[test]
    fn codex_tool_events_map_to_working() {
        // PreToolUse / PostToolUse fire mid-turn (real `Bash` tool payloads) ⇒ working.
        let m = codex();
        let working = Mapped::State {
            state: AgentState::Working,
            detail: None,
            turn_end: false,
        };
        assert_eq!(map_event("PreToolUse", CODEX_PRE_TOOL_USE, &m), working);
        assert_eq!(map_event("PostToolUse", CODEX_POST_TOOL_USE, &m), working);
    }

    #[test]
    fn codex_permission_request_maps_to_blocked() {
        // The event that hook-covers blocked — fires exactly when the approval prompt appears,
        // carrying the pending tool. No matcher (codex sends it only on need).
        assert_eq!(
            map_event("PermissionRequest", CODEX_PERMISSION_REQUEST, &codex()),
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: false,
            }
        );
    }

    #[test]
    fn codex_stop_maps_to_idle() {
        // Stop fires on turn completion (real `last_assistant_message`) ⇒ idle — the hooks.json
        // twin of the notify agent-turn-complete idle.
        assert_eq!(
            map_event("Stop", CODEX_STOP, &codex()),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
    }

    #[test]
    fn codex_notify_has_no_session_id() {
        // Codex's notify carries `thread-id`, not `session_id`, so the subagent guard is inert —
        // `parse_session_id` finds nothing and the event is attributed to the pane.
        assert_eq!(parse_session_id(CODEX_TURN_COMPLETE), None);
    }

    #[test]
    fn codex_turn_complete_decides_idle_stamp() {
        // A turn-complete on a working pane resolves to an idle stamp with attention (a
        // working→idle completion is noteworthy), no session recorded.
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            map_event("notify", CODEX_TURN_COMPLETE, &codex()),
            parse_session_id(CODEX_TURN_COMPLETE).as_deref(),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        match plan {
            EventPlan::Stamp {
                state,
                set_attention,
                register_session,
                ..
            } => {
                assert_eq!(state, AgentState::Idle);
                assert!(set_attention, "working→idle is a noteworthy completion");
                assert_eq!(register_session, None, "codex notify has no session id");
            }
            other => panic!("expected Stamp, got {other:?}"),
        }
    }

    // ---- Gemini: REAL fires from 0.46.0 (paths redacted, AfterTool `returnDisplay` trimmed). Native
    // event names resolved by the generic `map_event`; `GEM_NOTIFICATION` hook-covers blocked. ------

    const GEM_SESSION_START: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionStart","timestamp":"2026-07-25T15:06:55.608Z","source":"startup"}"#;
    const GEM_BEFORE_AGENT: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"BeforeAgent","timestamp":"2026-07-25T15:07:10.394Z","prompt":"Run the shell command: echo hello-from-tma"}"#;
    const GEM_BEFORE_TOOL: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"BeforeTool","timestamp":"2026-07-25T15:07:15.436Z","tool_name":"run_shell_command","tool_input":{"command":"echo hello-from-tma","description":"Execute echo command to print hello-from-tma."}}"#;
    const GEM_AFTER_TOOL: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"AfterTool","timestamp":"2026-07-25T15:07:15.479Z","tool_name":"run_shell_command","tool_input":{"command":"echo hello-from-tma","description":"Execute echo command to print hello-from-tma."},"tool_response":{"llmContent":"Output: hello-from-tma\nProcess Group PGID: 77665","returnDisplay":"<DISPLAY>"}}"#;
    const GEM_AFTER_AGENT: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"AfterAgent","timestamp":"2026-07-25T15:07:16.804Z","prompt":"Run the shell command: echo hello-from-tma","prompt_response":"  \nhello-from-tma ","stop_hook_active":false}"#;
    const GEM_SESSION_END: &str = r#"{"session_id":"7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"SessionEnd","timestamp":"2026-07-25T15:08:19.698Z","reason":"exit"}"#;
    // The real Notification approval payload, captured when the permission prompt appeared (fired
    // before it was answered). `notification_type` "ToolPermission" is what the matcher gates on.
    const GEM_NOTIFICATION: &str = r#"{"session_id":"d375f751-c0af-4804-894c-b6ccc4616cf9","transcript_path":"<TRANSCRIPT>","cwd":"<CWD>","hook_event_name":"Notification","timestamp":"2026-07-26T20:04:18.311Z","notification_type":"ToolPermission","message":"Tool Confirm Shell Command requires execution","details":{"type":"exec","title":"Confirm Shell Command","command":"rm -rf /tmp/h19_probe_dir","rootCommand":"rm"}}"#;

    fn gemini() -> Manifest {
        Manifest::parse(
            include_str!("../../tma-core/manifests/gemini.toml"),
            "gemini.toml",
        )
        .unwrap()
    }

    #[test]
    fn gemini_events_map_via_the_generic_manifest_path() {
        let m = gemini();
        // SessionStart registers the pane (the identity rescue), the source of gemini's whole
        // coverage, since its `node` comm gives no passive identity.
        assert_eq!(
            map_event("SessionStart", GEM_SESSION_START, &m),
            Mapped::Register
        );
        assert_eq!(
            map_event("SessionEnd", GEM_SESSION_END, &m),
            Mapped::Deregister
        );
        let working = Mapped::State {
            state: AgentState::Working,
            detail: None,
            turn_end: false,
        };
        assert_eq!(map_event("BeforeAgent", GEM_BEFORE_AGENT, &m), working);
        assert_eq!(map_event("BeforeTool", GEM_BEFORE_TOOL, &m), working);
        assert_eq!(map_event("AfterTool", GEM_AFTER_TOOL, &m), working);
        // AfterAgent is the turn-complete event ⇒ idle (fires last in a turn).
        assert_eq!(
            map_event("AfterAgent", GEM_AFTER_AGENT, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
        // BeforeModel/AfterModel are deliberately UNMAPPED (multi-fire, race the final idle).
        assert_eq!(map_event("BeforeModel", "{}", &m), Mapped::Unmapped);
        assert_eq!(map_event("AfterModel", "{}", &m), Mapped::Unmapped);
        // Notification with notification_type "ToolPermission" ⇒ blocked/permission (the
        // approval-prompt hook, fired before the prompt is answered).
        assert_eq!(
            map_event("Notification", GEM_NOTIFICATION, &m),
            Mapped::State {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
                turn_end: false,
            }
        );
        // A non-permission Notification (matcher miss) must NOT claim blocked — no other
        // notification_type exists in 0.46.0, but the matcher keeps a future one from false-blocking.
        assert_eq!(
            map_event(
                "Notification",
                r#"{"notification_type":"IdleReminder"}"#,
                &m
            ),
            Mapped::Unmapped
        );
    }

    #[test]
    fn gemini_session_id_parses_from_real_payloads() {
        for p in [
            GEM_SESSION_START,
            GEM_BEFORE_AGENT,
            GEM_AFTER_TOOL,
            GEM_AFTER_AGENT,
            GEM_SESSION_END,
        ] {
            assert_eq!(
                parse_session_id(p).as_deref(),
                Some("7ae9d79d-9b49-45f4-bdb1-a5b2a6e90e0e"),
                "payload: {p}"
            );
        }
    }

    #[test]
    fn gemini_after_agent_decides_idle_with_attention() {
        // A turn-complete (AfterAgent) on a working pane resolves to an idle stamp with attention
        // (working→idle is a noteworthy completion), recording the session on a first hook.
        let prev = stamp(AgentState::Working, Provenance::Hook, 100);
        let plan = decide(
            map_event("AfterAgent", GEM_AFTER_AGENT, &gemini()),
            parse_session_id(GEM_AFTER_AGENT).as_deref(),
            Some(&prev),
            false,
            BLOCKED_ONLY,
            110,
        );
        assert!(matches!(
            plan,
            EventPlan::Stamp {
                state: AgentState::Idle,
                set_attention: true,
                ..
            }
        ));
    }

    // ---- Cursor: REAL captures from 2026.07.23-e383d2b (paths/email redacted, per-turn fields
    // trimmed). Lowercase native events resolved by `map_event`; cursor scopes `session_id` per
    // conversation, so a fresh prompt's id differs from sessionStart's (fine, registration is idempotent).

    const CUR_SESSION_START: &str = r#"{"conversation_id":"491550db-0a81-4cd0-a248-89254960295e","generation_id":"491550db-0a81-4cd0-a248-89254960295e","model":"default","is_background_agent":false,"session_id":"491550db-0a81-4cd0-a248-89254960295e","hook_event_name":"sessionStart","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":null}"#;
    const CUR_BEFORE_SUBMIT_PROMPT: &str = r#"{"conversation_id":"78139b05-16d8-4190-b888-55f2eec06d47","generation_id":"bd69fa74-23c8-41e5-9e37-b3c131c4b59c","model":"default","prompt":"Run this exact shell command and show output: uname -a","attachments":[],"session_id":"78139b05-16d8-4190-b888-55f2eec06d47","hook_event_name":"beforeSubmitPrompt","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":null}"#;
    const CUR_PRE_TOOL_USE: &str = r#"{"conversation_id":"491550db-0a81-4cd0-a248-89254960295e","generation_id":"491550db-0a81-4cd0-a248-89254960295e","model":"default","tool_name":"Shell","tool_input":{"command":"echo HELLO_FROM_CURSOR"},"tool_use_id":"fc8581b3-2f76-44e8-a8de-51f4d1d2929d","cwd":"","session_id":"491550db-0a81-4cd0-a248-89254960295e","hook_event_name":"preToolUse","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":null}"#;
    const CUR_POST_TOOL_USE: &str = r#"{"conversation_id":"491550db-0a81-4cd0-a248-89254960295e","generation_id":"491550db-0a81-4cd0-a248-89254960295e","model":"default","tool_name":"Shell","tool_output":"<OUTPUT>","duration":12,"session_id":"491550db-0a81-4cd0-a248-89254960295e","hook_event_name":"postToolUse","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":"<TRANSCRIPT>"}"#;
    const CUR_STOP: &str = r#"{"conversation_id":"78139b05-16d8-4190-b888-55f2eec06d47","generation_id":"bd69fa74-23c8-41e5-9e37-b3c131c4b59c","model":"default","status":"completed","loop_count":0,"input_tokens":40450,"output_tokens":754,"session_id":"78139b05-16d8-4190-b888-55f2eec06d47","hook_event_name":"stop","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":"<TRANSCRIPT>"}"#;
    // REAL capture (2026-07-29): a `cat` of a missing file exited non-zero, firing
    // postToolUseFailure with `failure_type":"error"` and `is_interrupt":false`; the agent then
    // recovered and answered, so this is a working continuation, gated on the non-interrupt flag.
    const CUR_POST_TOOL_USE_FAILURE: &str = r#"{"conversation_id":"9dead191-6e5a-4cf4-a288-fdd38b3ee42c","generation_id":"9dead191-6e5a-4cf4-a288-fdd38b3ee42c","model":"default","tool_name":"Shell","tool_input":{"command":"cat /nonexistent-file-xyz-123","cwd":"","timeout":30000},"error_message":"cat: /nonexistent-file-xyz-123: No such file or directory","failure_type":"error","duration":347.259,"tool_use_id":"34ae8a0e-595a-49bf-a092-594d3bdfa237","is_interrupt":false,"cwd":"","session_id":"9dead191-6e5a-4cf4-a288-fdd38b3ee42c","hook_event_name":"postToolUseFailure","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":"<TRANSCRIPT>"}"#;
    const CUR_SESSION_END: &str = r#"{"conversation_id":"491550db-0a81-4cd0-a248-89254960295e","generation_id":"491550db-0a81-4cd0-a248-89254960295e","model":"default","reason":"completed","duration_ms":17851,"is_background_agent":false,"final_status":"completed","session_id":"491550db-0a81-4cd0-a248-89254960295e","hook_event_name":"sessionEnd","cursor_version":"2026.07.23-e383d2b","workspace_roots":["<CWD>"],"user_email":"<EMAIL>","transcript_path":"<TRANSCRIPT>"}"#;

    fn cursor() -> Manifest {
        Manifest::parse(
            include_str!("../../tma-core/manifests/cursor.toml"),
            "cursor.toml",
        )
        .unwrap()
    }

    #[test]
    fn cursor_events_map_via_the_generic_manifest_path() {
        let m = cursor();
        assert_eq!(
            map_event("sessionStart", CUR_SESSION_START, &m),
            Mapped::Register
        );
        assert_eq!(
            map_event("sessionEnd", CUR_SESSION_END, &m),
            Mapped::Deregister
        );
        let working = Mapped::State {
            state: AgentState::Working,
            detail: None,
            turn_end: false,
        };
        assert_eq!(
            map_event("beforeSubmitPrompt", CUR_BEFORE_SUBMIT_PROMPT, &m),
            working
        );
        assert_eq!(map_event("preToolUse", CUR_PRE_TOOL_USE, &m), working);
        assert_eq!(map_event("postToolUse", CUR_POST_TOOL_USE, &m), working);
        // postToolUseFailure: the real non-interrupt failure is a working continuation.
        assert_eq!(
            map_event("postToolUseFailure", CUR_POST_TOOL_USE_FAILURE, &m),
            working
        );
        // The matcher gates on the non-interrupt flag: a user-abort variant (is_interrupt true —
        // not captured, so a minimal probe) must NOT false-stamp working, it falls through unmapped.
        assert_eq!(
            map_event(
                "postToolUseFailure",
                r#"{"hook_event_name":"postToolUseFailure","is_interrupt":true}"#,
                &m
            ),
            Mapped::Unmapped
        );
        // stop is the turn-complete event ⇒ idle.
        assert_eq!(
            map_event("stop", CUR_STOP, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
        // The observer-only shell/thought events are deliberately UNMAPPED (they add no coverage
        // and beforeShellExecution is not a blocked signal — verified live).
        assert_eq!(
            map_event("beforeShellExecution", "{}", &m),
            Mapped::Unmapped
        );
        assert_eq!(map_event("afterAgentThought", "{}", &m), Mapped::Unmapped);
    }

    #[test]
    fn cursor_session_id_parses_from_real_payloads() {
        assert_eq!(
            parse_session_id(CUR_SESSION_START).as_deref(),
            Some("491550db-0a81-4cd0-a248-89254960295e")
        );
        // A per-conversation id on a later prompt (differs from sessionStart's — see the note).
        assert_eq!(
            parse_session_id(CUR_STOP).as_deref(),
            Some("78139b05-16d8-4190-b888-55f2eec06d47")
        );
    }

    // ---- pi: REAL captures from 0.82.1, redacted/trimmed. KEY FINDING: pi's events carry NO session
    // id, so the tma extension forwards `{session_id}` (`PI_FORWARDED`); the raw constants prove it.
    const PI_RAW_SESSION_START: &str = r#"{"type":"session_start","reason":"startup"}"#;
    const PI_RAW_BEFORE_AGENT_START: &str = r#"{"type":"before_agent_start","prompt":"<PROMPT>","systemPrompt":"<SYSTEM_PROMPT>","systemPromptOptions":{"cwd":"<CWD>"}}"#;
    const PI_RAW_TOOL_EXECUTION_START: &str = r#"{"type":"tool_execution_start","toolCallId":"bash_0","toolName":"bash","args":{"command":"echo hello-from-tma"}}"#;
    const PI_RAW_AGENT_SETTLED: &str = r#"{"type":"agent_settled"}"#;
    const PI_RAW_SESSION_SHUTDOWN: &str = r#"{"type":"session_shutdown","reason":"quit"}"#;
    // What the extension actually shells out (verified live: `tma-hook pi <event>` with this on
    // stdin, one real forwarded session id).
    const PI_FORWARDED: &str = r#"{"session_id":"019f9ec5-95dc-70fd-a2fc-01e9bb1b2c37"}"#;

    fn pi() -> Manifest {
        Manifest::parse(include_str!("../../tma-core/manifests/pi.toml"), "pi.toml").unwrap()
    }

    #[test]
    fn pi_events_map_via_the_generic_manifest_path() {
        let m = pi();
        // The state mapping keys on the event NAME (pi's claims are unconditional, no matcher), so
        // the forwarded `{session_id}` payload is what reaches the daemon.
        assert_eq!(
            map_event("session_start", PI_FORWARDED, &m),
            Mapped::Register
        );
        assert_eq!(
            map_event("session_shutdown", PI_FORWARDED, &m),
            Mapped::Deregister
        );
        let working = Mapped::State {
            state: AgentState::Working,
            detail: None,
            turn_end: false,
        };
        assert_eq!(map_event("before_agent_start", PI_FORWARDED, &m), working);
        assert_eq!(map_event("tool_execution_start", PI_FORWARDED, &m), working);
        assert_eq!(
            map_event("agent_settled", PI_FORWARDED, &m),
            Mapped::State {
                state: AgentState::Idle,
                detail: None,
                turn_end: true,
            }
        );
        // The deliberately-unmapped events add no coverage (redundant with the five above).
        assert_eq!(map_event("agent_start", PI_FORWARDED, &m), Mapped::Unmapped);
        assert_eq!(
            map_event("tool_execution_end", PI_FORWARDED, &m),
            Mapped::Unmapped
        );
        assert_eq!(map_event("turn_start", PI_FORWARDED, &m), Mapped::Unmapped);
    }

    #[test]
    fn pi_session_id_is_injected_by_the_extension_not_carried_by_events() {
        // The forwarded payload carries the injected id (what the daemon registers on).
        assert_eq!(
            parse_session_id(PI_FORWARDED).as_deref(),
            Some("019f9ec5-95dc-70fd-a2fc-01e9bb1b2c37")
        );
        // The raw pi events carry NO session id — the finding that forces the extension to inject
        // it. (If pi ever adds one, this documents the day it changed.)
        for raw in [
            PI_RAW_SESSION_START,
            PI_RAW_BEFORE_AGENT_START,
            PI_RAW_TOOL_EXECUTION_START,
            PI_RAW_AGENT_SETTLED,
            PI_RAW_SESSION_SHUTDOWN,
        ] {
            assert_eq!(
                parse_session_id(raw),
                None,
                "pi raw event unexpectedly carries a session id: {raw}"
            );
        }
    }

    // ---- model identity: the model name in registration-class payloads ---------------------------

    #[test]
    fn registration_payloads_carry_model_per_agent() {
        // Claude SessionStart (startup), Codex SessionStart, and Cursor sessionStart each carry
        // `model` as a plain top-level string — the Codex and Cursor consts are real captures.
        assert_eq!(
            tma_core::hook_payload_model(SESSION_START_MODEL).as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            tma_core::hook_payload_model(CODEX_SESSION_START).as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            tma_core::hook_payload_model(CUR_SESSION_START).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn payloads_without_model_yield_none() {
        // Claude omits `model` after /clear or restore (the captured SESSION_START), and no other
        // Claude event carries it (Stop); Gemini/OpenCode/pi registration payloads never do.
        for p in [
            SESSION_START,
            STOP,
            GEM_SESSION_START,
            OC_SESSION_START,
            PI_FORWARDED,
        ] {
            assert_eq!(tma_core::hook_payload_model(p), None, "payload: {p}");
        }
    }

    // ---- single source of truth --------------------------------------------

    #[test]
    fn hookless_manifest_wires_no_events() {
        // A manifest with no [hooks] block must yield an empty event set so the installer refuses
        // rather than wiring the two subagent events into a config the agent never reads (which once
        // contaminated ~/.claude/settings.json).
        let hookless = Manifest::parse(
            "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"gemini\"]\n[capture]\nvisible = []\n",
            "gemini.toml",
        )
        .unwrap();
        assert!(hookless.hooks.is_none(), "fixture must be hookless");
        assert!(
            hook_events(&hookless).is_empty(),
            "hookless manifest must wire no hook events"
        );
    }

    #[test]
    fn parser_covers_every_wired_event() {
        let mut wired = hook_events(&claude());
        wired.sort();
        let mut coverage: Vec<String> = CLAUDE_PARSER_COVERAGE
            .iter()
            .map(|s| s.to_string())
            .collect();
        coverage.sort();
        assert_eq!(
            wired, coverage,
            "hook_events must equal the parser coverage"
        );
    }
}
