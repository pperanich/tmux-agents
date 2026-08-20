//! Notification dispatch + bounded transition history. Daemon-only and strictly additive: the
//! daemonless `tma event` path fires inline through the *same* [`fire`](tma_runtime::notify::fire)
//! primitive, so the two cannot diverge. All dispatch runs in ONE place
//! ([`NotifyState::reconcile`]), keyed only on the persisted
//! `@agent_state`/`@agent_since`/`@agent_notified_at` tuple, so it fires once per episode regardless
//! of which producer drove the pane into `blocked`. Fire iff `blocked` AND the marker strictly
//! predates `@agent_since`: one predicate that is dedup, cold-start, and episode re-arming.
//! Write-before-fire commits the marker BEFORE the action, so a crash drops one fire, never doubles.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Child;

use tma_core::render;
use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, ReadResult, StampedState};

use tma_runtime::config::{trigger_enabled, NotifyCommands, NotifySinks, NotifyTrigger};
use tma_runtime::notify::{evaluate_context_high, fire, notification_for, trigger_for};
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

/// Env var that OVERRIDES the `notify.command` config (a test/CI seam; config is canonical). Unset ⇒
/// use `notify.command`; both unset ⇒ `display-message` only.
const NOTIFY_CMD_ENV: &str = "TMA_NOTIFY_CMD";

/// Cap on the transition-history ring. Disposable daemon memory: a debugging/latency aid, NOT the
/// dedup record (that is the persisted `@agent_notified_at` marker).
const HISTORY_CAP: usize = 256;

/// Cap on in-flight fire-and-forget notify-command children: bounds a pathological hung sink. At the
/// cap the OLDEST (most likely hung) is killed + reaped ([`bound_pending`]), never left a zombie.
const MAX_PENDING: usize = 64;

/// One recorded state transition for the history ring (latency verification + debugging).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Transition {
    pub pane: String,
    /// The pane's previously-observed state (`None` on first observation / after a clear).
    pub from: Option<AgentState>,
    pub to: AgentState,
    /// The transition epoch (`@agent_since`).
    pub at: u64,
    /// Provenance of the state that was transitioned into (`@agent_source`).
    pub source: Provenance,
}

/// Fire predicate: the [`NotifyTrigger`] this state fires (its token in the `on` set AND the marker
/// strictly predates the episode instant), or `None`. `blocked` fires regardless of attention (focus
/// can clear it mid-episode); a "done" landing is idle still carrying `@agent_attention`.
/// `@agent_since` is write-once per state, so a blocked-then-done episode fires once for each — and
/// a SECOND completion inside one idle run is a new episode only because `@agent_turn_at` moved,
/// which is why the comparison is [`StampedState::episode_at`] rather than `since`.
fn fire_trigger(s: &StampedState, on: &[NotifyTrigger]) -> Option<NotifyTrigger> {
    let noteworthy = s.state == AgentState::Blocked || s.attention;
    let trigger = trigger_for(s.state, noteworthy)?;
    (trigger_enabled(on, trigger)
        && !tma_runtime::event::episode_already_notified(s.notified_at, s.episode_at()))
    .then_some(trigger)
}

/// Resolve the effective notify routing: the `TMA_NOTIFY_CMD` env override when set (it replaces
/// every trigger's command), else the configured per-trigger routing. Shared by `new`/`reconfigure`
/// so startup and reload resolve identically.
fn resolve_commands(config_commands: NotifyCommands) -> NotifyCommands {
    config_commands.overridden_by(std::env::var(NOTIFY_CMD_ENV).ok())
}

/// The daemon's notification + transition-history state. Owned by the serve loop
/// (single-threaded); no interior mutability.
pub(crate) struct NotifyState {
    /// The hook commands: the `notify.command` config plus its per-trigger `[notify.<trigger>]`
    /// overrides, all replaced by the [`NOTIFY_CMD_ENV`] env var when set (the test/CI seam).
    commands: NotifyCommands,
    /// `notify.on`: which transitions fire (default `["blocked"]`). No env override; config is
    /// canonical for the trigger set.
    on: Vec<NotifyTrigger>,
    /// `notify.bell` / `notify.osc`: the tty sinks. Pure companions of the fire (they ring iff a
    /// notification fires), threaded into the shared [`fire`] primitive.
    sinks: NotifySinks,
    /// `notify.context_high.threshold`: the context-utilization notify threshold, `None` when
    /// unconfigured. The daemon inherits context-high dispatch by reading the gauge each reconcile.
    context_high: Option<u8>,
    /// Bounded ring of recent transitions (disposable daemon memory).
    history: VecDeque<Transition>,
    /// Last-observed state per agent pane, for from→to transition detection. Pruned to live
    /// agent panes every reconcile, so it is bounded by the live fleet.
    last_state: HashMap<String, AgentState>,
    /// In-flight fire-and-forget command children awaiting reap (bounded by [`MAX_PENDING`]).
    pending: Vec<PendingFire>,

