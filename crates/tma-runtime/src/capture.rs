//! On-demand capture, the reconciliation sweep, and hook-liveness demotion. Daemon-only and
//! strictly additive (with no daemon, hookless panes fall back to [`crate::cycle::run_cycle`]);
//! events drive state, the sweep repairs it. On-demand ([`CaptureState::handle_edges`]): `%output`
//! bursts become per-pane active→quiet edges, captured one pane per edge (the hookless `blocked`
//! catch), never a fan-out. The sweep ([`CaptureState::run_sweep`]) runs the full poll cycle every
//! 30-60 s and is the ONLY multi-`capture-pane` fan-out (discovers agents, clears dead ones, corrects
//! drift). Hook-liveness demotion (the edge-count rule on [`DEMOTE_EDGES`]) handles wiring that dies
//! silently: it needs the persistent per-pane memory only the daemon holds.

use std::collections::HashMap;
use std::time::Instant;

use tma_core::evidence::Provenance;
use tma_core::snapshot::PaneSnapshot;
use tma_core::stamp::opt;
use tma_core::{AgentState, FoldConfig, ReadResult, StampedState};

use tma_tmux::control::ActivityEdge;
use tma_tmux::stamp;
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

use crate::cycle;
use crate::debug::fnv1a64;
use crate::identity::{self, PaneIdentity, Registration};
use crate::manifests::LoadedManifest;

/// Default demotion threshold (config `[daemon] demote_edges`): consecutive activity edges with no
/// hook-fresh stored claim (see [`CaptureState::note_edge`]) after which a hook-capable pane's
/// coverage goes suspect. Five, not three, because one long tool call legitimately emits several
/// active→quiet edges (streaming pauses) with no hook between (hooks fire at Pre/PostToolUse, not
/// mid-stream); a lower threshold would falsely demote a live pane.
pub(crate) const DEMOTE_EDGES: u32 = 5;

/// Cap on the per-pane hook-liveness map. Only hook-capable panes are tracked and the sweep prunes
/// dead ones, so this is a churn ceiling, not a steady-state size. When full, the least-recently-
/// touched entry is dropped (a lost demotion self-heals on the next edges / sweep).
const MAX_TRACKED: usize = 4096;

/// Per-pane hook-liveness memory (daemon-only). Bounded and pruned.
#[derive(Clone, Debug, Default)]
struct PaneHook {
    /// Consecutive activity edges observed for this pane with no intervening hook event.
    edges_since_hook: u32,
    /// `true` once `edges_since_hook >= DEMOTE_EDGES`: capture verdicts write unguarded.
    demoted: bool,
    /// A monotone touch stamp for the [`MAX_TRACKED`] eviction (loop-iteration counter).
    touched: u64,
}

/// The daemon's on-demand + sweep state. Owned by the serve loop (single-threaded); no interior
/// mutability.
pub struct CaptureState {
    /// Fold tuning, injected from `[fold]` config (constructed in the bin, not core).
    cfg: FoldConfig,
    /// Demotion threshold (config `[daemon] demote_edges`; defaults to [`DEMOTE_EDGES`]).
    demote_edges: u32,
    /// Per-pane hook-liveness counters, hook-capable panes only.
    hooks: HashMap<String, PaneHook>,
    /// Monotone touch clock for [`MAX_TRACKED`] eviction.
    clock: u64,
    /// `-F` conditional-write support, resolved once and cached (see [`stamp`]).
    guarded: Option<bool>,
    /// The last sweep's deferred ordered-input clear: `(pane_id, @agent_since)` for every pane still
    /// carrying `@agent_attention`. Drained by the serve loop AFTER notification dispatch, which
    /// reads the same persisted flag (see [`cycle::SeenClear`]).
    deferred_seen: Vec<(String, u64)>,

