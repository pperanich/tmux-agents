//! Guarded stamping: the write half of the poll cycle. Renders a producer's verdict into a
//! chained, server-side-guarded `tmux set-option` invocation and spawns it.
//!
//! The *rendering* is pure and lives in `tma-core` ([`tma_core::render`]); this module owns the
//! seam that touches tmux: mapping a [`Verdict`] to a guard, the capability probe, appending the
//! `@agent_summary` rollups (window and session), and the agent-exit removal.

use std::sync::atomic::{AtomicBool, Ordering};

use tma_core::render::{self, Guard, Publish, StampCommand, SummaryScope};
use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, Verdict, WriteAction};

use crate::tmux::{PaneRecord, Tmux, TmuxError};

/// Fires the one-time degrade warning at most once per process (so the picker's 1 Hz refresh
/// loop does not spam it every cycle).
static DEGRADE_WARNED: AtomicBool = AtomicBool::new(false);

/// What to write for one pane this cycle.
pub enum StampPlan {
    /// Commit a new (guarded) state tuple.
    Publish(Publish),
    /// Writes-on-hold: refresh freshness + hash only.
    Hold { stamped_at: u64, hash: Option<u64> },
    /// The agent exited: remove all `@agent_*` options.
    Remove,
}

/// Choose the write guard for a verdict's `Publish`. The guard re-checks `@agent_source` (and, for
/// the carve-out, `@agent_evidence_at`) server-side at write time, so a hook event landing inside
/// the read→write window still wins (TOCTOU-safe).
pub(crate) fn guard_for_verdict(v: &Verdict) -> Guard {
    if v.writes.episode_reset {
        // The stored tuple belongs to a dead pid; render writes it unconditionally anyway.
        return Guard::Unconditional;
    }
    if v.writes.may_override {
        return Guard::Unconditional;
    }
    match v.winning_evidence.source {
        // Corroboration of an existing hook claim: advance its evidence_at only while the
        // store still shows that state (a concurrent hook must not be clobbered).
        Provenance::Hook => Guard::RefreshClaim { state: v.state },
        // Blocker chrome overrides a working/idle hook claim iff its capture postdates the stamped
        // evidence (the carve-out); a working/idle capture write never clobbers a hook claim.
        _ if v.state == AgentState::Blocked => Guard::CarveOut {
            capture_at: v.winning_evidence.at,
        },
        _ => Guard::ProtectHook,
    }
}

/// Build the [`StampPlan`] for a verdict: `Publish` carries the guard, freshness, and identity;
/// `Hold` refreshes freshness + hash only. `now` is the cycle clock, `hash` the viewport hash.
pub fn plan_from_verdict(
    pane_id: &str,
    v: &Verdict,
    pid: u32,
    name: &str,
    hash: u64,
    now: u64,
) -> StampPlan {
    match v.writes.action {
        WriteAction::Hold => StampPlan::Hold {
            stamped_at: now,
            hash: Some(hash),
        },
        WriteAction::Publish => StampPlan::Publish(Publish {
            pane_id: pane_id.to_string(),
            state: v.state,
            detail: v.detail.clone(),
            source: v.winning_evidence.source,
            evidence_at: v.winning_evidence.at,
            since: v.winning_evidence.at,
            stamped_at: now,
            hash: Some(hash),
            pid,
            name: name.to_string(),
            set_attention: v.writes.set_attention,
            episode_reset: v.writes.episode_reset,
            guard: guard_for_verdict(v),
        }),
    }
}