    // ---- introspection counters (status file; tests + operators) ----
    /// Notifications fired over the daemon's life (monotone).
    fires: u64,
    /// Transitions pushed into the history ring over the daemon's life (monotone).
    transitions_recorded: u64,
}

impl NotifyState {
    /// `config_commands` is the `[notify]` routing (overridden by `TMA_NOTIFY_CMD`); `on` is the
    /// `notify.on` trigger set (`["blocked"]` default); `bell` is the `notify.bell` companion;
    /// `context_high` is the `notify.context_high.threshold`, `None` when unconfigured.
    pub(crate) fn new(
        config_commands: NotifyCommands,
        on: Vec<NotifyTrigger>,
        sinks: NotifySinks,
        context_high: Option<u8>,
    ) -> NotifyState {
        NotifyState {
            commands: resolve_commands(config_commands),
            on,
            sinks,
            context_high,
            history: VecDeque::new(),
            last_state: HashMap::new(),
            pending: Vec::new(),
            fires: 0,
            transitions_recorded: 0,
        }
    }

    /// Swap the config-derived command + `on` set + `bell` + `context_high` (SIGHUP reload of
    /// `[notify]`), preserving the history ring, last-seen state map, and in-flight children: a reload
    /// changes only *what* fires. A changed threshold applies from the next observation with the armed
    /// flag as-is, so no marker is touched here.
    pub(crate) fn reconfigure(
        &mut self,
        config_commands: NotifyCommands,
        on: Vec<NotifyTrigger>,
        sinks: NotifySinks,
        context_high: Option<u8>,
    ) {
        self.commands = resolve_commands(config_commands);
        self.on = on;
        self.sinks = sinks;
        self.context_high = context_high;
    }

    /// The single notification-dispatch pass (see module docs): one `list-panes` read that records
    /// each pane's transition and fires for a `blocked` pane whose marker predates its episode.
    pub(crate) fn reconcile(&mut self, tmux: &Tmux) -> Result<(), TmuxError> {
        self.reap();
        let panes = tmux.list_panes()?;
        let now = tma_runtime::now_ms();
        let mut live: HashSet<String> = HashSet::new();
        // `-F` support, probed once and only when context-high is configured (a reader-model dispatch
        // reading the gauge each reconcile, so the daemon inherits the notify without a new lane).
        let mut guarded_supported: Option<bool> = None;

        for rec in &panes {
            let Some(stored) = StampedState::from_options(&rec.options)
                .ok()
                .flatten()
                .map(ReadResult::into_inner)
            else {
                continue; // not an agent pane (no `@agent_state`)
            };
            live.insert(rec.pane_id.clone());

            // Transition history: record a change vs the last observed state.
            let prev = self.last_state.get(&rec.pane_id).copied();
            if prev != Some(stored.state) {
                self.push_transition(Transition {
                    pane: rec.pane_id.clone(),
                    from: prev,
                    to: stored.state,
                    at: stored.since,
                    source: stored.source,
                });
                self.last_state.insert(rec.pane_id.clone(), stored.state);
            }

            // Dispatch: fire once per state-run for a configured trigger, keyed only on the
            // persisted marker.
            if let Some(trigger) = fire_trigger(&stored, &self.on) {
                self.fire_for(tmux, rec, &stored, now, trigger);
            }

            // Context-high dispatch: read the gauge + armed flag off the same list-panes and
            // decide, on the pane's own `@agent_context_notified_at` marker. Independent of the state
            // lane above.
            if let Some(threshold) = self.context_high {
                let g = *guarded_supported
                    .get_or_insert_with(|| tma_tmux::stamp::guarded_writes_supported(tmux, &panes));
                self.fire_context_high(tmux, rec, threshold, g, now);
            }
        }

        // Prune the last-seen map to live agent panes: a closed/exited pane drops its entry.
        self.last_state.retain(|p, _| live.contains(p));
        Ok(())
    }

