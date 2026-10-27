//! Notification dispatch + bounded transition history. Daemon-only and strictly additive: the
//! daemonless `tma event` path fires inline through the *same* [`fire`](tma_runtime::notify::fire)
//! primitive, so the two cannot diverge. All dispatch runs in ONE place
//! ([`NotifyState::reconcile`]), keyed only on the persisted
//! `@agent_state`/`@agent_since`/`@agent_notified_at` tuple, so it fires once per episode regardless
//! of which producer drove the pane into `blocked`. Fire iff `blocked` AND the marker strictly
//! predates `@agent_since`: one predicate that is dedup, cold-start, and episode re-arming.
//! Write-before-fire commits the marker BEFORE the action, so a crash drops one fire, never doubles.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::process::Child;

use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, ReadResult, StampedState};

use tma_runtime::config::{trigger_enabled, NotifyCommands, NotifySinks, NotifyTrigger};
use tma_runtime::notify::{
    evaluate_context_high_checked, evaluate_stall_checked, fire, notification_for, trigger_for,
};
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

mod progress;

use progress::WindowKey;

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
    /// `notify.stall.threshold_s` as milliseconds, `None` when unconfigured. Evaluated per
    /// observation rather than per edge: a stalled pane draws no edge, which is the whole point.
    stall_ms: Option<u64>,
    /// Bounded ring of recent transitions (disposable daemon memory).
    history: VecDeque<Transition>,
    /// Last-observed state per agent pane, for from→to transition detection. Pruned to live
    /// agent panes every reconcile, so it is bounded by the live fleet.
    last_state: HashMap<String, AgentState>,
    /// In-flight fire-and-forget command children awaiting reap (bounded by [`MAX_PENDING`]).
    pending: Vec<PendingFire>,
    /// `notify.osc_progress`: the windows that held a `working` pane at the last pass. The progress
    /// sequence sets a state the emulator keeps, so only the edges against this set are written.
    working_windows: BTreeSet<WindowKey>,

    // ---- introspection counters (status file; tests + operators) ----
    /// Notifications fired over the daemon's life (monotone).
    fires: u64,
    reconciliations: u64,
    /// Transitions pushed into the history ring over the daemon's life (monotone).
    transitions_recorded: u64,
    /// OSC 9;4 progress edges written over the daemon's life (monotone).
    progress_edges: u64,
}

impl NotifyState {
    /// `config_commands` is the `[notify]` routing (overridden by `TMA_NOTIFY_CMD`); `on` is the
    /// `notify.on` trigger set (`["blocked"]` default); `bell` is the `notify.bell` companion;
    /// `context_high` is the `notify.context_high.threshold` and `stall_ms` the
    /// `notify.stall.threshold_s` in milliseconds, each `None` when unconfigured.
    pub(crate) fn new(
        config_commands: NotifyCommands,
        on: Vec<NotifyTrigger>,
        sinks: NotifySinks,
        context_high: Option<u8>,
        stall_ms: Option<u64>,
    ) -> NotifyState {
        NotifyState {
            commands: resolve_commands(config_commands),
            on,
            sinks,
            context_high,
            stall_ms,
            history: VecDeque::new(),
            last_state: HashMap::new(),
            pending: Vec::new(),
            working_windows: BTreeSet::new(),
            fires: 0,
            reconciliations: 0,
            transitions_recorded: 0,
            progress_edges: 0,
        }
    }

    /// Swap the config-derived command + `on` set + `bell` + `context_high` + `stall` (SIGHUP reload of
    /// `[notify]`), preserving the history ring, last-seen state map, and in-flight children: a reload
    /// changes only *what* fires. A changed threshold applies from the next observation with the armed
    /// flag as-is, so no marker is touched here.
    pub(crate) fn reconfigure(
        &mut self,
        config_commands: NotifyCommands,
        on: Vec<NotifyTrigger>,
        sinks: NotifySinks,
        context_high: Option<u8>,
        stall_ms: Option<u64>,
    ) {
        self.commands = resolve_commands(config_commands);
        self.on = on;
        self.sinks = sinks;
        self.context_high = context_high;
        self.stall_ms = stall_ms;
    }