    // ---- introspection counters (status file; tests + operators) ----
    /// Total on-demand single-pane captures (quiet edge + contradiction).
    on_demand_captures: u64,
    /// On-demand captures triggered by a hookless (or demoted) pane's quiet edge.
    quiet_captures: u64,
    /// On-demand captures triggered by a hook `working` vs quiet contradiction.
    contradiction_captures: u64,
    /// Edges whose pane died between `list-panes` and its capture/stamp (`TmuxError::Failed`):
    /// skipped so the batch's surviving sibling edges still fire. Monotone.
    skipped_dead_edges: u64,
    /// Panes demoted over the daemon's life (monotone).
    demotions: u64,
    /// Reconciliation sweeps run (monotone).
    sweeps: u64,
    /// `capture-pane` calls in the most recent sweep — the fan-out count (= agent count).
    sweep_captures: usize,
    /// Agent rows the most recent sweep resolved (discovery / clear observation).
    sweep_agents: usize,
    /// Wall time of the most recent sweep, milliseconds.
    last_sweep_wall_ms: u128,
}

impl CaptureState {
    pub fn new(cfg: FoldConfig, demote_edges: u32) -> CaptureState {
        CaptureState {
            cfg,
            demote_edges,
            hooks: HashMap::new(),
            clock: 0,
            guarded: None,
            deferred_seen: Vec::new(),
            on_demand_captures: 0,
            quiet_captures: 0,
            contradiction_captures: 0,
            skipped_dead_edges: 0,
            demotions: 0,
            sweeps: 0,
            sweep_captures: 0,
            sweep_agents: 0,
            last_sweep_wall_ms: 0,
        }
    }

    /// Swap the config-derived fold tuning + demotion threshold (SIGHUP reload). The hook-liveness
    /// map, the cached `-F` probe, and the counters are preserved, so a reload retunes without losing
    /// demotion state or re-probing tmux.
    pub fn set_config(&mut self, cfg: FoldConfig, demote_edges: u32) {
        self.cfg = cfg;
        self.demote_edges = demote_edges;
    }

    /// A hook event arrived for `pane` (proof its hooks are live): reset the edge counter and restore
    /// guarded behaviour. Called from the socket handler after the shared stamp applies.
    pub fn on_hook_event(&mut self, pane: &str) {
        if let Some(h) = self.hooks.get_mut(pane) {
            h.edges_since_hook = 0;
            h.demoted = false;
        }
    }