    /// Write-before-fire for one blocked pane: commit `@agent_notified_at` FIRST, and fire only on a
    /// successful commit, so a failed marker write (server gone) never yields an un-deduped fire.
    fn fire_for(
        &mut self,
        tmux: &Tmux,
        rec: &PaneRecord,
        stored: &StampedState,
        now: u64,
        trigger: NotifyTrigger,
    ) {
        // Clamp the marker forward past the episode instant (see [`clamp_marker`]): a backward
        // wall-clock step must not let the marker predate the episode, or it re-fires.
        let mark_at = clamp_marker(now, stored.episode_at());
        let marker = render::set_pane_option(&rec.pane_id, opt::NOTIFIED_AT, &mark_at.to_string());
        if tmux.apply(&[marker]).is_err() {
            return; // marker not committed ⇒ do not fire (retried next reconcile)
        }

        // `tma mute` on this pane: the episode is marked notified above and then stays silent, so a
        // mute that expires mid-episode does not ring for a transition the user already muted.
        if tma_runtime::notify::muted(rec, now) {
            return;
        }

        // The payload carries the trigger word (`blocked`/`done`), not the raw landing token (`idle`
        // for a done fire); see [`Notification::state`]. Built through the shared builder, so this
        // payload is identical to the daemonless path's for the same transition.
        //
        // The episode start is `episode_at()`, not `since`: the payload's `since_ms` is documented
        // as the episode's age at dispatch, and on a second completion `@agent_since` is still
        // pinned to the start of the idle run, which would report hours instead of latency. The
        // dedup above and the marker clamp already read the same instant.
        let n = notification_for(
            rec,
            rec.options
                .get(opt::NAME)
                .map(String::as_str)
                .unwrap_or("agent"),
            trigger.word(),
            stored.detail.as_ref().map(|d| d.as_str().to_string()),
            stored.session.clone(),
            stored.episode_at(),
            now,
        );
        let command = self.commands.for_trigger(trigger).map(str::to_string);
        if let Some(child) = fire(tmux, &n, command.as_deref(), &self.sinks) {
            self.track_child(child, command);
        }
        self.fires += 1;
    }

    /// Context-high dispatch for one pane: read the stored gauge + armed flag and run the
    /// shared [`evaluate_context_high`], which arms-and-fires (guarded, read-back) or rearms. The
    /// marker is its own armed flag, never the state lane's `@agent_notified_at`.
    fn fire_context_high(
        &mut self,
        tmux: &Tmux,
        rec: &PaneRecord,
        threshold: u8,
        guarded: bool,
        now: u64,
    ) {
        let command = self.commands.for_context_high().map(str::to_string);
        if let Some(child) = evaluate_context_high(
            tmux,
            guarded,
            rec,
            threshold,
            command.as_deref(),
            &self.sinks,
            now,
        ) {
            self.track_child(child, command);
            self.fires += 1;
        }
    }

    /// Track a freshly-spawned fire-and-forget child under the cap ([`bound_pending`] makes room),
    /// so the push never exceeds [`MAX_PENDING`]. The marker is committed, so displacing never affects dedup.
    fn track_child(&mut self, child: Child, command: Option<String>) {
        bound_pending(&mut self.pending, MAX_PENDING);
        self.pending.push(PendingFire {
            child,
            command: command.unwrap_or_default(),
        });
    }

    /// Reap any finished fire-and-forget command children (no zombies), keeping only those still
    /// running. Cheap on the usual empty/tiny `pending` vector. This is where the daemon learns a
    /// notify command's exit status at all, so a non-zero one lands in the failure marker here.
    fn reap(&mut self) {
        reap_finished(&mut self.pending, tma_runtime::now_ms());
    }

    fn push_transition(&mut self, t: Transition) {
        if self.history.len() >= HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(t);
        self.transitions_recorded += 1;
    }