    /// The single notification-dispatch pass (see module docs): one `list-panes` read that records
    /// each pane's transition and fires for a `blocked` pane whose marker predates its episode.
    #[cfg(test)]
    pub(crate) fn reconcile(&mut self, tmux: &Tmux) -> Result<(), TmuxError> {
        self.reconcile_checked(tmux, |_| true)
    }

    /// Program effects additionally require the daemon's monotonic lease check
    /// after the claim IO. This does not promote a losing publication.
    pub(crate) fn reconcile_checked(
        &mut self,
        tmux: &Tmux,
        mut effect_allowed: impl FnMut(&tma_tmux::persistence::PaneRead) -> bool,
    ) -> Result<(), TmuxError> {
        self.reconciliations += 1;
        self.reap();
        let mut panes = tmux.list_panes()?;
        let now = tma_runtime::now_ms();
        let mut live: HashSet<String> = HashSet::new();
        // `-F` support, probed once and only when a marker-lane trigger is configured (a
        // reader-model dispatch deciding off each reconcile's read, so the daemon inherits both
        // notifications without a new lane).
        let mut guarded_supported: Option<bool> = None;

        for rec in &panes {
            let read = tma_tmux::persistence::PaneRead::from_options(
                rec.pane_id.clone(),
                rec.options.clone(),
            );
            if read.program.program_origin && !effect_allowed(&read) {
                continue;
            }
            let Some(stored) = tma_core::persistence::current_stamp(&rec.options, now)
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
            let trigger = if rec
                .options
                .contains_key(tma_core::persistence::opt::ATTENTION_EPISODE)
            {
                trigger_for(
                    stored.state,
                    stored.state == AgentState::Blocked || stored.attention,
                )
                .filter(|trigger| trigger_enabled(&self.on, *trigger))
            } else {
                fire_trigger(&stored, &self.on)
            };
            if let Some(trigger) = trigger {
                self.fire_for(tmux, rec, &stored, now, trigger, &mut effect_allowed);
            }

            // Context-high dispatch: read the gauge + armed flag off the same list-panes and
            // decide, on the pane's own `@agent_context_notified_at` marker. Independent of the state
            // lane above.
            if let Some(threshold) = self.context_high {
                let g = *guarded_supported
                    .get_or_insert_with(|| tma_tmux::stamp::guarded_writes_supported(tmux, &panes));
                self.fire_context_high(tmux, rec, threshold, g, now, &mut effect_allowed);
            }

            // Stall dispatch: the same shape on the pane's own `@agent_stall_notified_at` marker,
            // measuring `now - @agent_since` while the pane reads `working`.
            if let Some(threshold_ms) = self.stall_ms {
                let g = *guarded_supported
                    .get_or_insert_with(|| tma_tmux::stamp::guarded_writes_supported(tmux, &panes));
                self.fire_stall(tmux, rec, threshold_ms, g, now, &mut effect_allowed);
            }
        }

        // Prune the last-seen map to live agent panes: a closed/exited pane drops its entry.
        self.last_state.retain(|p, _| live.contains(p));
        // Keep denied targets available for clearing an existing indicator, but
        // never promote their stale working state into a new progress effect.
        for rec in &mut panes {
            let read = tma_tmux::persistence::PaneRead::from_options(
                rec.pane_id.clone(),
                rec.options.clone(),
            );
            if read.program.program_origin && !effect_allowed(&read) {
                rec.options.remove(opt::STATE);
            }
        }
        self.dispatch_progress_checked(tmux, &panes, &mut effect_allowed);
        Ok(())
    }