    /// Drain and act on this iteration's activity edges (the on-demand tier): at most one capture
    /// per edge, never a fan-out. A batch shares one `list-panes` + one lazy `ps`; `capture-pane`
    /// spawns only for the pane whose edge/contradiction fires. `ServerGone` propagates.
    pub fn handle_edges(
        &mut self,
        tmux: &Tmux,
        manifests: &[LoadedManifest],
        edges: Vec<ActivityEdge>,
    ) -> Result<(), TmuxError> {
        if edges.is_empty() {
            return Ok(());
        }
        let panes = tmux.list_panes()?; // one read for the whole batch (not a per-edge fan-out)
        let now = crate::now_ms();
        let mut procs = None; // parsed once, lazily, only if an edge reaches an agent pane

        for edge in edges {
            let Some(rec) = panes.iter().find(|r| r.pane_id == edge.pane) else {
                // The pane is gone; drop its hook-liveness memory (the sweep clears any residue).
                self.hooks.remove(&edge.pane);
                continue;
            };

            // Ignored panes never get captured: the poll cycle owns clearing whatever stamp they
            // still carry, and reading a dev server's screen here would only re-detect it.
            if identity::is_ignored(&rec.options) {
                self.hooks.remove(&edge.pane);
                continue;
            }

            let prev = StampedState::from_options(&rec.options)
                .ok()
                .flatten()
                .map(ReadResult::into_inner);

            // Identify honouring a stored hook registration, as the poll cycle does, so a live
            // registered agent the ps walk momentarily misses is not mistaken for an exit.
            let registration = match (
                prev.as_ref().and_then(|p| p.session.as_deref()),
                rec.options.get(opt::NAME),
            ) {
                (Some(session), Some(name)) => Some(Registration {
                    agent_name: name.clone(),
                    session: Some(session.to_string()),
                }),
                _ => None,
            };
            let procs_ref = match &procs {
                Some(p) => p,
                None => {
                    procs = Some(tma_tmux::tmux::ps_all()?);
                    procs.as_ref().unwrap()
                }
            };
            let stored_title_anchor = rec.options.get(opt::TITLE_MATCH_PID);
            let identity = identity::identify(
                rec.pane_pid,
                &rec.current_command,
                &rec.title,
                procs_ref,
                manifests,
                stored_title_anchor.and_then(|v| v.parse().ok()),
                registration.as_ref(),
            );
            let PaneIdentity::Agent(id) = identity else {
                continue; // not an agent (or remote): nothing to capture; the sweep owns removal
            };
            if id.agent_pid == 0 {
                continue; // registered but no walkable process yet: hold; the sweep clears a dead one
            }

            // Hook-capable = carries a hook registration (@agent_session) OR its manifest
            // declares `[hooks].covers`. Only these panes accrue demotion memory.
            let hook_capable = rec.options.contains_key(opt::SESSION)
                || id
                    .manifest
                    .manifest
                    .hooks
                    .as_ref()
                    .is_some_and(|h| !h.covers.is_empty());

            // False-demotion mitigation: an edge landing while the stored claim is a fresh hook
            // event (inside the `hook_decay` window) is proof the wiring is alive (a streaming pause,
            // not a dead hook), so it must not count. `prev` is already in hand, so no round-trip.
            let hook_fresh = is_hook_fresh(prev.as_ref(), now, self.cfg.hook_decay_ms());
            let demoted = if hook_capable {
                self.note_edge(&edge.pane, hook_fresh)
            } else {
                false
            };

            // Trigger: capture a hookless quiet edge (the blocked-catch moment), a demoted pane
            // (suspect hook claim), or a hook-`working` contradiction; skip a fresh hook idle/blocked
            // (hooks drive it, spare the budget).
            let prev_hook_working = prev
                .as_ref()
                .is_some_and(|p| p.source == Provenance::Hook && p.state == AgentState::Working);
            let contradiction = hook_capable && prev_hook_working;
            if hook_capable && !demoted && !contradiction {
                continue;
            }

            match self.capture_one(tmux, &panes, rec, prev.as_ref(), &id, demoted, now) {
                Ok(()) => {}
                // The pane died between `list-panes` and its own capture/stamp: skip it, keep
                // draining so a blocked sibling still fires in the SLA, and drop its demotion memory.
                // Only `ServerGone` aborts.
                Err(e) if cycle::is_dead_pane(&e) => {
                    self.hooks.remove(&edge.pane);
                    self.skipped_dead_edges += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
            // Persist the flicker anchor for a title-narrowed match (parity with the poll cycle), so
            // an unregistered cursor pane holds identity through the flicker. Registered panes carry none.
            if id.title_match_pid.is_some() {
                if let Some(cmd) = identity::title_anchor_command(
                    &rec.pane_id,
                    stored_title_anchor.map(String::as_str),
                    id.title_match_pid,
                ) {
                    let _ = tmux.apply(&[cmd]);
                }
            }
            self.on_demand_captures += 1;
            if contradiction {
                self.contradiction_captures += 1;
            } else {
                self.quiet_captures += 1;
            }
        }
        Ok(())
    }

    /// Capture, fold, and guarded-stamp exactly one pane (the on-demand producer core, reusing
    /// [`cycle::run_cycle`]'s per-pane path). The single `capture-pane` is this edge's whole budget.
    /// Resolves the daemon's cached `-F` probe, then hands the nine shared steps to
    /// [`stamp_from_capture`]; the broker's re-verify gate drives the same primitive.
    #[allow(clippy::too_many_arguments)]
    fn capture_one(
        &mut self,
        tmux: &Tmux,
        all_panes: &[PaneRecord],
        rec: &PaneRecord,
        prev: Option<&StampedState>,
        id: &identity::Identified,
        demoted: bool,
        now: u64,
    ) -> Result<(), TmuxError> {
        let guarded = *self
            .guarded
            .get_or_insert_with(|| stamp::guarded_writes_supported(tmux, all_panes));
        stamp_from_capture(
            tmux, all_panes, rec, prev, id, &self.cfg, demoted, guarded, now,
        )
    }

    /// The reconciliation sweep: the full poll cycle ([`cycle::run_cycle`]), the only multi-
    /// `capture-pane` fan-out. Self-healing only (discovers agents, clears dead ones, corrects hook
    /// drift). Records the capture/agent count + wall time and prunes the hook map to live panes.
    ///
    /// The cycle's ordered-input clear is DEFERRED here rather than run inline: the serve loop
    /// dispatches notifications after the sweep, from the same persisted `@agent_attention`, so a
    /// clear inside the sweep could retire a completion nobody had been told about yet. The panes
    /// wait in [`CaptureState::take_deferred_seen`].
    pub fn run_sweep(
        &mut self,
        tmux: &Tmux,
        manifests: &[LoadedManifest],
    ) -> Result<(), TmuxError> {
        let start = Instant::now();
        let report = cycle::run_cycle_with(tmux, manifests, &self.cfg, cycle::SeenClear::Deferred)?;
        self.deferred_seen = report.deferred_seen;
        self.last_sweep_wall_ms = start.elapsed().as_millis();
        self.sweeps += 1;
        self.sweep_captures = report.captures;
        self.sweep_agents = report.rows.len();

        // Prune hook-liveness memory to currently-live agent panes. A pane no longer resolved as
        // an agent (killed, exited) drops its counter here.
        let mut live = std::collections::HashSet::new();
        for r in &report.rows {
            live.insert(r.pane_id.clone());
        }
        self.hooks.retain(|pane, _| live.contains(pane));
        Ok(())
    }

    /// Take the sweep's deferred ordered-input-clear candidates, leaving none behind: the caller
    /// owns the pass and a candidate must never be replayed against a later sweep's flags.
    pub fn take_deferred_seen(&mut self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.deferred_seen)
    }

    /// Record one activity edge against a hook-capable pane's counter; returns whether it is now
    /// demoted, and bounds the map. When `hook_fresh` (a still-fresh stored hook claim), the edge
    /// does NOT advance the counter (the wiring is proven live), but the entry is still touched for
    /// eviction ranking and its demotion state returned unchanged.
    fn note_edge(&mut self, pane: &str, hook_fresh: bool) -> bool {
        self.clock += 1;
        let clock = self.clock;
        // Evict the least-recently-touched entry if inserting a new pane would exceed the cap.
        if !self.hooks.contains_key(pane) && self.hooks.len() >= MAX_TRACKED {
            if let Some(oldest) = self
                .hooks
                .iter()
                .min_by_key(|(_, h)| h.touched)
                .map(|(k, _)| k.clone())
            {
                self.hooks.remove(&oldest);
            }
        }
        let entry = self.hooks.entry(pane.to_string()).or_default();
        entry.touched = clock;
        if hook_fresh {
            // A live hook claim: this edge is not evidence of dead wiring. Hold the counter.
            return entry.demoted;
        }
        entry.edges_since_hook = entry.edges_since_hook.saturating_add(1);
        if !entry.demoted && entry.edges_since_hook >= self.demote_edges {
            entry.demoted = true;
            self.demotions += 1;
        }
        entry.demoted
    }

    /// The number of panes currently demoted: the daemon's live broken-hook-wiring signal (hooks
    /// stopped firing while output continued). `tma install-hooks --check` is the static detector for
    /// the daemonless tiers.
    fn demoted_now(&self) -> usize {
        self.hooks.values().filter(|h| h.demoted).count()
    }

    /// The `key=value` introspection lines for the daemon status file, so acceptance tests can assert
    /// on-demand capture counts, the demotion state machine, and the sweep fan-out + wall time.
    pub fn status_lines(&self) -> String {
        format!(
            "on_demand_captures={}\nquiet_captures={}\ncontradiction_captures={}\n\
             skipped_dead_edges={}\ndemoted={}\ndemotions={}\nsweeps={}\nsweep_captures={}\n\
             sweep_agents={}\nlast_sweep_wall_ms={}\n",
            self.on_demand_captures,
            self.quiet_captures,
            self.contradiction_captures,
            self.skipped_dead_edges,
            self.demoted_now(),
            self.demotions,
            self.sweeps,
            self.sweep_captures,
            self.sweep_agents,
            self.last_sweep_wall_ms,
        )
    }
}

/// The single-pane guarded-stamp primitive: capture the pane once, evaluate
/// the agent's rules, fold fresh evidence over the prior claim, and land the verdict through the
/// conditional-write stamp adapter. This is the ONE capture-sourced producer of a stamp; both the
/// daemon's on-demand [`CaptureState::capture_one`] and the broker's re-verify gate
/// (`broker::tmux_io::reverify_pane`) drive it, so the guard semantics cannot drift
/// between the two callers. The caller resolves identity, the prior stamp, and `-F` guard support;
/// this owns the nine shared steps from `capture-pane` to `stamp::apply`.
///
/// `demoted` is daemon-only (the broker always passes `false`): it folds a stale hook claim as a
/// capture prior and forces the write unconditional, so a demoted pane's fresh verdict lands over
/// the suppressing `@agent_source` guard. Both halves are the same hook-ownership rule.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stamp_from_capture(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    rec: &PaneRecord,
    prev: Option<&StampedState>,
    id: &identity::Identified,
    cfg: &FoldConfig,
    demoted: bool,
    guarded: bool,
    now: u64,
) -> Result<(), TmuxError> {
    let tail_text = tmux.capture_pane(&rec.pane_id)?; // the one and only on-demand capture
    let tail_hash = fnv1a64(tail_text.as_bytes());
    let snapshot = PaneSnapshot {
        pane_id: rec.pane_id.clone(),
        pid_tree: Vec::new(),
        title: rec.title.clone(),
        tail_hash,
        tail_text,
        alternate_on: rec.alternate_on,
        scroll_position: rec.scroll_position,
        // Clamp `Region::Visible` rules to the visible screen (0 ⇒ None = whole tail).
        visible_height: (rec.pane_height != 0).then_some(rec.pane_height),
        captured_at: now,
    };
    let evaluation = id.manifest.engine.evaluate(&snapshot);
    let evidence = &evaluation.evidence;
    let facts = tma_core::SnapshotFacts {
        pid: id.agent_pid,
        foreground_is_agent: id.foreground_is_agent,
        scrolled: snapshot.scrolled(),
        history_view: evaluation.history_view,
    };

    let fold_prev = match (demoted, prev) {
        (true, Some(p)) => {
            let mut p = p.clone();
            if p.source == Provenance::Hook {
                p.source = Provenance::Capture;
            }
            Some(p)
        }
        (_, p) => p.cloned(),
    };

    let mut verdict =
        tma_core::verdict(fold_prev, &facts, evidence, &id.manifest.manifest, cfg, now);
    if demoted {
        // Force `Guard::Unconditional` via the stamp adapter (`may_override`), bypassing the
        // source guard so the capture verdict actually lands over the stale hook stamp.
        verdict.writes.may_override = true;
    }

    let plan = stamp::plan_from_verdict(
        &rec.pane_id,
        &verdict,
        id.agent_pid,
        &id.manifest.name,
        tail_hash,
        now,
    );
    stamp::apply(tmux, all_panes, &rec.pane_id, &plan, guarded)
}

