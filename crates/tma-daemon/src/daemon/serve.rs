//! The blocking poll loop and its dispatch: serve membership + captures + notifications, classify
//! each connection (event apply or `tma wait` subscribe), and drive the SIGHUP reload. No async runtime.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

use tma_runtime::capture::CaptureState;
use tma_runtime::config::{self, Config};
use tma_runtime::event::EventOutcome;
use tma_runtime::ipc::{self, Inbound, ACK, NAK};
use tma_runtime::manifests::LoadedManifest;
use tma_tmux::control::{self, ControlPool, ProbeOutcome};
use tma_tmux::tmux::{Tmux, TmuxError};

use crate::notify::NotifyState;

use super::pending::{self, Advance, Pending};
use super::subscribers::{push_subscribers, reap_closed_subscribers, register_subscriber};
use super::sys::drain_signal;
use super::{log_manifest_failures, SignalAction};

/// A `poll(2)` timeout as rustix 1.x wants it: `Some(&Timespec)` (was a `c_int` ms in 0.38).
fn poll_timeout(d: Duration) -> Timespec {
    Timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
        tv_nsec: d.subsec_nanos() as _,
    }
}

/// The event loop: poll the listener, the signal self-pipe, and each control client's stdout,
/// blocking until one is ready or the dynamic timeout (nearest quiet-edge deadline, else the sweep)
/// elapses. Returns on shutdown or a gone server. The [`ControlPool`] is created and dropped here,
/// so every `tmux -C` child is reaped on exit. `config`/`manifests` are owned so a SIGHUP reload can
/// swap them in place; `config_path`/`manifest_dir` are the paths the reload re-reads.
#[allow(clippy::too_many_arguments)]
pub(super) fn serve(
    tmux: &Tmux,
    listener: &UnixListener,
    mut manifests: Vec<LoadedManifest>,
    sig_read: OwnedFd,
    status_file: Option<&Path>,
    probe_cross_session: bool,
    sweep_ms: Option<u64>,
    mut config: Config,
    config_path: Option<&Path>,
    manifest_dir: Option<&Path>,
) {
    // Daemon knobs (quiet threshold, fold tuning + demotion, sweep cadence, zero-member recheck)
    // all come from `[daemon]`/`[fold]` config, defaulting to the shipped consts.
    let mut pool = ControlPool::new(config.daemon.quiet_threshold());
    // On-demand capture + reconciliation sweep + demotion state. Daemon-only.
    let mut capture = CaptureState::new(config.fold_config(), config.daemon.demote_edges);
    // Notification dispatch + transition history: the one place blocked notifications fire, fed by
    // the persisted marker. `TMA_NOTIFY_CMD` overrides the canonical `notify.command` (test/CI seam).
    let mut notify = NotifyState::new(
        config.notify.commands(),
        config.notify.on.clone(),
        config.notify.sinks(),
        config.notify.context_high.as_ref().map(|c| c.threshold),
    );
    // Mutable: a SIGHUP reload re-derives this from the reloaded `[daemon]` config.
    let mut empty_pool_recheck = config.daemon.zero_member_recheck();

    // `tma wait` push subscribers: retained connections fed a one-byte PUSH wake on state-affecting
    // work. Bounded by `MAX_SUBSCRIBERS`; a slow/dead one is dropped (never-wait), never stalling the loop.
    let mut subscribers: Vec<UnixStream> = Vec::new();

    // Connections parked mid-frame: a client whose whole frame was not yet buffered on accept joins
    // the poll set with its own read buffer + `FRAME_DEADLINE` drop deadline, so one slow client never
    // serializes the accept path (which would starve control reads / quiet edges). Bounded, kill-oldest.
    let mut pending: Vec<Pending> = Vec::new();

    // Behavior probe (once, before any pool client exists so its throwaway session is invisible to
    // the pool): does session-scoped push deliver? On failure, degrade to the faster sweep.
    let probe = control::probe_push(tmux, probe_cross_session);
    // The normal cadence is configurable (`[daemon] sweep_secs`); the degraded value stays the
    // shipped `SWEEP_DEGRADED` const (only the normal cadence is a knob).
    let sweep_normal = config.daemon.sweep();
    match probe {
        ProbeOutcome::Available => eprintln!(
            "tma: control-mode activity push available (tmux 3.6a verified); \
             reconciliation sweep every {}s (repair only).",
            sweep_normal.as_secs()
        ),
        ProbeOutcome::AssumedAvailable => eprintln!(
            "tma: control-mode push probe errored; assuming available (never observed delivery); \
             reconciliation sweep every {}s (repair only). A real push failure the probe cannot \
             see is bounded by the sweep, not the quiet edge.",
            sweep_normal.as_secs()
        ),
        ProbeOutcome::Unavailable => eprintln!(
            "tma: control-mode activity push UNAVAILABLE (session-scoped delivery silent); \
             degrading — reconciliation sweep every {}s (was {}s). Hookless `blocked` \
             latency is now bounded by the sweep (~{}s), not the near-instant quiet edge.",
            control::SWEEP_DEGRADED.as_secs(),
            sweep_normal.as_secs(),
            control::SWEEP_DEGRADED.as_secs(),
        ),
    }
    // Effective cadence (probe verdict + config cadence, `--sweep-ms` override winning). Mutable: a
    // SIGHUP reload re-derives it, keeping the one-time probe verdict fixed.
    let mut sweep = resolve_sweep(probe, &config, sweep_ms);

    // The server's startup identity (`#{pid}`). Since socket/lock key on the REUSED `#{socket_path}`,
    // without this a same-path restart would let the reconcile adopt the new instance while carrying
    // stale state (the demotion + notify maps). `server_restarted` re-checks this pid so a restart
    // exits deterministically. `None` (gone/unreadable at startup) leaves the `ServerGone` path to end it.
    let server_id = tmux.display_active("#{pid}").ok().filter(|s| !s.is_empty());

    // Initial membership: one control client per existing session. A startup ServerGone is left
    // for the loop's reconcile to catch.
    let _ = pool.reconcile(tmux);
    // Cold-start notification pass: a pane already `blocked` with `notified_at >= since` does NOT
    // re-fire (safe restarts). Seeds the transition ring with panes present at boot.
    let _ = notify.reconcile(tmux);
    if let Some(p) = status_file {
        pool.write_status(
            p,
            probe,
            sweep,
            &status_extra(&capture, &notify, subscribers.len()),
        );
    }

    // The reconciliation sweep runs every `sweep` (events drive state; the sweep repairs it).
    // The first sweep is one interval out, so a fresh daemon does not fan-out captures on boot.
    let mut last_sweep = Instant::now();

    loop {
        // Emit due quiet edges and compute the poll timeout. Surface a new edge count immediately
        // (before the possibly-long sweep poll) so a freshly-quiet pane's edge is promptly observable.
        let edges_before = pool.edges_emitted();
        // While clientless, recheck server liveness within ~1 s (see `ControlPool::is_empty`); with
        // clients a gone server wakes us via `%exit`/EOF, so the long sweep cadence stands.
        let iter_sweep = effective_sweep(pool.is_empty(), sweep, empty_pool_recheck);
        // Time enters the pool tick at this boundary: the monotone deadline clock and the wall epoch
        // stamped onto any edge emitted this wake.
        let timeout = control::tick(&mut pool, Instant::now(), tma_runtime::now_ms(), iter_sweep);

        // On-demand capture: drain this iteration's active→quiet edges and capture the ONE pane each
        // fired for (never a fan-out); where hookless `blocked` is caught. Server-gone ends the loop.
        //
        // DEMOTION ORDERING INVARIANT (why demoted unguarded writes are safe): edges are drained and
        // folded HERE, strictly BEFORE inbound hook frames are accepted below (`on_hook_event` resets
        // a pane's edge counter and clears its demotion). So a hook frame arriving later this
        // iteration re-guards the pane before the NEXT drain folds its edge, and an unguarded capture
        // never races a live hook event. Reordering these two blocks would break that. The FOLD half
        // lives in `drain_and_fold_edges`; the APPLY half is every `dispatch_inbound` call below.
        if drain_and_fold_edges(
            &mut pool,
            &mut capture,
            &mut notify,
            &mut subscribers,
            tmux,
            &manifests,
            status_file,
            probe,
            sweep,
            edges_before,
        ) {
            break;
        }

        // Build the poll set: listener, signal pipe, one fd per control client, one fd per `tma wait`
        // subscriber (so a waiter's hangup wakes the loop for prompt reaping), then one fd per parked
        // connection (so its next bytes wake us to finish or drop it).
        let client_fds = control::pollfds(&pool);
        let mut fds: Vec<PollFd> =
            Vec::with_capacity(2 + client_fds.len() + subscribers.len() + pending.len());
        fds.push(PollFd::new(listener, PollFlags::IN));
        fds.push(PollFd::new(&sig_read, PollFlags::IN));
        for (_sid, fd) in &client_fds {
            fds.push(PollFd::from_borrowed_fd(*fd, PollFlags::IN));
        }
        let sub_base = fds.len();
        for s in &subscribers {
            fds.push(PollFd::new(s, PollFlags::IN));
        }
        let pend_base = fds.len();
        for p in &pending {
            fds.push(PollFd::new(p, PollFlags::IN));
        }
        // Session ids in fd order. Consuming `client_fds` ends the immutable pool borrow its
        // `BorrowedFd`s hold, so the reload/dispatch below can take `&mut pool`; `fds` keeps its own
        // fd borrow (a copy) until dropped just after `poll`.
        let client_sids: Vec<String> = client_fds.into_iter().map(|(sid, _)| sid).collect();

        // Shrink the poll timeout to the nearest parked-connection deadline when one is due sooner
        // than the tick timeout, so a stalled peer sending no further bytes is still dropped on time.
        let poll_to = match pending::nearest_deadline(&pending, Instant::now()) {
            Some(d) => timeout.min(d),
            None => timeout,
        };
        let ts = poll_timeout(poll_to);
        let rc = match poll(&mut fds, Some(&ts)) {
            Ok(n) => n,
            Err(Errno::INTR) => continue,
            Err(_) => break,
        };
        // Snapshot the revents, then drop the borrowing `PollFd`s so the loop body may mutate
        // pool/subscribers/listener freely below.
        let revents: Vec<PollFlags> = fds.iter().map(|f| f.revents()).collect();
        drop(fds);

        // Reap hung-up subscribers BEFORE the accept block can push NEW ones, so `revents[sub_base + i]`
        // still lines up with `subscribers`. A reap dirties the status so the gauge updates promptly.
        let reaped = if subscribers.is_empty() {
            0
        } else {
            reap_closed_subscribers(&mut subscribers, &revents, sub_base)
        };

        // A signal woke the poll. Drain the self-pipe and act: SIGTERM/SIGINT end the loop;
        // SIGHUP hot-reloads config + manifests and swaps the derived state in place.
        if revents[1].contains(PollFlags::IN) {
            match drain_signal(&sig_read) {
                SignalAction::Shutdown => break,
                SignalAction::None => {}
                SignalAction::Reload => {
                    if reload_state(&mut config, &mut manifests, config_path, manifest_dir) {
                        // Re-derive every config-dependent knob from the new config. Each holder
                        // keeps its runtime state (control clients, demotion map, notify history).
                        pool.set_quiet_threshold(config.daemon.quiet_threshold());
                        capture.set_config(config.fold_config(), config.daemon.demote_edges);
                        notify.reconfigure(
                            config.notify.commands(),
                            config.notify.on.clone(),
                            config.notify.sinks(),
                            config.notify.context_high.as_ref().map(|c| c.threshold),
                        );
                        empty_pool_recheck = config.daemon.zero_member_recheck();
                        sweep = resolve_sweep(probe, &config, sweep_ms);
                        eprintln!("tma: reloaded config + manifests (SIGHUP)");
                        // Re-evaluate notifications now: a reload is a cold-start for `notify.on`, so
                        // a completion newly in scope fires once (the marker still dedups otherwise).
                        if dispatch_notify(&mut notify, tmux) {
                            break;
                        }
                        if let Some(p) = status_file {
                            pool.write_status(
                                p,
                                probe,
                                sweep,
                                &status_extra(&capture, &notify, subscribers.len()),
                            );
                        }
                    }
                }
            }
        }

        let mut status_dirty = reaped > 0;

        // Parked connections: advance any that became readable or whose deadline passed. A frame that
        // only now completes dispatches through the SAME apply/subscribe/ACK path as an inline accept,
        // and strictly AFTER this iteration's edge drain (top of loop), so the DEMOTION ORDERING
        // INVARIANT holds even for a frame parked across earlier iterations. This runs BEFORE the
        // accept block mutates `pending`, so `revents[pend_base + i]` still lines up with the snapshot.
        if !pending.is_empty() {
            let now = Instant::now();
            let readable = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
            let mut kept: Vec<Pending> = Vec::with_capacity(pending.len());
            for (i, mut conn) in std::mem::take(&mut pending).into_iter().enumerate() {
                let due = conn.is_due(now);
                if !revents[pend_base + i].intersects(readable) && !due {
                    kept.push(conn); // neither readable nor expired: still parked
                    continue;
                }
                match conn.advance() {
                    Advance::Complete(inbound) => {
                        dispatch_inbound(
                            tmux,
                            &manifests,
                            &mut capture,
                            &mut subscribers,
                            &notify,
                            conn.into_stream(),
                            inbound,
                        );
                        status_dirty = true;
                    }
                    Advance::Park if !due => kept.push(conn),
                    _ => conn.nak_and_drop(), // Drop, or still-partial past its deadline
                }
            }
            pending = kept;
        }

        // Inbound socket connection(s): drain the accept queue (non-blocking accept). A client's whole
        // frame is usually already in the socket buffer, so the read+parse completes inline here and
        // the connection never parks; only a would-block/partial frame joins `pending` (bounded,
        // kill-oldest). A completed hook event can clear a pane's demotion, so flush the status.
        if revents[0].contains(PollFlags::IN) {
            let now = Instant::now();
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => match Pending::accept(stream, now) {
                        Some((conn, Advance::Complete(inbound))) => {
                            dispatch_inbound(
                                tmux,
                                &manifests,
                                &mut capture,
                                &mut subscribers,
                                &notify,
                                conn.into_stream(),
                                inbound,
                            );
                            status_dirty = true;
                        }
                        Some((conn, Advance::Park)) => pending::admit(&mut pending, conn),
                        Some((conn, Advance::Drop)) => conn.nak_and_drop(),
                        None => {} // could not set the accepted stream non-blocking: dropped
                    },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Control-mode clients: which fds became readable (POLLIN) or hung up (POLLHUP/ERR)?
        let closed = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
        let mut ready: HashSet<String> = HashSet::new();
        for (i, sid) in client_sids.iter().enumerate() {
            if revents[2 + i].intersects(closed) {
                ready.insert(sid.clone());
            }
        }

        let fx = control::dispatch_ready(&mut pool, &ready, Instant::now());
        if !ready.is_empty() {
            status_dirty = true;
        }

        // A pane/window lifecycle event ⇒ recompute the window and session summaries so a closed
        // agent pane's rollups clear promptly, even with no `SessionEnd`. Server-gone ends the loop.
        if fx.need_summary {
            if let Err(TmuxError::ServerGone) = control::reconcile_summaries(tmux) {
                break;
            }
        }

        // Reconcile pool membership on `%sessions-changed`, a dropped client, or the sweep tick
        // (rc == 0). Also the zero-member recovery + server-gone check (`ServerGone` ends the loop).
        if fx.need_reconcile || rc == 0 {
            // Before adopting `list-sessions`: a same-`#{socket_path}` restart would make it succeed
            // against the NEW server, so a changed `#{pid}` exits like server-gone, never adopting stale state.
            if server_restarted(tmux, server_id.as_deref()) {
                break;
            }
            match pool.reconcile(tmux) {
                Ok(()) => status_dirty = true,
                Err(TmuxError::ServerGone) => break,
                Err(_) => {} // transient read error: retry next tick
            }
        }

        // Reconciliation sweep: the full phase-1 cycle every `sweep`, the ONLY multi-`capture-pane`
        // fan-out. Self-healing: discovers unannounced agents, clears dead processes, fixes hook drift.
        if sweep_due(last_sweep, Instant::now(), sweep) {
            match capture.run_sweep(tmux, &manifests) {
                Ok(()) => status_dirty = true,
                Err(TmuxError::ServerGone) => break,
                Err(_) => {} // transient read/capture error: retry next cadence
            }
            last_sweep = Instant::now();
        }

        // Notification dispatch: the ONE place blocked notifications fire, keyed on the persisted
        // marker so it fires once per episode whatever producer drove `blocked` (redundant passes are no-ops).
        if status_dirty && dispatch_notify(&mut notify, tmux) {
            break;
        }

        // The sweep's deferred ordered-input clear, STRICTLY after that dispatch: both read the same
        // persisted `@agent_attention`, and the dispatch is what turns a completion into a desktop
        // notification, so a clear landing first would swallow it. Nothing here dirties status — the
        // sweep that produced these candidates already did, and the clear is a retraction the next
        // cycle reports anyway.
        let seen = capture.take_deferred_seen();
        if !seen.is_empty() {
            tma_runtime::seen::clear_seen(tmux, &seen);
        }

        // Wake subscribers on any state-affecting work (`status_dirty`). The push is only a WAKE hint;
        // `tma wait` re-runs its authoritative cycle, so the coarse gate is correct (spurious costs one cycle).
        //
        // Ordered before the status write, not after: a push to a waiter that has already exited fails
        // and drops it, so writing the gauge first records a subscriber that is gone by the end of this
        // iteration — and nothing dirties the status again until the 45 s sweep, since the hangup this
        // path consumed is exactly what `reap_closed_subscribers` would have counted.
        if status_dirty && !subscribers.is_empty() {
            // Folded back in so the gauge is right even if this is ever reordered after the write.
            status_dirty |= push_subscribers(&mut subscribers) > 0;
        }

        if status_dirty {
            if let Some(p) = status_file {
                pool.write_status(
                    p,
                    probe,
                    sweep,
                    &status_extra(&capture, &notify, subscribers.len()),
                );
            }
        }
    }
    // `pool` drops here → every control client is killed + waited (no leaked `tmux -C`). `subscribers`
    // drops too → each waiter socket closes, so a blocked `tma wait` reads EOF and degrades to polling.
}

/// Combined status-file body: the capture + notify introspection blocks plus the `wait_subscribers`
/// gauge, which lets integration tests gate on push mode being active.
fn status_extra(capture: &CaptureState, notify: &NotifyState, n_subscribers: usize) -> String {
    format!(
        "{}{}wait_subscribers={}\n",
        capture.status_lines(),
        notify.status_lines(),
        n_subscribers
    )
}

/// Run the notification dispatch pass; returns `true` if the server is gone (the caller ends the
/// loop). A transient read error is swallowed: the next pass retries from the persisted marker.
fn dispatch_notify(notify: &mut NotifyState, tmux: &Tmux) -> bool {
    matches!(notify.reconcile(tmux), Err(TmuxError::ServerGone))
}

/// The FOLD half of the DEMOTION ORDERING INVARIANT: drain this iteration's active→quiet edges and
/// fold them (the on-demand capture tier, at most one capture per edge), then fire any `blocked` the
/// capture produced and refresh status/subscribers. All of this runs BEFORE the poll block so a
/// hookless pane's `blocked` latency tracks the quiet edge, not the next fallback cycle, and strictly
/// before any inbound frame is applied (`dispatch_inbound`). Returns `true` when the server is gone
/// (the caller ends the loop). `edges_before` is the pre-`tick` emit count, so a tick-emitted edge
/// that produced no drained edge still flushes status.
#[allow(clippy::too_many_arguments)]
fn drain_and_fold_edges(
    pool: &mut ControlPool,
    capture: &mut CaptureState,
    notify: &mut NotifyState,
    subscribers: &mut Vec<UnixStream>,
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    status_file: Option<&Path>,
    probe: ProbeOutcome,
    sweep: Duration,
    edges_before: u64,
) -> bool {
    let edges = pool.drain_edges();
    let had_edges = !edges.is_empty();
    if had_edges {
        match capture.handle_edges(tmux, manifests, edges) {
            Ok(()) => {}
            Err(TmuxError::ServerGone) => return true,
            Err(_) => {} // transient read/capture error: the sweep repairs it
        }
        // Fire any `blocked` the capture just produced BEFORE the possibly-long poll block, so
        // hookless blocked→notification latency stays at the quiet-edge cadence (<5 s).
        if dispatch_notify(notify, tmux) {
            return true;
        }
    }
    // A hookless quiet edge may have changed state; wake subscribers BEFORE the poll block so a
    // hookless waiter's blocked latency tracks the quiet edge, not the next fallback cycle. The push
    // comes before the status write because it can DROP a subscriber whose peer has already exited,
    // and the `wait_subscribers` gauge must report the set that survived it.
    let dropped = if had_edges && !subscribers.is_empty() {
        push_subscribers(subscribers)
    } else {
        0
    };
    if pool.edges_emitted() != edges_before || had_edges || dropped > 0 {
        if let Some(p) = status_file {
            pool.write_status(
                p,
                probe,
                sweep,
                &status_extra(capture, notify, subscribers.len()),
            );
        }
    }
    false
}

/// Dispatch a fully-parsed inbound frame, consuming its `stream`: an event is applied and acked (NAK
/// on anything this daemon did not resolve, so the client re-applies), a subscribe upgrades the connection into a
/// retained push subscriber. The frame-application step, split from the read so the poll loop drives
/// it identically whether the frame completed inline on accept or only after parking. Every call sits
/// AFTER this iteration's edge drain (top of loop), which is where the DEMOTION ORDERING INVARIANT
/// lands (see the drain block).
#[allow(clippy::too_many_arguments)]
fn dispatch_inbound(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    capture: &mut CaptureState,
    subscribers: &mut Vec<UnixStream>,
    notify: &NotifyState,
    mut stream: UnixStream,
    inbound: Inbound,
) {
    match inbound {
        Inbound::Event(ev) => {
            // One-byte delivery ack (best-effort): on NAK/timeout/EOF the client falls through to a
            // direct stamp, so an unprocessed frame is never silently eaten.
            let accepted = apply_hook_event(tmux, manifests, capture, &ev);
            let _ = stream.write_all(&[if accepted { ACK } else { NAK }]);
        }
        Inbound::Subscribe => register_subscriber(subscribers, stream),
        // A read-only answer off the in-memory ring: no tmux, so it cannot stall the loop.
        Inbound::History => ipc::write_history(&mut stream, &notify.history_document()),
    }
}

/// Apply one hook-event frame through the shared adapter; `true` iff this daemon actually resolved
/// it. `false` on an agent it has no manifest for, and on an event its manifests map to nothing, so
/// the caller NAKs and the client re-applies with its own manifests. That second case is the
/// upgrade-skew hole: a resident daemon carrying older compiled-in manifests would otherwise ack a
/// transition it wrote nothing for, and the client would skip its own direct stamp. A deliberate
/// no-write verdict (the subagent ownership guard refusing a foreign session) is `true` — re-applying
/// it on the client would double-write exactly what the daemon correctly refused.
fn apply_hook_event(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    capture: &mut CaptureState,
    ev: &ipc::Frame,
) -> bool {
    // The hook fired for this pane: its wiring is alive, so clear any demotion before the manifest
    // lookup (even an event for an unknown agent proves the pane's hooks run).
    capture.on_hook_event(&ev.pane);
    let Some(lm) = manifests.iter().find(|m| m.name == ev.agent) else {
        return false; // agent unknown to THIS daemon: NAK so the client direct-stamps
    };
    let now = tma_runtime::now_ms();
    // Applies the stamp only (same guarded write + summary recompute as the direct path). The daemon
    // never inline-fires; all dispatch runs in `NotifyState::reconcile`.
    let outcome = tma_runtime::event::apply_event(
        tmux,
        lm,
        &ev.pane,
        &ev.kind,
        &ev.payload,
        &tma_runtime::event::NotifyPolicy {
            opt_in: false,
            on: &[],
            commands: &tma_runtime::config::NotifyCommands::default(),
            sinks: &tma_runtime::config::NotifySinks::default(),
        },
        now,
    );
    outcome == EventOutcome::Applied
}

/// Server-restart check (on the reconcile path): `true` only when the live `#{pid}` differs from the
/// startup identity (a same-`#{socket_path}` restart); else `false`, leaving a gone server to `ServerGone`.
fn server_restarted(tmux: &Tmux, startup_id: Option<&str>) -> bool {
    let Some(startup) = startup_id else {
        return false;
    };
    match tmux.display_active("#{pid}") {
        Ok(pid) => !pid.is_empty() && pid != startup,
        Err(_) => false,
    }
}

/// Re-read config + manifests from the SAME paths startup used (SIGHUP reload), swapping both in on
/// success. A failed parse/load keeps BOTH old (never corrupt a running daemon); the swap is all-or-nothing.
fn reload_state(
    config: &mut Config,
    manifests: &mut Vec<LoadedManifest>,
    config_path: Option<&Path>,
    manifest_dir: Option<&Path>,
) -> bool {
    match config::reload_pair(config, manifests, config_path, manifest_dir) {
        // A skipped user manifest is logged, not a failed reload: the rest of the set still swaps in.
        Ok(failures) => {
            log_manifest_failures(&failures);
            true
        }
        Err(msg) => {
            eprintln!("{msg}");
            false
        }
    }
}

/// The effective sweep cadence: the probe verdict picks the base (config normal cadence when push is
/// available, else the degraded const), and `--sweep-ms` overrides. Pure so startup and reload agree.
fn resolve_sweep(probe: ProbeOutcome, config: &Config, sweep_ms: Option<u64>) -> Duration {
    let base = match probe {
        // AssumedAvailable is a fail-open probe error: keep the normal cadence, same as Available.
        ProbeOutcome::Available | ProbeOutcome::AssumedAvailable => config.daemon.sweep(),
        ProbeOutcome::Unavailable => control::SWEEP_DEGRADED,
    };
    sweep_ms.map(Duration::from_millis).unwrap_or(base)
}

/// The effective sweep cadence for one loop iteration: while the pool is clientless, shorten to the
/// zero-member recheck so a gone server is caught within ~1 s; with clients the full sweep stands.
/// Pure over the flag and the two cadences so the selection is unit-testable off wall-clock.
fn effective_sweep(pool_empty: bool, sweep: Duration, empty_pool_recheck: Duration) -> Duration {
    if pool_empty {
        sweep.min(empty_pool_recheck)
    } else {
        sweep
    }
}

/// Sweep-due predicate: the reconciliation sweep runs once `now` is at least `sweep` past the last
/// one. `now` is threaded (not read here) so the cadence is deterministic under test; `saturating_`
/// keeps a non-advancing clock from panicking.
fn sweep_due(last_sweep: Instant, now: Instant, sweep: Duration) -> bool {
    now.saturating_duration_since(last_sweep) >= sweep
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reload derivation: the sweep cadence follows the probe verdict + reloaded config, and the
    /// `--sweep-ms` test override still wins, so a SIGHUP reload recomputes it consistently.
    #[test]
    fn resolve_sweep_follows_probe_and_override() {
        let cfg = Config::default();
        // Push available ⇒ the config's normal cadence.
        assert_eq!(
            resolve_sweep(ProbeOutcome::Available, &cfg, None),
            cfg.daemon.sweep()
        );
        // A probe error fails open ⇒ same normal cadence as a verified-available probe.
        assert_eq!(
            resolve_sweep(ProbeOutcome::AssumedAvailable, &cfg, None),
            cfg.daemon.sweep()
        );
        // Push unavailable ⇒ the fixed degraded const, independent of config.
        assert_eq!(
            resolve_sweep(ProbeOutcome::Unavailable, &cfg, None),
            control::SWEEP_DEGRADED
        );
        // The INTERNAL/TEST override wins over both probe verdicts.
        assert_eq!(
            resolve_sweep(ProbeOutcome::Available, &cfg, Some(500)),
            Duration::from_millis(500)
        );
    }

    /// Clientless iterations shorten the cadence to the recheck (so a gone server is caught within
    /// ~1 s); with clients the full sweep stands, and an already-shorter sweep is never lengthened.
    #[test]
    fn effective_sweep_shortens_only_while_clientless() {
        let sweep = Duration::from_secs(45);
        let recheck = Duration::from_secs(1);
        assert_eq!(
            effective_sweep(false, sweep, recheck),
            sweep,
            "clients ⇒ full sweep"
        );
        assert_eq!(
            effective_sweep(true, sweep, recheck),
            recheck,
            "clientless ⇒ recheck"
        );
        assert_eq!(
            effective_sweep(true, Duration::from_millis(500), recheck),
            Duration::from_millis(500),
            "an already-shorter sweep is not lengthened"
        );
    }

    /// The sweep fires at and past the cadence, not before, and a non-advancing clock is never due.
    #[test]
    fn sweep_due_fires_at_and_past_the_cadence() {
        let t0 = Instant::now();
        let sweep = Duration::from_secs(45);
        assert!(
            !sweep_due(t0, t0 + Duration::from_secs(44), sweep),
            "before ⇒ not due"
        );
        assert!(sweep_due(t0, t0 + sweep, sweep), "at the cadence ⇒ due");
        assert!(
            sweep_due(t0, t0 + Duration::from_secs(90), sweep),
            "past ⇒ due"
        );
        assert!(!sweep_due(t0, t0, sweep), "clock did not advance ⇒ not due");
    }
}