    /// The `notify.osc_progress` lane, off the same `list-panes` read: write the working edges this
    /// pass produced and remember the new set. With the sink off it only takes down whatever a
    /// previously-enabled pass left lit, then costs nothing (the set stays empty).
    fn dispatch_progress_checked(
        &mut self,
        tmux: &Tmux,
        panes: &[PaneRecord],
        effect_allowed: &mut impl FnMut(&tma_tmux::persistence::PaneRead) -> bool,
    ) {
        if !self.sinks.osc_progress {
            if !self.working_windows.is_empty() {
                self.clear_progress(tmux, panes);
            }
            return;
        }
        let lanes = progress::progress_panes(panes);
        let (mut working, edges) = progress::progress_edges(&self.working_windows, &lanes);
        for (pane, state) in edges {
            let Some(rec) = panes.iter().find(|rec| rec.pane_id == pane) else {
                continue;
            };
            let read =
                tma_tmux::persistence::PaneRead::from_options(pane.clone(), rec.options.clone());
            if !read.program.program_origin || state == tma_tmux::tmux::Progress::Clear {
                tmux.osc_progress(&pane, state);
                self.progress_edges += 1;
                continue;
            }
            if tmux.osc_progress_checked(&pane, state, || effect_allowed(&read)) {
                self.progress_edges += 1;
            } else {
                let key = (rec.session.clone(), rec.window_index);
                if self.working_windows.contains(&key) {
                    working.insert(key);
                } else {
                    working.remove(&key);
                }
            }
        }
        self.working_windows = working;
    }

    /// Take every remembered window's progress indicator down and forget them.
    fn clear_progress(&mut self, tmux: &Tmux, panes: &[PaneRecord]) {
        let lanes = progress::progress_panes(panes);
        for (pane, state) in progress::clear_edges(&self.working_windows, &lanes) {
            tmux.osc_progress(&pane, state);
            self.progress_edges += 1;
        }
        self.working_windows.clear();
    }

    /// The daemon's shutdown pass for the progress lane: one `list-panes` and a clear per window
    /// tma lit. A lit tab is state the emulator keeps, so it has to be handed back explicitly.
    pub(crate) fn shutdown_progress(&mut self, tmux: &Tmux) {
        if self.working_windows.is_empty() {
            return;
        }
        let Ok(panes) = tmux.list_panes() else {
            return;
        };
        self.clear_progress(tmux, &panes);
    }