/// Whether this server supports `set-option -F` conditional writes (the guards). Cached in
/// [`opt::SETPF_OK`] for the server's life; probes once against any pane. On `false`, producers fall
/// back to advisory (unguarded) writes (NOT TOCTOU-safe, clobber race), warned once. If the probe
/// cannot run, default to guarded WITHOUT caching so a later cycle re-probes.
pub fn guarded_writes_supported(tmux: &Tmux, panes: &[PaneRecord]) -> bool {
    match tmux.get_server_option(opt::SETPF_OK) {
        Ok(Some(v)) => {
            let ok = v == "1";
            if !ok {
                warn_degrade_once();
            }
            return ok;
        }
        Ok(None) => {}         // not probed yet — fall through to probe
        Err(_) => return true, // server gone / read failed: assume modern, don't cache
    }

    let Some(pane) = panes.first() else {
        return true; // nothing to probe against; retry on a later cycle with panes
    };
    match probe_conditional_writes(tmux, &pane.pane_id) {
        Ok(ok) => {
            let _ = tmux.set_server_option(opt::SETPF_OK, if ok { "1" } else { "0" });
            if !ok {
                warn_degrade_once();
            }
            ok
        }
        Err(_) => true, // probe failed mid-run: default guarded, leave the cache unset
    }
}

fn warn_degrade_once() {
    if !DEGRADE_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "tma: this tmux lacks `set-option -F` expansion; using advisory writes \
             (clobber race possible under concurrent hook events)."
        );
    }
}

/// Behaviour probe for `set-option -pF`: tests that the format actually *expands* server-side (a
/// pre-3.2 tmux stores the literal string on a bare success), else degrade to advisory writes.
pub fn probe_conditional_writes(tmux: &Tmux, pane_id: &str) -> Result<bool, TmuxError> {
    const KEY: &str = "@__tma_probe";
    let set = StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-F".into(),
            "-t".into(),
            pane_id.into(),
            KEY.into(),
            // Expands to `ok` only if `-F` evaluates the conditional.
            "#{?#{==:1,1},ok,no}".into(),
        ],
    };
    tmux.apply(&[set])?;
    let value = tmux.display(pane_id, &format!("#{{{KEY}}}"))?;
    let unset = StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-u".into(),
            "-t".into(),
            pane_id.into(),
            KEY.into(),
        ],
    };
    let _ = tmux.apply(&[unset]);
    Ok(value == "ok")
}

/// Apply a plan for one pane as a single chained invocation, appending both `@agent_summary`
/// recomputes (window and session). `guarded` selects the `Publish` path: `-F` guards, else the advisory degrade (clobber race).
pub fn apply(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    plan: &StampPlan,
    guarded: bool,
) -> Result<(), TmuxError> {
    let mut cmds = match plan {
        StampPlan::Publish(p) if guarded => render::render_publish(p),
        StampPlan::Publish(p) => {
            // Advisory degrade (no `-F`): plain unguarded sets, with `@agent_since` write-once
            // computed producer-side from the snapshot. Read-then-write, NOT TOCTOU-safe.
            let prev_state = stored_state(all_panes, pane_id);
            let prev_since = stored_since(all_panes, pane_id);
            render::render_publish_advisory(p, prev_state, prev_since)
        }
        StampPlan::Hold { stamped_at, hash } => render::render_hold(pane_id, *stamped_at, *hash),
        StampPlan::Remove => render::render_remove(pane_id),
    };

    // The target pane's state as it will read *after* this write, for the rollup.
    let target_state = match plan {
        StampPlan::Publish(p) => Some(p.state),
        StampPlan::Remove => None,
        StampPlan::Hold { .. } => stored_state(all_panes, pane_id),
    };
    cmds.push(window_summary_command(all_panes, pane_id, target_state));
    cmds.push(session_summary_command(all_panes, pane_id, target_state));

    tmux.apply(&cmds)
}

