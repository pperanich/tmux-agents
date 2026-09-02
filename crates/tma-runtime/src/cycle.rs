//! The poll cycle (tier 1): the discover → capture → detect → publish pass behind every one-shot
//! surface (`tma ls`, `tma status`, the picker's refresh).
//!
//! Consumer-first: a fresh, settled stamp (within [`FoldConfig::freshness_secs`]) is read, never
//! re-captured; only stale panes take the producer path (identify → capture → fold → guarded stamp).
//! The floor has no ambient driver, so every client's `#(tma status)` is a producer firing together;
//! the server-scoped `@tma_last_poll` hint lets one skip producing when another produced in the same
//! second (bucketed from the ms stamps). Per-pane `@agent_stamped_at` stays authoritative.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tma_core::evidence::Provenance;
use tma_core::render;
use tma_core::snapshot::{PaneSnapshot, ProcInfo};
use tma_core::stamp::opt;
use tma_core::{
    sort_rank, AgentRow, AgentState, FoldConfig, QuotaLabel, ReadResult, SnapshotFacts,
    StampedState, WriteAction,
};

use tma_tmux::stamp::{self, StampPlan};
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

use crate::debug::fnv1a64;
use crate::identity::{self, PaneIdentity, Registration};
use crate::manifests::LoadedManifest;

/// The outcome of one poll cycle: the agent rows plus instrumentation for `--debug-timing`
/// and the freshness/stampede acceptance tests.
#[derive(Debug, Default)]
pub struct CycleReport {
    pub rows: Vec<AgentRow>,
    /// `capture-pane` calls made this cycle (producer work). Zero on a pure-consumer cycle.
    pub captures: usize,
    /// Producer-path panes whose capture was skipped because nothing has been written to their
    /// window since their stamp ([`can_reuse_stamp`]). The complement of `captures`.
    pub skipped_quiet: usize,
    pub produced: usize,
    pub consumed: usize,
    pub removed: usize,
    /// Panes that died between `list-panes` and their own capture/stamp (`TmuxError::Failed`):
    /// skipped and treated as removed rather than aborting the cycle.
    pub skipped_dead: usize,
    /// Whether the stampede guard short-circuited producing this cycle.
    pub stampede_skipped: bool,
    /// Panes still carrying `@agent_attention` at the end of a [`SeenClear::Deferred`] cycle, with
    /// each one's `@agent_since`: the input for the caller's own [`crate::seen::clear_seen`] pass.
    /// Always empty on an inline cycle, which has already run that pass itself.
    pub deferred_seen: Vec<(String, u64)>,
    pub elapsed: Duration,
}

/// When a cycle runs the ordered-input clear ([`crate::seen`]).
///
/// The split is renderer vs detector, and it turns on one question: does the caller merely DISPLAY
/// the rows, or does it DECIDE something from them? A renderer wants the clear inline, so the frame
/// it paints shows the result of its own clear instead of a ✓ it has already retired. A detector —
/// the daemon's notification dispatch, `tma wait`'s goal, `tma subscribe`'s edge diff — reads
/// `@agent_attention` to decide whether a completion happened, and an inline clear retracts the mark
/// out from under that read: the completion is not announced late, it is never announced at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeenClear {
    /// Inside the cycle, before it returns: the rows already reflect it. The rendering surfaces —
    /// `ls`, `status`, the picker, `jump`.
    #[default]
    Inline,
    /// Not at all — the panes are reported in [`CycleReport::deferred_seen`] and the caller clears
    /// them once it has read the flag. The daemon defers around its notification dispatch, `wait`
    /// around its goal evaluation, `subscribe` around its emission; each would otherwise eat the
    /// very completion it exists to report.
    Deferred,
}

/// Does a per-pane tmux error mean one pane died mid-cycle (skip it) rather than the server being
/// gone (abort)? A pane killed between `list-panes` and its own `capture-pane`/stamp fails with
/// `TmuxError::Failed`; aborting on that would blank every row, so it is skipped (treated as
/// removed). Only `ServerGone` and a genuine spawn/parse failure propagate.
///
/// Deliberately looser than the broker's classifier, which reads tmux's stderr: the cycle has
/// nowhere to report a per-pane failure and would blank every row if it aborted, so it treats any
/// failed pane command as one pane lost. The broker acts on a user's request and owes an answer.
pub(crate) fn is_dead_pane(err: &TmuxError) -> bool {
    matches!(err, TmuxError::Failed { .. })
}

/// Run one poll cycle against the tmux server (tier 1). Reads every pane once, consumes
/// fresh stamps, and produces (captures + guarded-stamps) stale agent panes. The ordered-input
/// clear runs inline; [`run_cycle_with`] is the variant that defers it.
pub fn run_cycle(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
) -> Result<CycleReport, TmuxError> {
    run_cycle_with(tmux, manifests, cfg, SeenClear::Inline)
}