    /// Write-before-fire for one blocked pane: commit `@agent_notified_at` FIRST, and fire only on a
    /// successful commit, so a failed marker write (server gone) never yields an un-deduped fire.
    fn fire_for(
        &mut self,
        tmux: &Tmux,
        rec: &PaneRecord,
        _stored: &StampedState,
        now: u64,
        trigger: NotifyTrigger,
        effect_allowed: &mut impl FnMut(&tma_tmux::persistence::PaneRead) -> bool,
    ) {
        let read =
            tma_tmux::persistence::PaneRead::from_options(rec.pane_id.clone(), rec.options.clone());
        let Some(observation) = tma_tmux::persistence::AttentionObservation::from_read(read, now)
        else {
            return;
        };
        if observation.read.program.program_origin && !effect_allowed(&observation.read) {
            return;
        }
        let Ok(mut claim) =
            tma_tmux::persistence::claim_observation(tmux, &observation, tma_runtime::now_ms)
        else {
            return;
        };
        // A timed-out hook sender can refresh this episode after the daemon's decision read.
        // There may be no later IPC wake. Retry once, only within the exact current observation.
        if !claim.claimed() {
            let current = claim.winner();
            let same_raw = |key| current.options.get(key) == observation.read.options.get(key);
            if same_raw(tma_core::persistence::opt::RECEIPT)
                || !same_raw(tma_core::persistence::opt::METADATA_OWNER)
                || !same_raw(opt::ATTENTION)
            {
                return;
            }
            let now = tma_runtime::now_ms();
            if observation.read.program.program_origin
                && !matches!(
                    observation.read.projection(now),
                    Ok(tma_core::persistence::ProjectionRead::Live { .. })
                )
            {
                return;
            }
            let Some(current) =
                tma_tmux::persistence::AttentionObservation::from_read(current.clone(), now)
            else {
                return;
            };
            if current.identity != observation.identity {
                return;
            }
            if current.read.program.program_origin && !effect_allowed(&current.read) {
                return;
            }
            let Ok(retried) =
                tma_tmux::persistence::claim_observation(tmux, &current, tma_runtime::now_ms)
            else {
                return;
            };
            claim = retried;
            if !claim.claimed() {
                return;
            }
        }
        let mut winner = rec.clone();
        winner.options = claim.winner().options.clone();
        let rec = &winner;
        let Some(current) =
            tma_core::persistence::current_stamp(&rec.options, tma_runtime::now_ms())
                .ok()
                .flatten()
        else {
            return;
        };
        let stored = current.into_inner();
        if !effect_allowed(claim.winner()) {
            return;
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
    #[allow(clippy::too_many_arguments)]
    fn fire_context_high(
        &mut self,
        tmux: &Tmux,
        rec: &PaneRecord,
        threshold: u8,
        guarded: bool,
        now: u64,
        effect_allowed: &mut impl FnMut(&tma_tmux::persistence::PaneRead) -> bool,
    ) {
        let command = self.commands.for_context_high().map(str::to_string);
        let read =
            tma_tmux::persistence::PaneRead::from_options(rec.pane_id.clone(), rec.options.clone());
        if let Some(child) = evaluate_context_high_checked(
            tmux,
            guarded,
            rec,
            threshold,
            command.as_deref(),
            &self.sinks,
            now,
            || effect_allowed(&read),
        ) {
            self.track_child(child, command);
            self.fires += 1;
        }
    }

    /// Stall dispatch for one pane: read the state tuple + armed flag and run the shared
    /// [`evaluate_stall`], which arms-and-fires (guarded, read-back) or rearms. Its own armed flag,
    /// never the state lane's `@agent_notified_at` and never the context lane's marker.
    #[allow(clippy::too_many_arguments)]
    fn fire_stall(
        &mut self,
        tmux: &Tmux,
        rec: &PaneRecord,
        threshold_ms: u64,
        guarded: bool,
        now: u64,
        effect_allowed: &mut impl FnMut(&tma_tmux::persistence::PaneRead) -> bool,
    ) {
        let command = self.commands.for_stall().map(str::to_string);
        let read =
            tma_tmux::persistence::PaneRead::from_options(rec.pane_id.clone(), rec.options.clone());
        if let Some(child) = evaluate_stall_checked(
            tmux,
            guarded,
            rec,
            threshold_ms,
            command.as_deref(),
            &self.sinks,
            now,
            || effect_allowed(&read),
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
            "notify_reconciliations={}\nnotify_fires={}\ntransitions_recorded={}\nhistory_len={}\nhistory_cap={}\n\
             notify_pending={}\nprogress_edges={}\n",
            self.reconciliations,
            self.fires,
            self.transitions_recorded,
            self.history.len(),
            HISTORY_CAP,
            self.pending.len(),
            self.progress_edges,
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
#[cfg(test)]
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
            osc_777: true,
            osc_progress: true,
            log: None,
            include_title: false,
        };
        ns.reconfigure(
            NotifyCommands::default(),
            blocked_and_done(),
            both.clone(),
            None,
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

#[cfg(test)]
mod program_adoption_tests {
    use super::*;
    use tma_core::persistence::{PaneOwner, PersistedResult, ProcessChain};
    use tma_core::program_status::{
        EpisodeId, RecordId, ResultGeneration, ResultIdentity, ResultKind, WorkCycle,
    };
    use tma_core::render::{Guard, Publish};
    use tma_core::snapshot::ProcessIdentity;
    use tma_tmux::persistence::{self as store, OptionEdit};

    fn program_panes(count: usize) -> (tma_test_support::Scratch, Tmux) {
        let scratch = tma_test_support::Scratch::new("b6_notify_checks");
        for index in 0..count {
            assert!(scratch
                .tmux(&[
                    "new-session",
                    "-d",
                    "-s",
                    &format!("owned-{index}"),
                    "exec /bin/sleep 120"
                ])
                .status
                .success());
        }
        let tmux = Tmux::new(Some(scratch.socket.clone()));
        let rows = tmux.list_panes().unwrap();
        let token = store::fresh_token().unwrap();
        assert!(matches!(
            store::acquire_collector(&tmux, &rows[0].pane_id, &token, tma_runtime::now_ms).unwrap(),
            store::LeaseOutcome::Won(_)
        ));
        for (index, row) in rows.iter().enumerate() {
            let sample = tma_tmux::tmux::sample_program_process(row.pane_pid).unwrap();
            let owner = PaneOwner {
                collector: token.clone(),
                episode: EpisodeId::new(index as u64 + 1),
                stream_epoch: 1,
                processes: ProcessChain::new(vec![ProcessIdentity {
                    pid: row.pane_pid,
                    birth: sample.birth,
                }])
                .unwrap(),
            };
            let now = tma_runtime::now_ms();
            let publish = Publish {
                pane_id: row.pane_id.clone(),
                state: AgentState::Working,
                detail: None,
                source: Provenance::Hook,
                evidence_at: now - 2000,
                since: now - 2000,
                stamped_at: now,
                hash: None,
                pid: row.pane_pid,
                name: "reporter".into(),
                set_attention: false,
                episode_reset: false,
                guard: Guard::Unconditional,
            };
            assert!(store::publish_program(
                &tmux,
                &store::read_pane(&tmux, &row.pane_id).unwrap(),
                &owner,
                tma_runtime::now_ms,
                &publish,
                None,
                &[]
            )
            .unwrap()
            .won());
        }
        (scratch, tmux)
    }

    fn deny_after_io(scratch: &tma_test_support::Scratch, pattern: &str) -> Tmux {
        use std::os::unix::fs::PermissionsExt;
        let wrapper = scratch.workdir.join("deny-later-notify-io");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
for arg do
case "$arg" in
*'set-option'*'@agent_context_notified_at'*|*'set-option'*'@agent_stall_notified_at'*|*'#{{pane_tty}}'*) printf 'io\n' >> '{calls}'; break ;;
esac
done
for arg do
case "$arg" in
{pattern})
if [ ! -e '{denied}' ]; then
tmux "$@"; tma_notify_result=$?
printf '%s\n' "$tma_notify_result" > '{done}'
touch '{denied}'; sleep 0.1; exit "$tma_notify_result"
fi
break ;;
esac
done
exec tmux "$@"
"#,
                calls = scratch.workdir.join("notify-io-calls").display(),
                denied = scratch.workdir.join("notify-denied").display(),
                done = scratch.workdir.join("notify-first-io-done").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Tmux::connect(&tma_tmux::tmux::Server {
            socket_name: Some(scratch.socket.clone()),
            bin: Some(wrapper.to_string_lossy().into_owned()),
            ..Default::default()
        })
    }

    #[test]
    fn denied_program_continuation_does_not_start_a_later_marker_claim_or_rearm() {
        let _gate = tma_test_support::DaemonTestGuard::acquire();
        if !tma_test_support::tmux_available() {
            return;
        }
        let (scratch, plain) = program_panes(1);
        let pane = plain.list_panes().unwrap()[0].pane_id.clone();
        plain
            .set_pane_option(&pane, opt::CONTEXT_PCT, "90")
            .unwrap();
        plain
            .set_pane_option(&pane, opt::CONTEXT_AT, &tma_runtime::now_ms().to_string())
            .unwrap();
        let tmux = deny_after_io(&scratch, "*'set-option'*'@agent_context_notified_at'*");
        let mut notify = NotifyState::new(
            NotifyCommands::default(),
            vec![],
            NotifySinks::default(),
            Some(75),
            Some(1),
        );
        notify
            .reconcile_checked(&tmux, |_| !scratch.workdir.join("notify-denied").exists())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(scratch.workdir.join("notify-first-io-done"))
                .unwrap()
                .trim(),
            "0"
        );
        let calls = std::fs::read_to_string(scratch.workdir.join("notify-io-calls")).unwrap();
        assert_eq!(
            calls.lines().count(),
            1,
            "later program marker must not start after earlier IO denies continuation"
        );
        assert!(plain.list_panes().unwrap()[0]
            .options
            .contains_key(opt::CONTEXT_NOTIFIED_AT));
        assert!(!plain.list_panes().unwrap()[0]
            .options
            .contains_key(opt::STALL_NOTIFIED_AT));

        std::fs::remove_file(scratch.workdir.join("notify-denied")).unwrap();
        std::fs::write(scratch.workdir.join("notify-io-calls"), "").unwrap();
        plain.set_pane_option(&pane, opt::CONTEXT_PCT, "1").unwrap();
        plain.set_pane_option(&pane, opt::STATE, "idle").unwrap();
        plain
            .set_pane_option(&pane, opt::STALL_NOTIFIED_AT, "1")
            .unwrap();
        notify
            .reconcile_checked(&tmux, |_| !scratch.workdir.join("notify-denied").exists())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(scratch.workdir.join("notify-io-calls"))
                .unwrap()
                .lines()
                .count(),
            1,
            "later program rearm must not start after earlier IO denies continuation"
        );
        assert!(plain.list_panes().unwrap()[0]
            .options
            .contains_key(opt::STALL_NOTIFIED_AT));
    }

    #[test]
    fn denied_program_continuation_does_not_start_a_later_progress_edge() {
        let _gate = tma_test_support::DaemonTestGuard::acquire();
        if !tma_test_support::tmux_available() {
            return;
        }
        let (scratch, plain) = program_panes(2);
        let tmux = deny_after_io(&scratch, "*'#{pane_tty}'*");
        let mut notify = NotifyState::new(
            NotifyCommands::default(),
            vec![],
            NotifySinks {
                osc_progress: true,
                ..Default::default()
            },
            None,
            None,
        );
        notify
            .reconcile_checked(&tmux, |_| !scratch.workdir.join("notify-denied").exists())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(scratch.workdir.join("notify-first-io-done"))
                .unwrap()
                .trim(),
            "0"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.workdir.join("notify-io-calls"))
                .unwrap()
                .lines()
                .count(),
            1,
            "later progress edge must recheck after the first native tty read"
        );
        assert_eq!(
            notify.working_windows.len(),
            0,
            "the first edge is denied after its own slow tty lookup"
        );
        assert_eq!(notify.progress_edges, 0);
        notify.reconcile_checked(&plain, |_| true).unwrap();
        assert_eq!(notify.working_windows.len(), 2);
        assert_eq!(notify.progress_edges, 2);
    }

    #[test]
    fn native_direct_and_daemon_claims_dedup_and_failure_and_muted_results_are_spent() {
        let _gate = tma_test_support::DaemonTestGuard::acquire();
        if !tma_test_support::tmux_available() {
            return;
        }
        let scratch = tma_test_support::Scratch::new("b5b_notify");
        assert!(scratch
            .tmux(&["new-session", "-d", "-s", "owned", "exec /bin/sleep 120"])
            .status
            .success());
        let pane = scratch.get("owned", "#{pane_id}");
        let tmux = Tmux::new(Some(scratch.socket.clone()));
        let log = scratch.workdir.join("notify.jsonl");
        let sinks = NotifySinks {
            log: Some(log.clone()),
            ..NotifySinks::default()
        };
        let on = vec![NotifyTrigger::Blocked, NotifyTrigger::Done];
        let commands = NotifyCommands::default();
        let manifests = tma_runtime::manifests::load(None, &[]).unwrap().manifests;
        let manifest = manifests
            .iter()
            .find(|manifest| manifest.name == "claude")
            .unwrap();
        let direct = tma_runtime::event::NotifyPolicy {
            opt_in: true,
            on: &on,
            commands: &commands,
            sinks: &sinks,
        };
        tma_runtime::event::apply_event(
            &tmux,
            manifest,
            &pane,
            "PermissionRequest",
            r#"{"session_id":"same","tool_name":"Bash","tool_use_id":"call"}"#,
            &direct,
            tma_runtime::now_ms(),
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        let mut daemon = NotifyState::new(commands.clone(), on, sinks.clone(), None, None);
        daemon.reconcile(&tmux).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        let token = store::fresh_token().unwrap();
        assert!(matches!(
            store::acquire_collector(&tmux, &pane, &token, tma_runtime::now_ms).unwrap(),
            store::LeaseOutcome::Won(_)
        ));
        let pid = scratch.get(&pane, "#{pane_pid}").parse().unwrap();
        let sample = tma_tmux::tmux::sample_program_process(pid).unwrap();
        let owner = PaneOwner {
            collector: token,
            episode: EpisodeId::new(1),
            stream_epoch: 1,
            processes: ProcessChain::new(vec![ProcessIdentity {
                pid,
                birth: sample.birth,
            }])
            .unwrap(),
        };
        let now = tma_runtime::now_ms();
        let publish = Publish {
            pane_id: pane.clone(),
            state: AgentState::Idle,
            detail: Some(tma_core::Detail::new(tma_core::Detail::ERROR)),
            source: Provenance::Hook,
            evidence_at: now,
            since: now,
            stamped_at: now,
            hash: None,
            pid,
            name: "claude".into(),
            set_attention: true,
            episode_reset: false,
            guard: Guard::Unconditional,
        };
        let mut result = PersistedResult {
            owner: owner.clone(),
            identity: ResultIdentity {
                owner: owner.episode,
                record: RecordId::root(),
                generation: ResultGeneration::new(1),
            },
            cycle: WorkCycle::new(0),
            kind: ResultKind::Failure,
        };
        let observed = store::read_pane(&tmux, &pane).unwrap();
        assert!(store::publish_program(
            &tmux,
            &observed,
            &owner,
            tma_runtime::now_ms,
            &publish,
            Some(&result),
            &[OptionEdit::set(opt::TURN_AT, now.to_string())]
        )
        .unwrap()
        .won());
        daemon.reconcile(&tmux).unwrap();
        daemon.reconcile(&tmux).unwrap();
        let contents = std::fs::read_to_string(&log).unwrap();
        assert_eq!(contents.lines().count(), 2);
        assert!(contents
            .lines()
            .last()
            .unwrap()
            .contains("\"detail\":\"error\""));
        let record = tmux.list_panes().unwrap().remove(0);
        let notification = notification_for(
            &record,
            "claude",
            "done",
            Some("error".into()),
            None,
            now,
            now,
        );
        assert_eq!(notification.state, "done");
        assert_eq!(notification.display_state(), "failed");
        tmux.set_pane_option(
            &pane,
            opt::MUTE_UNTIL,
            &now.saturating_add(60_000).to_string(),
        )
        .unwrap();
        result.identity.generation = ResultGeneration::new(2);
        let observed = store::read_pane(&tmux, &pane).unwrap();
        assert!(store::publish_program(
            &tmux,
            &observed,
            &owner,
            tma_runtime::now_ms,
            &publish,
            Some(&result),
            &[OptionEdit::set(opt::TURN_AT, (now + 1).to_string())]
        )
        .unwrap()
        .won());
        daemon.reconcile(&tmux).unwrap();
        assert_eq!(
            store::read_pane(&tmux, &pane)
                .unwrap()
                .program
                .notified
                .unwrap()
                .result,
            result
        );
        tmux.unset_pane_option(&pane, opt::MUTE_UNTIL).unwrap();
        daemon.reconcile(&tmux).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
    }
}