/// Whether a stored claim is hook-fresh: it came from a hook event no older than the decay window,
/// so an activity edge landing on it is a streaming pause, not dead wiring, and must not advance the
/// demotion counter. `now`/`decay_ms` are threaded so the decay decision is clock-free and testable.
/// Boundary matches the fold decay rule: exactly-at `decay_ms` is still fresh (`<=`).
fn is_hook_fresh(prev: Option<&StampedState>, now: u64, decay_ms: u64) -> bool {
    prev.is_some_and(|p| {
        p.source == Provenance::Hook && now.saturating_sub(p.evidence_at) <= decay_ms
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demotes_after_n_edges_and_hook_event_resets() {
        let mut cs = CaptureState::new(FoldConfig::default(), DEMOTE_EDGES);
        // The first DEMOTE_EDGES-1 edges do not demote (default DEMOTE_EDGES = 5). None is
        // hook-fresh, so every edge counts.
        for _ in 0..DEMOTE_EDGES - 1 {
            assert!(!cs.note_edge("%1", false));
        }
        assert_eq!(cs.demoted_now(), 0);
        // The DEMOTE_EDGES-th edge crosses the threshold.
        assert!(cs.note_edge("%1", false));
        assert_eq!(cs.demoted_now(), 1);
        assert_eq!(cs.demotions, 1);
        // A hook event resets the counter and restores guarded behaviour.
        cs.on_hook_event("%1");
        assert_eq!(cs.demoted_now(), 0);
        // The counter restarts: a full DEMOTE_EDGES fresh edges are needed to re-demote (no
        // double count off the pre-reset accrual).
        for _ in 0..DEMOTE_EDGES - 1 {
            assert!(!cs.note_edge("%1", false));
        }
        assert!(cs.note_edge("%1", false));
        assert_eq!(cs.demotions, 2, "re-demotion counted once, not per-edge");
    }

    #[test]
    fn hook_fresh_edges_do_not_count_toward_demotion() {
        // False-demotion mitigation: while the stored claim is a fresh hook event, edges do not
        // accrue — a hook-capable pane running one long tool call (many streaming pauses, no
        // intervening hook) must NOT demote. Far more than DEMOTE_EDGES hook-fresh edges: none
        // count.
        let mut cs = CaptureState::new(FoldConfig::default(), DEMOTE_EDGES);
        for _ in 0..DEMOTE_EDGES * 3 {
            assert!(!cs.note_edge("%1", true), "a hook-fresh edge never demotes");
        }
        assert_eq!(cs.demoted_now(), 0);
        assert_eq!(cs.demotions, 0);
        // The entry is still tracked (touched for eviction), just not advanced.
        assert_eq!(cs.hooks.get("%1").map(|h| h.edges_since_hook), Some(0));
        // Once the hook claim goes stale (edges arrive non-fresh), counting resumes from zero and
        // the pane demotes after the full threshold — the fresh edges left no residue.
        for _ in 0..DEMOTE_EDGES - 1 {
            assert!(!cs.note_edge("%1", false));
        }
        assert!(cs.note_edge("%1", false));
        assert_eq!(cs.demoted_now(), 1);
    }

    /// A minimal stored claim for the decay-window decision: only source + evidence_at matter here.
    fn claim(source: Provenance, evidence_at: u64) -> StampedState {
        StampedState {
            state: AgentState::Working,
            detail: None,
            source,
            evidence_at,
            since: evidence_at,
            stamped_at: evidence_at,
            attention: false,
            notified_at: None,
            hash: None,
            pid: 4242,
            session: None,
            subagents: vec![],
        }
    }

    #[test]
    fn is_hook_fresh_holds_only_inside_the_decay_window() {
        let decay = 500u64;
        // No prior claim ⇒ not fresh (the edge counts).
        assert!(!is_hook_fresh(None, 1_000, decay));
        // A hook claim within the window (and exactly at the boundary) is fresh.
        assert!(is_hook_fresh(
            Some(&claim(Provenance::Hook, 700)),
            1_000,
            decay
        ));
        assert!(
            is_hook_fresh(Some(&claim(Provenance::Hook, 500)), 1_000, decay),
            "exactly at the decay boundary is still fresh"
        );
        // A hook claim past the window has decayed ⇒ not fresh, so the edge resumes counting.
        assert!(!is_hook_fresh(
            Some(&claim(Provenance::Hook, 499)),
            1_000,
            decay
        ));
        // A capture-sourced claim is never hook-fresh however recent.
        assert!(!is_hook_fresh(
            Some(&claim(Provenance::Capture, 990)),
            1_000,
            decay
        ));
    }

    #[test]
    fn hook_event_for_untracked_pane_is_a_noop() {
        let mut cs = CaptureState::new(FoldConfig::default(), DEMOTE_EDGES);
        cs.on_hook_event("%99"); // never edged; must not panic or create an entry
        assert_eq!(cs.demoted_now(), 0);
        assert!(cs.hooks.is_empty());
    }

    #[test]
    fn tracked_map_is_bounded() {
        let mut cs = CaptureState::new(FoldConfig::default(), DEMOTE_EDGES);
        for i in 0..(MAX_TRACKED + 100) {
            cs.note_edge(&format!("%{i}"), false);
        }
        assert!(
            cs.hooks.len() <= MAX_TRACKED,
            "hook-liveness map stays bounded: {}",
            cs.hooks.len()
        );
    }

    #[test]
    fn status_lines_expose_the_counters() {
        let cs = CaptureState::new(FoldConfig::default(), DEMOTE_EDGES);
        let s = cs.status_lines();
        for key in [
            "on_demand_captures=",
            "contradiction_captures=",
            "demoted=",
            "sweeps=",
            "sweep_captures=",
            "last_sweep_wall_ms=",
        ] {
            assert!(s.contains(key), "status missing {key}: {s}");
        }
    }
}