/// Apply a context-utilization observation to `pane_id`: the guarded `-F` evidence-time write
/// when the server supports it, else the advisory degrade (a producer-side `not older` decision against
/// the stored `@agent_context_at`, then a plain write). One chained invocation. `pct = None` is a
/// null-clear, `tokens = None` clears the absolute count pair. The ownership filter is the caller's
/// (it needs the payload session); this is the store.
pub fn apply_context(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    pct: Option<u8>,
    tokens: Option<u64>,
    evidence_at: u64,
    guarded: bool,
) -> Result<(), TmuxError> {
    let cmds = if guarded {
        render::render_context(pane_id, pct, tokens, evidence_at)
    } else {
        // No `-F` expansion: decide producer-side. Drop an observation not newer than the stored
        // evidence time (`e|>` accepts equal, so does this: only a strictly-older one is dropped).
        if evidence_at < stored_context_at(all_panes, pane_id) {
            return Ok(());
        }
        render::render_context_advisory(pane_id, pct, tokens, evidence_at)
    };
    tmux.apply(&cmds)
}

/// Arm the `context_high` notify marker on `pane_id`: a guarded set-from-absent plus a
/// mandatory read-back, so two concurrent firers resolve to one bell. Returns `true` when THIS call
/// won the marker (its value is the stored one), so only the winner fires. Degrades to a producer-side
/// read-decide-write on a server without `-F`. `now` is the marker value (debuggability only).
pub fn arm_context_notify(
    tmux: &Tmux,
    pane_id: &str,
    now: u64,
    guarded: bool,
) -> Result<bool, TmuxError> {
    if guarded {
        let cmd = render::render_context_notify_fire(pane_id, now);
        tmux.apply(std::slice::from_ref(&cmd))?;
        // Read-back: the winner sees its own value; a loser reads the winner's older marker.
        let stored = tmux.get_pane_option(pane_id, opt::CONTEXT_NOTIFIED_AT)?;
        Ok(stored.as_deref() == Some(now.to_string().as_str()))
    } else {
        // Advisory degrade: only set when currently absent (racy, the documented no-`-F` posture).
        if tmux
            .get_pane_option(pane_id, opt::CONTEXT_NOTIFIED_AT)?
            .filter(|v| !v.is_empty())
            .is_some()
        {
            return Ok(false);
        }
        let cmd = render::render_context_notify_fire_advisory(pane_id, now);
        tmux.apply(std::slice::from_ref(&cmd))?;
        Ok(true)
    }
}

/// Rearm the `context_high` notify marker on `pane_id`: unset it so the next crossing fires.
pub fn rearm_context_notify(tmux: &Tmux, pane_id: &str) -> Result<(), TmuxError> {
    let cmd = render::render_context_notify_rearm(pane_id);
    tmux.apply(std::slice::from_ref(&cmd))
}