    /// The transition ring as the shared wire document (oldest first), for the history IPC verb.
    /// Disposable daemon memory: it starts empty on every restart, which is exactly what the
    /// `[notify] log` file is for when a durable record is wanted.
    pub(crate) fn history_document(&self) -> String {
        tma_runtime::transitions::render_document(&tma_runtime::transitions::Transitions {
            records: self
                .history
                .iter()
                .map(|t| tma_runtime::transitions::TransitionRecord {
                    pane: t.pane.clone(),
                    from: t.from.map(|s| s.token().to_string()),
                    to: t.to.token().to_string(),
                    at: t.at,
                    source: t.source.token().to_string(),
                })
                .collect(),
            cap: HISTORY_CAP,
            recorded: self.transitions_recorded,
        })
    }

    /// The `key=value` introspection lines for the status file, so acceptance tests can assert the
    /// fire count, history bound, and total transitions recorded.
    pub(crate) fn status_lines(&self) -> String {
        format!(
            "notify_fires={}\ntransitions_recorded={}\nhistory_len={}\nhistory_cap={}\n\
             notify_pending={}\n",
            self.fires,
            self.transitions_recorded,
            self.history.len(),
            HISTORY_CAP,
            self.pending.len(),
        )
    }
}

impl Default for NotifyState {
    fn default() -> Self {
        Self::new(
            NotifyCommands::default(),
            vec![NotifyTrigger::Blocked],
            NotifySinks::default(),
            None,
        )
    }
}

/// One in-flight notify child plus the command that spawned it, so its exit status can be recorded
/// against a name when it is reaped (empty for a fire with no hook command).
struct PendingFire {
    child: Child,
    command: String,
}

/// Drop finished children from `pending`, reaping each and recording its outcome in the failure
/// marker. Shared by [`NotifyState::reap`] and [`bound_pending`]. A child is dropped ONLY on
/// `Ok(Some(_))` (exited + reaped); `Ok(None)` and a transient `Err` (e.g. EINTR) are RETAINED, since
/// dropping an `Err` handle would leak the child unreaped. A later pass reaps it, and the cap's
/// kill+wait forces a blocking reap if it persists.
fn reap_finished(pending: &mut Vec<PendingFire>, now: u64) {
    pending.retain_mut(|p| match p.child.try_wait() {
        Ok(Some(status)) => {
            if !p.command.is_empty() {
                tma_runtime::notify::failure::record_exit(&p.command, &status, now);
            }
            false
        }
        _ => true,
    });
}

/// Ensure `pending` has room under `cap`: reap finished, then kill+reap the OLDEST if still full, so
/// the caller's push cannot exceed `cap` nor leak a child. Free-standing for unit tests.
fn bound_pending(pending: &mut Vec<PendingFire>, cap: usize) {
    reap_finished(pending, tma_runtime::now_ms());
    if pending.len() >= cap && !pending.is_empty() {
        // The oldest still-running child is the most likely hung one; kill + wait reaps it. Not
        // recorded as a sink failure: WE killed it, and the marker is for the user's command failing.
        let mut oldest = pending.remove(0);
        let _ = oldest.child.kill();
        let _ = oldest.child.wait();
    }
}