/// [`run_cycle`] with an explicit ordered-input-clear policy. Anything that READS
/// `@agent_attention` to decide something passes [`SeenClear::Deferred`] and clears after that read
/// (the daemon, `wait`, `subscribe`); a surface that only renders the rows wants it inline.
pub fn run_cycle_with(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
    seen_clear: SeenClear,
) -> Result<CycleReport, TmuxError> {
    let start = Instant::now();
    let now = crate::now_ms();
    let panes = tmux.list_panes()?;

    // Stampede guard: another producer polled within this second ⇒ consume only. `now` and
    // `@tma_last_poll` are epoch ms, bucketed to seconds so the guard bounds the ~second-wide burst
    // of co-firing clients rather than demanding an exact-ms coincidence.
    let last_poll = tmux
        .get_server_option(opt::LAST_POLL)?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let stampede_skip = last_poll != 0 && now / 1000 == last_poll / 1000;

    let mut report = CycleReport {
        stampede_skipped: stampede_skip,
        ..Default::default()
    };
    let mut procs: Option<Vec<ProcInfo>> = None;
    let mut did_produce = false;
    // The stampede claim, chained onto the first stamp this cycle commits (see that call site).
    let poll_claim = render::set_server_option(opt::LAST_POLL, &now.to_string());
    // `-F` conditional-write support, probed lazily before the first producer write and cached for
    // the server's life; an unsupported server degrades to advisory plain writes (a clobber race).
    let mut guarded_supported: Option<bool> = None;
    // This cycle's per-pane result: `Some(state)` for a pane ending as an agent, `None` otherwise.
    // Every pane records exactly one entry; the end-of-cycle reconciliation reads it as the
    // authoritative window-summary basis.
    let mut resulting: HashMap<String, Option<AgentState>> = HashMap::new();

    for rec in &panes {
        // A corrupt stamp is deliberately treated as absent here: the pane is then re-detected and
        // restamped, which repairs it. `tma doctor` and `tma debug explain` are where the bad value
        // gets named; the hot path has nowhere to report it and no reason to stop for it.
        let read = StampedState::from_options(&rec.options).ok().flatten();
        let prev = read.clone().map(ReadResult::into_inner);

        // The user's escape hatch, checked before the consumer fast path so a fresh stamp cannot
        // keep an ignored pane on the surfaces. A stamp left from before the option was set is
        // removed (the same path a pane whose agent exited takes), so the pane drops out everywhere.
        if identity::is_ignored(&rec.options) {
            if prev.is_some() {
                match stamp::apply(tmux, &panes, &rec.pane_id, &StampPlan::Remove, true) {
                    Ok(()) => report.removed += 1,
                    Err(e) if is_dead_pane(&e) => report.skipped_dead += 1,
                    Err(e) => return Err(e),
                }
            }
            resulting.insert(rec.pane_id.clone(), None);
            continue;
        }

        // Consumer path: a fresh, settled stamp is trusted as-is. A `stamped_at` in the future (a
        // backward wall-clock step) fails the `<= now` guard, so the pane re-stamps against the
        // corrected clock instead of being trusted forever.
        let fresh = matches!(&read, Some(ReadResult::Settled(s))
            if s.stamped_at <= now && now - s.stamped_at < cfg.freshness_ms());
        if fresh || (stampede_skip && prev.is_some()) {
            if let Some(p) = &prev {
                report.consumed += 1;
                report.rows.push(row_from_stamp(rec, p, now));
                resulting.insert(rec.pane_id.clone(), Some(p.state));
            } else {
                resulting.insert(rec.pane_id.clone(), None);
            }
            continue;
        }

        // Producer path: needs the process tree (parsed once per cycle, lazily).
        if procs.is_none() {
            procs = Some(tma_tmux::tmux::ps_all()?);
        }
        let procs = procs.as_ref().unwrap();

        // Registered half: a stored `@agent_session` + `@agent_name` fed to identify, so a live
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

        // The stored flicker anchor feeds the title-narrowed sticky match.
        let stored_title_anchor = rec.options.get(opt::TITLE_MATCH_PID);
        let identity = identity::identify(
            rec.pane_pid,
            &rec.current_command,
            &rec.title,
            procs,
            manifests,
            stored_title_anchor.and_then(|v| v.parse().ok()),
            registration.as_ref(),
        );
        let PaneIdentity::Agent(id) = identity else {
            // Not an agent. A lingering stamp means its agent exited; remove it (plain unsets, so
            // the guard flag is irrelevant).
            if prev.is_some() {
                match stamp::apply(tmux, &panes, &rec.pane_id, &StampPlan::Remove, true) {
                    Ok(()) => report.removed += 1,
                    // Pane already gone: the removal it wanted is moot; treat as skipped.
                    Err(e) if is_dead_pane(&e) => report.skipped_dead += 1,
                    Err(e) => return Err(e),
                }
            }
            // Clear a stale title anchor so a lost title-narrowed match leaves nothing behind for a
            // future coincidence. No-op when the pane never had one.
            if let Some(cmd) = identity::title_anchor_command(
                &rec.pane_id,
                stored_title_anchor.map(String::as_str),
                None,
            ) {
                let _ = tmux.apply(&[cmd]);
            }
            resulting.insert(rec.pane_id.clone(), None);
            continue;
        };

        // Dead-registration reaper (`agent_pid == 0`): a hook-registered pane whose agent died
        // without a SessionEnd would otherwise hold its stamp forever. This is also where an agent
        // behind a container/inner-server boundary lands (`id.behind`), on the same terms: no
        // capture, stamps held, the reaper as the liveness bound. Distinguish a live pid-less
        // agent from a dead one by the subtree, not an age bound (which would deregister a live
        // gemini, whose `node` matches no `process_names`): a live agent leaves a non-shell process,
        // a dead one only the shell. Shell-only past `REG_DEAD_THRESHOLD_MS` (tracked in the durable
        // `@tma_reg_dead_since` marker) reaps; any non-shell process clears the marker.
        if id.agent_pid == 0 {
            let shell_only =
                identity::subtree_is_shell_only(&identity::subtree(rec.pane_pid, procs));
            let marker = rec
                .options
                .get(opt::REG_DEAD_SINCE)
                .and_then(|v| v.parse::<u64>().ok());
            match reg_dead_action(shell_only, marker, now) {
                RegDeadAction::Reap => {
                    // Confirmed dead: the `Remove` path also unsets `@tma_reg_dead_since` (in
                    // `REMOVABLE`). Plain unsets, so the guard flag is irrelevant.
                    match stamp::apply(tmux, &panes, &rec.pane_id, &StampPlan::Remove, true) {
                        Ok(()) => report.removed += 1,
                        Err(e) if is_dead_pane(&e) => report.skipped_dead += 1,
                        Err(e) => return Err(e),
                    }
                    resulting.insert(rec.pane_id.clone(), None);
                    continue;
                }
                // The marker rides the same apply path as the title anchor: a plain set/unset,
                // outside the guarded state tuple.
                RegDeadAction::SetMarker => {
                    let cmd = render::set_pane_option(
                        &rec.pane_id,
                        opt::REG_DEAD_SINCE,
                        &now.to_string(),
                    );
                    let _ = tmux.apply(&[cmd]);
                }
                RegDeadAction::ClearMarker => {
                    let cmd = render::unset_pane_option(&rec.pane_id, opt::REG_DEAD_SINCE);
                    let _ = tmux.apply(&[cmd]);
                }
                RegDeadAction::Hold => {}
            }
            // Hold the existing stamp rather than capture+fold: a fold with
            // `foreground_is_agent == false` would foreground-cap this hook state to `unknown`.
            if let Some(p) = &prev {
                report.consumed += 1;
                report.rows.push(row_from_stamp(rec, p, now));
                resulting.insert(rec.pane_id.clone(), Some(p.state));
            } else {
                resulting.insert(rec.pane_id.clone(), None);
            }
            continue;
        }

        // Quiet-pane skip: nothing has been written to this pane's window since its stamp, so the
        // screen the fold would read is byte-for-byte the one behind the stored verdict. Reuse it
        // and spend no `capture-pane` (~11 ms per agent pane, serial). Writes nothing: the stamp
        // stays as old as it was, so the next cycle re-evaluates this same gate against a later
        // `now` and captures the moment a time-driven transition comes due.
        if let Some(p) = prev
            .as_ref()
            .filter(|p| can_reuse_stamp(p, rec.window_activity, id.foreground_is_agent, cfg, now))
        {
            report.skipped_quiet += 1;
            report.consumed += 1;
            report.rows.push(row_from_stamp(rec, p, now));
            resulting.insert(rec.pane_id.clone(), Some(p.state));
            continue;
        }

        // Detect and fold this pane.
        let tail_text = match tmux.capture_pane(&rec.pane_id) {
            Ok(t) => t,
            Err(e) if is_dead_pane(&e) => {
                // Pane died between `list-panes` and its capture: skip it, treated as removed, so the
                // rollup shrinks like an externally-killed pane rather than the cycle aborting.
                resulting.insert(rec.pane_id.clone(), None);
                report.skipped_dead += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        report.captures += 1;
        let tail_hash = fnv1a64(tail_text.as_bytes());
        let snapshot = PaneSnapshot {
            pane_id: rec.pane_id.clone(),
            pid_tree: Vec::new(), // the fold reads facts, not the tree, in the poll path
            title: rec.title.clone(),
            tail_hash,
            tail_text,
            alternate_on: rec.alternate_on,
            scroll_position: rec.scroll_position,
            // Clamp `Region::Visible` rules to the visible screen; a zero height degrades to `None`
            // (the whole tail), never an empty region.
            visible_height: (rec.pane_height != 0).then_some(rec.pane_height),
            captured_at: now,
        };

        let evaluation = id.manifest.engine.evaluate(&snapshot);
        let evidence = &evaluation.evidence;
        let facts = SnapshotFacts {
            pid: id.agent_pid,
            foreground_is_agent: id.foreground_is_agent,
            scrolled: snapshot.scrolled(),
            history_view: evaluation.history_view,
        };
        let verdict = tma_core::verdict(
            prev.clone(),
            &facts,
            evidence,
            &id.manifest.manifest,
            cfg,
            now,
        );

        let plan = stamp::plan_from_verdict(
            &rec.pane_id,
            &verdict,
            id.agent_pid,
            &id.manifest.name,
            tail_hash,
            now,
        );
        // Resolve (and cache) `-F` support once, right before the first producer write.
        let guarded =
            *guarded_supported.get_or_insert_with(|| stamp::guarded_writes_supported(tmux, &panes));
        // The stampede claim rides the first stamp that lands, so a producing cycle costs ONE
        // `set-option` invocation per pane and not one more for the hint (every invocation forces
        // a full redraw of every attached client). Value and meaning are unchanged: this cycle's
        // `now`, claimed only once a stamp has actually committed.
        let trailing: &[render::StampCommand] = if did_produce {
            &[]
        } else {
            std::slice::from_ref(&poll_claim)
        };
        match stamp::apply_with(tmux, &panes, &rec.pane_id, &plan, guarded, trailing) {
            Ok(()) => {}
            Err(e) if is_dead_pane(&e) => {
                // Pane died between its capture and this stamp write: treat as removed, drop no
                // sibling's row. Don't push a row for a pane that no longer exists.
                resulting.insert(rec.pane_id.clone(), None);
                report.skipped_dead += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
        // Persist the flicker anchor for a title-narrowed match so identity holds across cursor's
        // title flicker next cycle. Only a title-narrowed claim carries `title_match_pid`.
        if id.title_match_pid.is_some() {
            if let Some(cmd) = identity::title_anchor_command(
                &rec.pane_id,
                stored_title_anchor.map(String::as_str),
                id.title_match_pid,
            ) {
                let _ = tmux.apply(&[cmd]);
            }
        }
        did_produce = true;
        resulting.insert(rec.pane_id.clone(), Some(verdict.state));
        if verdict.writes.action == WriteAction::Publish {
            report.produced += 1;
        } else {
            report.consumed += 1;
        }

        // The row shows what the write leaves stored, not the intended verdict: a source-guarded
        // write that loses (a hook claim the capture cannot clobber) commits nothing and keeps the
        // prior tuple. `render::project_publish` mirrors the guard + write-once `@agent_since` +
        // attention-hold so the row cannot drift from the write. Presentation only; a late concurrent
        // hook self-corrects next cycle.
        let (row_state, since, attention) = match &plan {
            StampPlan::Publish(publish) => {
                let p = render::project_publish(prev.as_ref(), publish);
                (p.state, p.since, p.attention)
            }
            StampPlan::Hold { .. } | StampPlan::Remove => (
                verdict.state,
                prev.as_ref()
                    .map(|p| p.since)
                    .unwrap_or(verdict.winning_evidence.at),
                prev.as_ref().is_some_and(|p| p.attention),
            ),
        };
        let turn_at = row_turn_at(&plan, prev.as_ref());
        let companions = row_companions(rec, now);
        report.rows.push(AgentRow {
            pane_id: rec.pane_id.clone(),
            agent: id.manifest.name.clone(),
            state: row_state,
            detail: verdict.detail.as_ref().map(|d| d.as_str().to_string()),
            since,
            turn_at,
            session: rec.session.clone(),
            window_index: rec.window_index,
            pane_index: rec.pane_index,
            title: rec.title.clone(),
            attention,
            agent_session: companions.agent_session,
            context_pct: companions.context_pct,
            context_at: companions.context_at,
            tokens: companions.tokens,
            quota: companions.quota,
            cost_usd: companions.cost_usd,
            muted: companions.muted,
            model: companions.model,
            cwd: rec.cwd.clone(),
            repo: None,
            pending: companions.pending,
        });
    }

    // End-of-cycle summary reconciliation: the authoritative, convergent writer for both the window
    // `@agent_summary` and its session-scoped mirror. Each scope's desired value is a pure function
    // of its full membership (this cycle's `resulting` verdicts, falling back to each pane's
    // cycle-start `@agent_state`), compared in memory against the stored value and written only on
    // drift, so a pure-consumer cycle stays cheap. This corrects an externally-killed pane and any
    // multi-agent scope the in-`apply()` per-pane append mis-rolled (that append stays a
    // best-effort hint for non-cycle callers). Residual: a concurrent-hook-suppressed change
    // reflects the intended verdict and self-corrects next cycle.
    let summary_cmds =
        stamp::reconcile_summary_commands(&panes, |p| match resulting.get(&p.pane_id) {
            Some(entry) => *entry,
            None => stamp::stored_pane_state(p),
        });
    if !summary_cmds.is_empty() {
        match tmux.apply(&summary_cmds) {
            Ok(()) => {}
            // One pane killed since this cycle's `list-panes` fails the whole chained reconcile.
            // Skip it on the same terms as the per-pane writes: the rollups are convergent, so the
            // next cycle rewrites them, and aborting here would blank every row over one dead pane.
            Err(e) if is_dead_pane(&e) => {}
            Err(e) => return Err(e),
        }
    }

    // Context pull path: tail each Codex `file-tail` pane's rollout and stamp the gauge. The
    // memo is process-local (a `static`), so a long-lived surface's steady state is one stat per quiet
    // pane per cycle; a one-shot process starts fresh (one read), the accepted cost. Non-Codex users
    // touch no filesystem — the per-pane channel check short-circuits before any discovery.
    if let Some(home) = crate::rollout::codex_home() {
        use std::sync::{LazyLock, Mutex};
        static CONTEXT_TAIL: LazyLock<Mutex<crate::rollout::RolloutTail>> =
            LazyLock::new(|| Mutex::new(crate::rollout::RolloutTail::new()));
        // Take-swap so the lock never spans the tmux spawn or the sessions/ fs walk (the repo.rs
        // `resolve` doctrine): a concurrent taker costs one duplicate cold read with last-write-wins
        // on put-back, harmless for cache-only data.
        let mut tail = std::mem::take(&mut *CONTEXT_TAIL.lock().unwrap_or_else(|e| e.into_inner()));
        crate::rollout::poll_context_tails(tmux, &panes, manifests, &mut tail, &home, now);
        *CONTEXT_TAIL.lock().unwrap_or_else(|e| e.into_inner()) = tail;
    }

    // Ordered-input clear: a done marker the user has demonstrably read since it was raised comes
    // down here, on the pane they are sitting on and never navigated away from. Skipped entirely on
    // a stampede-skipped cycle (the producer that did poll ran it) and on a fleet with nothing
    // flagged, so the zero-config floor pays no extra round trip in steady state.
    if !stampede_skip {
        let raised = crate::seen::raised_panes(&report.rows);
        if !raised.is_empty() {
            match seen_clear {
                SeenClear::Inline => {
                    crate::seen::clear_seen_rows(tmux, &mut report.rows, &raised);
                }
                SeenClear::Deferred => report.deferred_seen = raised,
            }
        }
    }

    // Deterministic order for surfaces: blocked → working → idle → unknown, then locator.
    report.rows.sort_by(|a, b| {
        sort_rank(a.state).cmp(&sort_rank(b.state)).then_with(|| {
            (&a.session, a.window_index, a.pane_index).cmp(&(
                &b.session,
                b.window_index,
                b.pane_index,
            ))
        })
    });
    report.elapsed = start.elapsed();
    Ok(report)
}

/// Build agent rows from the current stamps only — one `list-panes`, no capture, no fold.
/// The picker's instant first frame: stale-but-immediate, refined by the next cycle. An
/// `@agent_ignore` pane is dropped here too, so a stamp the next cycle is about to clear never
/// flashes on the first frame.
pub fn stamp_rows(tmux: &Tmux) -> Result<Vec<AgentRow>, TmuxError> {
    let panes = tmux.list_panes()?;
    let now = crate::now_ms();
    let mut rows: Vec<AgentRow> = panes
        .iter()
        .filter(|rec| !identity::is_ignored(&rec.options))
        .filter_map(|rec| {
            let stamp = StampedState::from_options(&rec.options)
                .ok()
                .flatten()
                .map(ReadResult::into_inner)?;
            Some(row_from_stamp(rec, &stamp, now))
        })
        .collect();
    rows.sort_by(|a, b| {
        sort_rank(a.state).cmp(&sort_rank(b.state)).then_with(|| {
            (&a.session, a.window_index, a.pane_index).cmp(&(
                &b.session,
                b.window_index,
                b.pane_index,
            ))
        })
    });
    Ok(rows)
}

fn row_from_stamp(rec: &PaneRecord, stamp: &StampedState, now: u64) -> AgentRow {
    let companions = row_companions(rec, now);
    AgentRow {
        pane_id: rec.pane_id.clone(),
        agent: rec
            .options
            .get(opt::NAME)
            .cloned()
            .unwrap_or_else(|| "?".to_string()),
        state: stamp.state,
        detail: stamp.detail.as_ref().map(|d| d.as_str().to_string()),
        since: stamp.since,
        turn_at: stamp.turn_at,
        session: rec.session.clone(),
        window_index: rec.window_index,
        pane_index: rec.pane_index,
        title: rec.title.clone(),
        attention: stamp.attention,
        // `@agent_session` is already decoded on the stamp tuple; the metrics ride the options.
        agent_session: stamp.session.clone(),
        context_pct: companions.context_pct,
        context_at: companions.context_at,
        tokens: companions.tokens,
        quota: companions.quota,
        cost_usd: companions.cost_usd,
        muted: companions.muted,
        model: companions.model,
        cwd: rec.cwd.clone(),
        repo: None,
        pending: companions.pending,
    }
}

/// The row fields a pane's stored options carry beside the state tuple: the owning `@agent_session`,
/// the `@agent_context_pct` gauge with its `@agent_context_at` evidence time and the `@agent_tokens`
/// count behind it, the account `@agent_quota_*` trio and `@agent_cost_usd`, and the `@agent_model`
/// label (watch-table-only). A struct, not a tuple: most of these are numeric options that would
/// swap silently at a call site.
struct RowCompanions {
    agent_session: Option<String>,
    context_pct: Option<u8>,
    context_at: Option<u64>,
    tokens: Option<u64>,
    quota: Option<QuotaLabel>,
    cost_usd: Option<f64>,
    muted: bool,
    model: Option<String>,
    pending: Option<tma_core::PendingCall>,
}

/// Read the companions from a pane's options. Absent/empty decodes as `None`; a non-numeric metric is
/// treated as absent (display-tolerant read). `now` resolves the mute deadline here, where the
/// cycle's clock already is, so no surface downstream has to read one.
fn row_companions(rec: &PaneRecord, now: u64) -> RowCompanions {
    let opt = |k: &str| rec.options.get(k).filter(|v| !v.is_empty());
    RowCompanions {
        agent_session: opt(opt::SESSION).cloned(),
        context_pct: opt(opt::CONTEXT_PCT).and_then(|v| v.parse().ok()),
        context_at: opt(opt::CONTEXT_AT).and_then(|v| v.parse().ok()),
        tokens: opt(opt::TOKENS).and_then(|v| v.parse().ok()),
        // The percent and the window token are stamped together; either missing is no annotation,
        // since a bare percent cannot say which window it measures.
        quota: opt(opt::QUOTA_PCT)
            .and_then(|v| v.parse().ok())
            .zip(opt(opt::QUOTA_WINDOW))
            .map(|(pct, window)| QuotaLabel {
                pct,
                window: window.clone(),
                resets_at_ms: opt(opt::QUOTA_RESETS_AT).and_then(|v| v.parse().ok()),
            }),
        cost_usd: opt(opt::COST_USD).and_then(|v| v.parse().ok()),
        muted: tma_core::stamp::mute_active(opt(opt::MUTE_UNTIL).and_then(|v| v.parse().ok()), now),
        model: opt(opt::MODEL).cloned(),
        // The trio is written and cleared as one, so the tool name alone decides whether a call is
        // pending; the other two are read beside it and default to empty.
        pending: opt(opt::PENDING_TOOL).map(|tool| tma_core::PendingCall {
            tool: tool.clone(),
            call: opt(opt::PENDING_CALL).cloned().unwrap_or_default(),
            summary: opt(opt::PENDING_SUMMARY).cloned().unwrap_or_default(),
        }),
    }
}

/// May this cycle reuse `prev` instead of capturing the pane? True only when nothing could have
/// changed the fold's answer: the screen behind the stamp is provably unchanged, and no window that
/// the mere passage of time closes is still open. Pure, so the whole rule is unit-tested off tmux.
///
/// `window_activity` is `#{window_activity}`, tmux's epoch-**seconds** timestamp of the last output
/// in the pane's window. Window-scoped, so it is conservative in the right direction: a quiet window
/// proves a quiet pane, never the reverse.
/// The `@agent_turn_at` the row carries, mirroring what the write beside it leaves stored.
///
/// The cycle never WRITES this key — only a `turn_end` hook does — so the row normally passes the
/// stored value through. The two zero arms are the cases where the write takes it away: `Remove`
/// drops the whole tuple, and an episode reset unsets it (`render::render_publish`). Without the
/// reset arm the row would carry the REPLACED agent's turn instant for one cycle, breaking the
/// invariant stated above `row_companions` that the row shows what the write leaves stored — and
/// under a backward clock step that stale value is what `episode_ms` and `wait --since` would read.
fn row_turn_at(plan: &StampPlan, prev: Option<&StampedState>) -> u64 {
    match plan {
        StampPlan::Remove => 0,
        StampPlan::Publish(publish) if publish.episode_reset => 0,
        _ => prev.map_or(0, |p| p.turn_at),
    }
}

fn can_reuse_stamp(
    p: &StampedState,
    window_activity: u64,
    foreground_is_agent: bool,
    cfg: &FoldConfig,
    now: u64,
) -> bool {
    // `freshness_secs = 0` asks for the screen to be re-read every cycle; honour that over the
    // shortcut, which would otherwise reuse a stamp no freshness window ever trusts.
    if cfg.freshness_ms() == 0 {
        return false;
    }
    // No stored hash means no cycle has ever read this pane's screen, so "unchanged since the
    // stamp" says nothing about what the rules would match.
    if p.hash.is_none() {
        return false;
    }
    // A stamp from the future (a backward wall-clock step) is not a baseline to trust.
    if p.stamped_at > now {
        return false;
    }
    // A working pane may still owe the dwell-delayed working→idle publish, and the idle chrome that
    // drives it is already on the unchanged screen — invisible without a capture. Always re-read.
    if p.state == AgentState::Working {
        return false;
    }
    // The foreground cap is the one verdict that turns on a process fact instead of the screen, and
    // that fact flips with no output at all — invisible to the activity gate below, which then pins
    // the stale verdict for as long as the pane stays quiet. `Provenance::Process` is the cap's own
    // `unknown` and nothing else writes it: with the agent back in the foreground that verdict is
    // void, and on a still screen nothing would ever free it (the pane holds `unknown` until
    // something happens to write to its window). The mirror case is a screen-read verdict that the
    // cap now owes an `unknown`. A hook claim is exempt from both — the cap holds it rather than
    // replacing it, so a re-read would reach the same stamp, and its decay rule below is what
    // expires it.
    match p.source {
        Provenance::Process if foreground_is_agent => return false,
        Provenance::Capture | Provenance::Activity if !foreground_is_agent => return false,
        _ => {}
    }
    // A hook claim past its decay window can be expired by contrary chrome that has been sitting on
    // the screen since before the stamp; only the capture can see it. Inside the window the claim
    // holds regardless, and the next cycle re-checks against a later `now`.
    if p.source == Provenance::Hook && now.saturating_sub(p.evidence_at) > cfg.decay_ms_for(p.state)
    {
        return false;
    }
    // tmux reports activity in whole seconds, so require the entire activity second to precede the
    // stamp: a write 300 ms after a stamp in the same second must never read as quiet. A zero
    // activity stamp (tmux reported it empty) is unusable, not evidence of quiet.
    window_activity != 0 && window_activity.saturating_add(1).saturating_mul(1000) <= p.stamped_at
}

/// Dead-registration reaper threshold: how long a hook-registered pane (`@agent_pid == 0`) must stay
/// shell-only before its registration clears. A live pid-less gemini is protected by the subtree
/// test, not this bound, so the threshold only covers a transient shell-only blip (a SessionStart
/// hook fired before the agent's process appears in `ps`). 30 s clears that race with margin yet
/// stays under a minute so `tma wait` on a crashed agent unblocks promptly.
const REG_DEAD_THRESHOLD_MS: u64 = 30_000;

/// The reaper's decision for a hook-registered, `agent_pid == 0` pane. Pure, so unit-tested without
/// tmux. `shell_only` is [`identity::subtree_is_shell_only`]; `marker` is the stored
/// `@tma_reg_dead_since` (epoch ms of the first shell-only observation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegDeadAction {
    /// Hold the existing stamp, write nothing (live pid-less agent, or still within the window).
    Hold,
    /// A non-shell process returned after the marker was set: clear it (flapping resets).
    ClearMarker,
    /// First shell-only observation (or a future/invalid marker): stamp `@tma_reg_dead_since`.
    SetMarker,
    /// Shell-only has persisted past the threshold: clear the registration (`Remove` path).
    Reap,
}

fn reg_dead_action(shell_only: bool, marker: Option<u64>, now: u64) -> RegDeadAction {
    if !shell_only {
        // A non-shell process is present ⇒ live (pid-less) agent: never reap. Clear any marker a
        // prior shell-only blip left, so a later crash starts a fresh window.
        return if marker.is_some() {
            RegDeadAction::ClearMarker
        } else {
            RegDeadAction::Hold
        };
    }
    match marker {
        // Within the window: keep holding. Past it: reap.
        Some(first) if first <= now => {
            if now - first >= REG_DEAD_THRESHOLD_MS {
                RegDeadAction::Reap
            } else {
                RegDeadAction::Hold
            }
        }
        // No marker yet, or a marker in the future (a backward wall-clock step): (re)start the
        // window from the corrected clock rather than trusting a future timestamp forever.
        _ => RegDeadAction::SetMarker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- dead-registration reaper decision (pure) ------------------------------------------

    const NOW: u64 = 1_000_000_000_000;

    #[test]
    fn reg_dead_live_unnamed_holds_forever() {
        // A live pid-less agent (gemini): subtree is NOT shell-only, no marker ⇒ hold, never reap.
        assert_eq!(reg_dead_action(false, None, NOW), RegDeadAction::Hold);
    }

    #[test]
    fn reg_dead_first_shell_only_sets_the_marker() {
        // Newly shell-only with no marker: stamp the durable first-seen marker, do not reap yet.
        assert_eq!(reg_dead_action(true, None, NOW), RegDeadAction::SetMarker);
    }

    #[test]
    fn reg_dead_shell_only_within_window_holds() {
        // Marker set 5 s ago (< 30 s threshold): keep holding, no reap.
        assert_eq!(
            reg_dead_action(true, Some(NOW - 5_000), NOW),
            RegDeadAction::Hold
        );
    }

    #[test]
    fn reg_dead_shell_only_past_threshold_reaps() {
        // Marker aged past the 30 s threshold: clear the registration.
        assert_eq!(
            reg_dead_action(true, Some(NOW - REG_DEAD_THRESHOLD_MS), NOW),
            RegDeadAction::Reap
        );
        assert_eq!(
            reg_dead_action(true, Some(NOW - REG_DEAD_THRESHOLD_MS - 1), NOW),
            RegDeadAction::Reap
        );
    }

    #[test]
    fn reg_dead_flapping_clears_the_marker() {
        // A non-shell process returned after the marker was set (agent restarted / momentary ps
        // race resolved): clear the marker so the reap window restarts on a later crash.
        assert_eq!(
            reg_dead_action(false, Some(NOW - 10_000), NOW),
            RegDeadAction::ClearMarker
        );
    }

    #[test]
    fn reg_dead_future_marker_resets_window() {
        // A marker in the future (backward wall-clock step) is not trusted: restart the window.
        assert_eq!(
            reg_dead_action(true, Some(NOW + 60_000), NOW),
            RegDeadAction::SetMarker
        );
    }

    // --- quiet-pane capture skip (pure) ---------------------------------------------------

    /// A settled prior with the transition-time fields collapsed onto one timestamp, plus a hash
    /// (the proof some earlier cycle actually read this pane's screen).
    fn quiet_prev(state: AgentState, source: Provenance, at: u64) -> StampedState {
        StampedState {
            state,
            detail: None,
            source,
            evidence_at: at,
            since: at,
            turn_at: 0,
            stamped_at: at,
            attention: false,
            notified_at: None,
            hash: Some(0xabc),
            pid: 4242,
            session: None,
            subagents: vec![],
        }
    }

    /// A prior carrying a recorded turn end, for the row-projection cases below.
    fn prior_with_turn(turn_at: u64) -> StampedState {
        StampedState {
            turn_at,
            ..quiet_prev(AgentState::Idle, Provenance::Hook, NOW)
        }
    }

    /// A publish plan built through the REAL `plan_from_verdict`, so these cases cannot drift from
    /// how the cycle actually constructs one (`Publish` is private to tma-tmux by design).
    fn publish_plan(episode_reset: bool) -> StampPlan {
        let verdict = tma_core::Verdict {
            state: AgentState::Idle,
            detail: None,
            winning_evidence: tma_core::WinningEvidence {
                source: Provenance::Hook,
                at: NOW,
                label: "test".to_string(),
            },
            writes: tma_core::WritePlan {
                action: tma_core::WriteAction::Publish,
                may_override: false,
                set_attention: false,
                episode_reset,
            },
        };
        stamp::plan_from_verdict("%0", &verdict, 4242, "claude", 0xabc, NOW)
    }

    /// The ordinary case: the cycle does not write `@agent_turn_at`, so the row passes the stored
    /// value straight through and `episode_ms` keeps reporting the recorded turn.
    #[test]
    fn the_row_carries_a_recorded_turn_end_through_an_ordinary_publish() {
        let prev = prior_with_turn(NOW - 5_000);
        assert_eq!(row_turn_at(&publish_plan(false), Some(&prev)), NOW - 5_000);
    }

    /// An episode reset (pid change) unsets `@agent_turn_at` in the write, so the row must drop it
    /// in the same cycle. Carrying the REPLACED agent's turn instant through would break the
    /// row-mirrors-the-write invariant, and under a backward clock step that stale value is what
    /// `episode_ms` and `wait --since` would compare against.
    #[test]
    fn an_episode_reset_drops_the_replaced_agents_turn_end_from_the_row() {
        let prev = prior_with_turn(NOW - 5_000);
        assert_eq!(row_turn_at(&publish_plan(true), Some(&prev)), 0);
    }

    /// Removing the tuple takes the whole episode lane with it.
    #[test]
    fn removing_the_tuple_drops_the_turn_end_from_the_row() {
        let prev = prior_with_turn(NOW - 5_000);
        assert_eq!(row_turn_at(&StampPlan::Remove, Some(&prev)), 0);
    }

    /// `#{window_activity}` (epoch seconds) for a window last written to `secs` before the stamp.
    fn activity_before(stamped_at: u64, secs: u64) -> u64 {
        stamped_at / 1000 - secs
    }

    #[test]
    fn a_quiet_idle_pane_reuses_its_stamp() {
        let cfg = FoldConfig::default();
        let prev = quiet_prev(AgentState::Idle, Provenance::Capture, NOW);
        assert!(can_reuse_stamp(
            &prev,
            activity_before(NOW, 5),
            true,
            &cfg,
            NOW + 10_000
        ));
    }

    #[test]
    fn activity_since_the_stamp_forces_a_capture() {
        let cfg = FoldConfig::default();
        let prev = quiet_prev(AgentState::Idle, Provenance::Capture, NOW);
        // Output one second AFTER the stamp: the screen may have changed.
        assert!(!can_reuse_stamp(
            &prev,
            NOW / 1000 + 1,
            true,
            &cfg,
            NOW + 10_000
        ));
        // Output in the SAME second as the stamp is ambiguous at second resolution, so it captures.
        assert!(!can_reuse_stamp(
            &prev,
            NOW / 1000,
            true,
            &cfg,
            NOW + 10_000
        ));
        // An unreported (zero) activity timestamp is not evidence of quiet.
        assert!(!can_reuse_stamp(&prev, 0, true, &cfg, NOW + 10_000));
    }

    #[test]
    fn a_hook_claim_past_its_decay_window_forces_a_capture() {
        let cfg = FoldConfig::default();
        let quiet = activity_before(NOW, 5);
        // A blocked hook claim inside its (300 s) window holds whatever the screen says: reuse.
        let blocked = quiet_prev(AgentState::Blocked, Provenance::Hook, NOW);
        assert!(can_reuse_stamp(
            &blocked,
            quiet,
            true,
            &cfg,
            NOW + cfg.blocked_decay_ms()
        ));
        // Past it, contrary chrome that has been on the screen all along may now expire the claim,
        // and only a capture can see it.
        assert!(!can_reuse_stamp(
            &blocked,
            quiet,
            true,
            &cfg,
            NOW + cfg.blocked_decay_ms() + 1
        ));
        // The shorter window applies to a non-blocked hook claim.
        let idle = quiet_prev(AgentState::Idle, Provenance::Hook, NOW);
        assert!(can_reuse_stamp(
            &idle,
            quiet,
            true,
            &cfg,
            NOW + cfg.hook_decay_ms()
        ));
        assert!(!can_reuse_stamp(
            &idle,
            quiet,
            true,
            &cfg,
            NOW + cfg.hook_decay_ms() + 1
        ));
        // A capture-sourced claim never decays, so age alone does not force a capture.
        let capture = quiet_prev(AgentState::Blocked, Provenance::Capture, NOW);
        assert!(can_reuse_stamp(
            &capture,
            quiet,
            true,
            &cfg,
            NOW + 10 * cfg.blocked_decay_ms()
        ));
    }

    #[test]
    fn a_working_pane_always_recaptures() {
        // The dwell-delayed working→idle publish is driven by idle chrome already on the unchanged
        // screen, so skipping would pin the pane working forever.
        let cfg = FoldConfig::default();
        let prev = quiet_prev(AgentState::Working, Provenance::Capture, NOW);
        assert!(!can_reuse_stamp(
            &prev,
            activity_before(NOW, 60),
            true,
            &cfg,
            NOW + 60_000
        ));
    }

    #[test]
    fn a_lifted_foreground_cap_forces_a_capture() {
        // The cap stamped `unknown` while the agent was not the foreground; the pane then went
        // quiet. Nothing will ever write to its window again, so only re-reading the process fact
        // can free it — without this the pane holds `unknown` for as long as it stays settled.
        let cfg = FoldConfig::default();
        let quiet = activity_before(NOW, 5);
        let capped = quiet_prev(AgentState::Unknown, Provenance::Process, NOW);
        assert!(!can_reuse_stamp(&capped, quiet, true, &cfg, NOW + 10_000));
        // Still capped: the fold would reach the same verdict, so the skip still pays off.
        assert!(can_reuse_stamp(&capped, quiet, false, &cfg, NOW + 10_000));
    }

    #[test]
    fn a_foreground_that_left_the_agent_forces_a_capture() {
        // The mirror image: a screen-sourced verdict stands from a fold that ran with the agent in
        // the foreground. The foreground has since moved on, so the cap is now owed and the stored
        // state is stale even though the screen never changed.
        let cfg = FoldConfig::default();
        let quiet = activity_before(NOW, 5);
        let prev = quiet_prev(AgentState::Idle, Provenance::Capture, NOW);
        assert!(!can_reuse_stamp(&prev, quiet, false, &cfg, NOW + 10_000));
        // A hook claim is exempt: the cap holds it rather than replacing it, so the re-read would
        // land on the same stamp. Wrapper panes (an agent under a launcher) sit here permanently,
        // and capturing them every cycle is the cost the skip exists to avoid.
        let hook = quiet_prev(AgentState::Blocked, Provenance::Hook, NOW);
        assert!(can_reuse_stamp(&hook, quiet, false, &cfg, NOW + 10_000));
    }

    #[test]
    fn an_unread_screen_or_a_future_stamp_never_reuses() {
        let cfg = FoldConfig::default();
        let quiet = activity_before(NOW, 5);
        // Hook-stamped but never captured: no baseline hash, so "unchanged" proves nothing.
        let mut no_hash = quiet_prev(AgentState::Idle, Provenance::Hook, NOW);
        no_hash.hash = None;
        assert!(!can_reuse_stamp(&no_hash, quiet, true, &cfg, NOW + 1_000));
        // A stamp in the future (backward wall-clock step) is not a baseline to trust.
        let prev = quiet_prev(AgentState::Idle, Provenance::Capture, NOW);
        assert!(!can_reuse_stamp(&prev, quiet, true, &cfg, NOW - 1_000));
    }

    #[test]
    fn sort_rank_orders_blocked_first() {
        assert!(sort_rank(AgentState::Blocked) < sort_rank(AgentState::Working));
        assert!(sort_rank(AgentState::Working) < sort_rank(AgentState::Idle));
        assert!(sort_rank(AgentState::Idle) < sort_rank(AgentState::Unknown));
    }

    /// Both per-pane error branches (`run_cycle`, `capture::handle_edges`) route their skip-or-abort
    /// through `is_dead_pane`: a `Failed` pane is skipped, only `ServerGone`/parse aborts. A
    /// deterministic mid-cycle kill needs a seam `run_cycle` does not expose, so the branch is
    /// covered here at the shared classifier.
    #[test]
    fn is_dead_pane_skips_failed_not_server_gone() {
        assert!(
            is_dead_pane(&TmuxError::Failed {
                cmd: "tmux capture-pane -p -t %7".to_string(),
                code: 1,
                stderr: "can't find pane %7".to_string(),
            }),
            "a per-pane Failed is a dead pane: skip it"
        );
        assert!(
            !is_dead_pane(&TmuxError::ServerGone),
            "ServerGone is whole-server: propagate, never skip"
        );
        assert!(
            !is_dead_pane(&TmuxError::Parse {
                cmd: "list-panes".to_string(),
                reason: "unexpected output".to_string(),
            }),
            "a parse failure is not a per-pane death: propagate"
        );
    }
}