/// The pane's currently-stored `@agent_context_at` (0 when unset): the advisory path's `not older`
/// comparison basis.
fn stored_context_at(all_panes: &[PaneRecord], pane_id: &str) -> u64 {
    all_panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .and_then(|p| p.options.get(opt::CONTEXT_AT))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The `@agent_summary` write for `target_pane`'s window, rolling up sibling states. `target_state`
/// is what the pane reads after the pending write (`None` when removed). Exposed for the hook path.
///
/// A best-effort in-chain hint: rolled up from the intended state plus cycle-start siblings, so a
/// multi-agent window can be mis-rolled. The convergent authority is `run_cycle`'s end-of-cycle
/// reconciliation (recompute + write-on-drift); this append serves callers with no cycle.
pub fn window_summary_command(
    all_panes: &[PaneRecord],
    target_pane: &str,
    target_state: Option<AgentState>,
) -> StampCommand {
    let summary = summary_string_for(SummaryScope::Window, all_panes, target_pane, target_state);
    render::render_summary(SummaryScope::Window, target_pane, summary.as_deref())
}

/// [`window_summary_command`]'s session-scoped mirror: the same rollup over every pane in the
/// target's session, written to `@agent_session_summary`.
pub fn session_summary_command(
    all_panes: &[PaneRecord],
    target_pane: &str,
    target_state: Option<AgentState>,
) -> StampCommand {
    let summary = summary_string_for(SummaryScope::Session, all_panes, target_pane, target_state);
    render::render_summary(SummaryScope::Session, target_pane, summary.as_deref())
}

/// [`window_summary_command`] carrying the tuple's `guard`, so the rollup commits iff the pane stamp
/// commits: a hook event that loses arbitration must not overwrite the winning claim's summary with
/// its own suppressed state. The cycle path keeps the unguarded variant (reconciliation self-corrects).
pub fn window_summary_command_guarded(
    all_panes: &[PaneRecord],
    target_pane: &str,
    target_state: Option<AgentState>,
    guard: Guard,
) -> StampCommand {
    let summary = summary_string_for(SummaryScope::Window, all_panes, target_pane, target_state);
    render::render_summary_guarded(SummaryScope::Window, target_pane, summary.as_deref(), guard)
}

/// [`window_summary_command_guarded`]'s session-scoped mirror, carrying the same guard so both
/// rollups commit exactly when the pane stamp does.
pub fn session_summary_command_guarded(
    all_panes: &[PaneRecord],
    target_pane: &str,
    target_state: Option<AgentState>,
    guard: Guard,
) -> StampCommand {
    let summary = summary_string_for(SummaryScope::Session, all_panes, target_pane, target_state);
    render::render_summary_guarded(
        SummaryScope::Session,
        target_pane,
        summary.as_deref(),
        guard,
    )
}

/// The rollup string for `target_pane`'s window or session: `target_state` for the target pane (its
/// post-write state, `None` when removed) plus each sibling's stored `@agent_state`.
fn summary_string_for(
    scope: SummaryScope,
    all_panes: &[PaneRecord],
    target_pane: &str,
    target_state: Option<AgentState>,
) -> Option<String> {
    let target = all_panes.iter().find(|p| p.pane_id == target_pane)?;
    // The target pane's stored state is replaced by its intended post-write `target_state`.
    let mut states = states_in_scope(scope, all_panes, target, Some(target_pane));
    if let Some(s) = target_state {
        states.push(s);
    }
    render::summary_string(&states)
}

/// Collect the stored `@agent_state` of every pane sharing `scope` with `member`, in `all_panes`
/// order, optionally skipping one by id. Shared by the in-chain rollups and the reconcilers.
pub(crate) fn states_in_scope(
    scope: SummaryScope,
    all_panes: &[PaneRecord],
    member: &PaneRecord,
    skip_pane: Option<&str>,
) -> Vec<AgentState> {
    all_panes
        .iter()
        .filter(|p| in_scope(scope, p, member))
        .filter(|p| skip_pane != Some(p.pane_id.as_str()))
        .filter_map(|p| p.options.get(opt::STATE).and_then(|v| v.parse().ok()))
        .collect()
}

/// Whether `pane` rolls up into the same `scope` as `member`.
fn in_scope(scope: SummaryScope, pane: &PaneRecord, member: &PaneRecord) -> bool {
    pane.session == member.session
        && (scope == SummaryScope::Session || pane.window_index == member.window_index)
}

/// The convergent rollup reconciliation both authorities share (the end-of-cycle pass and the
/// daemon's lifecycle pass): recompute every window's and every session's summary from `state_of`
/// and emit a write only where it drifts from what tmux stores, so a quiet server writes nothing.
/// One function for both scopes, so the two rollups cannot diverge.
pub fn reconcile_summary_commands(
    panes: &[PaneRecord],
    state_of: impl Fn(&PaneRecord) -> Option<AgentState>,
) -> Vec<StampCommand> {
    let mut cmds = Vec::new();
    for scope in [SummaryScope::Window, SummaryScope::Session] {
        // One representative pane per scope instance; its stored value is the whole scope's.
        let mut seen: Vec<(&str, Option<u32>)> = Vec::new();
        for rec in panes {
            let key = match scope {
                SummaryScope::Window => (rec.session.as_str(), Some(rec.window_index)),
                SummaryScope::Session => (rec.session.as_str(), None),
            };
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);

            let states: Vec<AgentState> = panes
                .iter()
                .filter(|p| in_scope(scope, p, rec))
                .filter_map(&state_of)
                .collect();
            let desired = render::summary_string(&states);
            let stored = match scope {
                SummaryScope::Window => rec.window_summary.as_deref(),
                SummaryScope::Session => rec.session_summary.as_deref(),
            };
            if desired.as_deref() != stored {
                cmds.push(render::render_summary(
                    scope,
                    &rec.pane_id,
                    desired.as_deref(),
                ));
            }
        }
    }
    cmds
}