/// The notify marker to commit for an episode: `now`, clamped forward past the episode instant
/// ([`StampedState::episode_at`]). Under a backward wall-clock step `now` can land before it, and a
/// marker predating the episode would read as not-yet-notified and re-fire; the `max` holds dedup.
/// With a monotone clock it is exactly `now`.
fn clamp_marker(now: u64, episode_at: u64) -> u64 {
    now.max(episode_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::Detail;

    fn blocked(notified_at: Option<u64>, since: u64) -> StampedState {
        StampedState {
            state: AgentState::Blocked,
            detail: Some(Detail::new("permission")),
            source: Provenance::Capture,
            evidence_at: since,
            since,
            turn_at: 0,
            stamped_at: since,
            attention: false,
            notified_at,
            hash: None,
            pid: 4242,
            session: None,
            subagents: vec![],
        }
    }

    /// The default trigger set (blocked-only).
    fn blocked_only() -> Vec<NotifyTrigger> {
        vec![NotifyTrigger::Blocked]
    }

    /// The opt-in set (blocked + done).
    fn blocked_and_done() -> Vec<NotifyTrigger> {
        vec![NotifyTrigger::Blocked, NotifyTrigger::Done]
    }

    // ---- blocked fire predicate (dedup + cold-start, one rule) -----------------------------

    #[test]
    fn fires_on_blocked_with_no_marker() {
        assert_eq!(
            fire_trigger(&blocked(None, 100), &blocked_only()),
            Some(NotifyTrigger::Blocked)
        );
    }

    #[test]
    fn fires_when_marker_strictly_predates_since() {
        // A stale marker from a prior state-run (< the new since) ⇒ fire.
        assert_eq!(
            fire_trigger(&blocked(Some(90), 100), &blocked_only()),
            Some(NotifyTrigger::Blocked)
        );
    }

    #[test]
    fn does_not_fire_when_marker_equals_since() {
        // Strict predates: equal does NOT fire (the per-state-run dedup boundary).
        assert_eq!(
            fire_trigger(&blocked(Some(100), 100), &blocked_only()),
            None
        );
    }

    #[test]
    fn does_not_fire_when_marker_postdates_since() {
        // Cold-start: notified_at >= since ⇒ already notified this state-run ⇒ no re-fire.
        assert_eq!(
            fire_trigger(&blocked(Some(120), 100), &blocked_only()),
            None
        );
    }

    #[test]
    fn blocked_fires_even_when_attention_cleared() {
        // Focus can clear @agent_attention mid-episode while blocked continues; blocked still
        // fires off the marker (attention is not the dedup record).
        let s = blocked(None, 100); // attention: false
        assert_eq!(
            fire_trigger(&s, &blocked_only()),
            Some(NotifyTrigger::Blocked)
        );
    }

    #[test]
    fn does_not_fire_when_not_blocked_or_done() {
        let mut s = blocked(None, 100);
        s.state = AgentState::Working;
        assert_eq!(fire_trigger(&s, &blocked_and_done()), None);
        // Plain idle (no attention) is not a "done" landing.
        s.state = AgentState::Idle;
        s.attention = false;
        assert_eq!(fire_trigger(&s, &blocked_and_done()), None);
    }

    // ---- done fire predicate (idle + attention, opt-in) ------------------------------------

    #[test]
    fn done_fires_only_when_configured() {
        // Idle carrying attention = a working→idle completion (the "done" landing).
        let mut s = blocked(None, 100);
        s.state = AgentState::Idle;
        s.attention = true;
        // Not in the default blocked-only set ⇒ no fire (blocked-only users unaffected).
        assert_eq!(fire_trigger(&s, &blocked_only()), None);
        // Opted in ⇒ fires "done".
        assert_eq!(
            fire_trigger(&s, &blocked_and_done()),
            Some(NotifyTrigger::Done)
        );
    }

    #[test]
    fn done_dedups_within_the_idle_state_run() {
        // Once notified_at >= the idle since, the done landing is deduped (no re-fire while idle).
        let mut s = blocked(Some(100), 100);
        s.state = AgentState::Idle;
        s.attention = true;
        assert_eq!(fire_trigger(&s, &blocked_and_done()), None);
    }

    /// A SECOND completion on a pane that never left `idle`: the user cleared the first marker,
    /// the turn-end hook raised it again and moved `@agent_turn_at`, and `@agent_since` — being
    /// write-once per state run — did not move at all. Comparing the marker against `since` alone
    /// deduped this away as the episode the first completion already notified; comparing it
    /// against the episode instant fires it. Revert `fire_trigger` to `s.since` and this fails.
    #[test]
    fn a_second_completion_inside_one_idle_run_fires_again() {
        let mut s = blocked(Some(100), 100);
        s.state = AgentState::Idle;
        s.attention = true;
        // The turn end that re-raised the marker, recorded past the first fire.
        s.turn_at = 200;
        assert_eq!(
            fire_trigger(&s, &blocked_and_done()),
            Some(NotifyTrigger::Done)
        );
        // And once THAT completion is notified, it dedups again.
        s.notified_at = Some(200);
        assert_eq!(fire_trigger(&s, &blocked_and_done()), None);
    }

    /// The marker commits past the episode instant, not just past `since`: under a backward
    /// wall-clock step a marker predating the turn end it is deduping would re-fire forever.
    #[test]
    fn the_marker_clamps_past_a_turn_end_ahead_of_the_clock() {
        assert_eq!(clamp_marker(150, 200), 200);
        let mut s = blocked(None, 100);
        s.turn_at = 300;
        assert_eq!(clamp_marker(150, s.episode_at()), 300);
    }

    // ---- transition-history ring bound -----------------------------------------------------

    #[test]
    fn history_ring_is_bounded() {
        let mut ns = NotifyState::new(
            NotifyCommands::default(),
            blocked_only(),
            NotifySinks::default(),
            None,
        );
        for i in 0..(HISTORY_CAP + 500) {
            ns.push_transition(Transition {
                pane: format!("%{i}"),
                from: None,
                to: AgentState::Blocked,
                at: i as u64,
                source: Provenance::Capture,
            });
        }
        assert_eq!(
            ns.history.len(),
            HISTORY_CAP,
            "the ring never grows past its cap"
        );
        // It keeps the MOST RECENT transitions (a ring, not a truncated prefix).
        assert_eq!(
            ns.history.back().unwrap().pane,
            format!("%{}", HISTORY_CAP + 499)
        );
        assert_eq!(ns.history.front().unwrap().pane, format!("%{}", 500));
        // The monotone total still counts every push.
        assert_eq!(ns.transitions_recorded, (HISTORY_CAP + 500) as u64);
    }

    #[test]
    fn reconfigure_swaps_trigger_set_preserving_history() {
        // SIGHUP reload: `reconfigure` swaps the `on` set but keeps the accumulated ring, so a
        // reload never drops the daemon's dispatch history.
        let mut ns = NotifyState::new(
            NotifyCommands::default(),
            blocked_only(),
            NotifySinks::default(),
            None,
        );
        ns.push_transition(Transition {
            pane: "%1".to_string(),
            from: None,
            to: AgentState::Blocked,
            at: 100,
            source: Provenance::Capture,
        });
        assert_eq!(ns.on, blocked_only());
        assert!(!ns.sinks.bell);
        let both = NotifySinks {
            bell: true,
            osc: true,
            log: None,
        };
        ns.reconfigure(
            NotifyCommands::default(),
            blocked_and_done(),
            both.clone(),
            None,
        );
        assert_eq!(ns.on, blocked_and_done(), "the trigger set was swapped");
        assert_eq!(ns.sinks, both, "the tty sinks were swapped on reload");
        assert_eq!(ns.history.len(), 1, "the history ring survives the reload");
    }

    #[test]
    fn bound_pending_displaces_and_reaps_the_oldest_when_full() {
        use std::process::Command;
        // A tiny cap makes the overflow path deterministic without spawning MAX_PENDING processes.
        let spawn = || PendingFire {
            child: Command::new("sleep").arg("30").spawn().unwrap(),
            command: String::new(),
        };
        let mut pending: Vec<PendingFire> = vec![spawn(), spawn()];
        assert_eq!(pending.len(), 2);
        // At the cap with both children still running: kill+reap the oldest to make room (no
        // zombie), leaving exactly one slot for the next push.
        bound_pending(&mut pending, 2);
        assert_eq!(
            pending.len(),
            1,
            "oldest displaced to make room under the cap"
        );
        pending.push(spawn());
        assert_eq!(pending.len(), 2, "never exceeds the cap");
        // Cleanup: kill+reap the survivors so the test leaks no processes.
        for mut p in pending {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    #[test]
    fn clamp_marker_holds_dedup_under_a_backward_clock_step() {
        // Monotone clock: the marker is exactly `now`.
        assert_eq!(clamp_marker(200, 100), 200);
        // Backward wall-clock step (now < since): clamp forward so the marker never predates the
        // episode, or the next reconcile would read it as not-yet-notified and re-fire.
        assert_eq!(clamp_marker(90, 100), 100);
        // Equal to `since`: unchanged (the per-state-run dedup boundary).
        assert_eq!(clamp_marker(100, 100), 100);
    }

    #[test]
    fn status_lines_expose_the_counters() {
        let ns = NotifyState::new(
            NotifyCommands::default(),
            blocked_only(),
            NotifySinks::default(),
            None,
        );
        let s = ns.status_lines();
        for key in [
            "notify_fires=",
            "transitions_recorded=",
            "history_len=",
            "history_cap=",
        ] {
            assert!(s.contains(key), "status missing {key}: {s}");
        }
    }
}