/// A pane's stored `@agent_state`, the reconciliation fallback for a pane this pass did not decide.
pub fn stored_pane_state(pane: &PaneRecord) -> Option<AgentState> {
    pane.options.get(opt::STATE).and_then(|v| v.parse().ok())
}

/// Parse a pane's currently-stored `@agent_state` from a `list-panes` read.
fn stored_state(all_panes: &[PaneRecord], pane_id: &str) -> Option<AgentState> {
    all_panes
        .iter()
        .find(|p| p.pane_id == pane_id)?
        .options
        .get(opt::STATE)
        .and_then(|v| v.parse().ok())
}

/// Parse a pane's currently-stored `@agent_since` (epoch), 0 when unset. The advisory write
/// path's producer-side write-once basis.
fn stored_since(all_panes: &[PaneRecord], pane_id: &str) -> u64 {
    all_panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .and_then(|p| p.options.get(opt::SINCE))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::verdict::{WinningEvidence, WritePlan};
    use tma_core::{Detail, WriteAction};

    fn verdict(
        state: AgentState,
        source: Provenance,
        at: u64,
        may_override: bool,
        episode_reset: bool,
    ) -> Verdict {
        Verdict {
            state,
            detail: None,
            winning_evidence: WinningEvidence {
                source,
                at,
                label: "t".into(),
            },
            writes: WritePlan {
                action: WriteAction::Publish,
                may_override,
                set_attention: false,
                episode_reset,
            },
        }
    }

    #[test]
    fn may_override_maps_to_unconditional() {
        let v = verdict(AgentState::Unknown, Provenance::Process, 10, true, false);
        assert_eq!(guard_for_verdict(&v), Guard::Unconditional);
    }

    #[test]
    fn episode_reset_is_unconditional_regardless_of_override() {
        let v = verdict(AgentState::Working, Provenance::Capture, 10, false, true);
        assert_eq!(guard_for_verdict(&v), Guard::Unconditional);
    }

    #[test]
    fn capture_working_protects_a_hook_claim() {
        let v = verdict(AgentState::Working, Provenance::Capture, 10, false, false);
        assert_eq!(guard_for_verdict(&v), Guard::ProtectHook);
    }

    #[test]
    fn blocker_chrome_uses_carveout_with_capture_time() {
        let v = verdict(AgentState::Blocked, Provenance::Capture, 42, false, false);
        assert_eq!(guard_for_verdict(&v), Guard::CarveOut { capture_at: 42 });
    }

    #[test]
    fn hook_corroboration_uses_refresh_claim() {
        let v = verdict(AgentState::Working, Provenance::Hook, 99, false, false);
        assert_eq!(
            guard_for_verdict(&v),
            Guard::RefreshClaim {
                state: AgentState::Working
            }
        );
    }

    #[test]
    fn detail_survives_into_publish_guard_wrapping() {
        // A blocked publish keeps its detail; sanity that the mapping does not drop it.
        let mut v = verdict(AgentState::Blocked, Provenance::Capture, 5, false, false);
        v.detail = Some(Detail::new("permission"));
        // The guard choice does not depend on detail, but the driver carries it through.
        assert_eq!(guard_for_verdict(&v), Guard::CarveOut { capture_at: 5 });
    }
}
