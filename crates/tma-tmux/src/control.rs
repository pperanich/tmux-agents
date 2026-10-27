//! The daemon's per-session tmux control-mode client pool. Daemon-only and strictly additive:
//! one long-lived `tmux -C` client per session (keyed on the stable `session_id` `$N`) because
//! `%output` is session-scoped: a cross-session subscribe returns success while delivering
//! nothing, so [`probe_push`] tests real delivery, not command success. `%output` fires even under
//! an alternate-screen TUI, so it (not a `-B` screen format) is the activity source. No async
//! runtime: each client's stdout is a non-blocking fd in the daemon's one poll set. Bursts
//! collapse to one active→quiet edge past [`QUIET_THRESHOLD`] into a bounded queue the capture tier
//! drains (bounding CPU); that quiet edge is where hookless `blocked` is caught.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::os::unix::io::{AsFd, BorrowedFd};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};

mod decoder;
mod protocol;
#[cfg(test)]
#[path = "control/tests.rs"]
mod protocol_tests;
mod terminal;

use decoder::{Decoded, Decoder, Note};
use protocol::Collector;
pub use protocol::{
    ObservationBoundary, ProgramDrain, ProgramStats, ProgramStreamState, MAX_PROGRAM_EVENTS,
    MAX_PROGRAM_PAYLOAD_BYTES,
};
use tma_core::program_status::{ObservationGap, PaneId, SessionId};

use crate::tmux::{Tmux, TmuxError};

/// A `poll(2)` timeout as rustix 1.x wants it: `Some(&Timespec)` (was a `c_int` ms in 0.38).
fn poll_timeout(d: Duration) -> Timespec {
    Timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
        tv_nsec: d.subsec_nanos() as _,
    }
}

/// Default active→quiet threshold: a pane emits one edge once its `%output` has been silent this
/// long. Runtime value comes from `[daemon] quiet_ms`; this const is the zero-config default.
pub const QUIET_THRESHOLD: Duration = Duration::from_millis(1000);

/// Default reconciliation-sweep cadence when push is available: events drive state, the sweep only
/// repairs it. Runtime value from `[daemon] sweep_secs`; this const is the default.
pub const SWEEP_NORMAL: Duration = Duration::from_secs(45);

/// Sweep cadence when the probe reports push UNAVAILABLE: hookless `blocked` latency is then bounded
/// by this interval instead of the near-instant quiet edge.
pub const SWEEP_DEGRADED: Duration = Duration::from_secs(5);

/// Liveness recheck cadence while the pool is clientless: bounds how long a gone server goes
/// undetected with no client fd to EOF. Runtime value from `[daemon] zero_member_recheck_secs`.
pub const EMPTY_POOL_RECHECK: Duration = Duration::from_secs(1);

/// How soon the loop re-wakes to retry a post-attach seed whose `list-panes` read failed (see
/// [`seed_attached`]). Kept under [`QUIET_THRESHOLD`] so a retried seed still produces its edge on
/// the same cadence an ordinary `%output` burst would; and negligible next to what provoked it,
/// since a read that fails on a loaded box has already spent the 3 s `TMUX_TIMEOUT` getting there.
const SEED_RETRY: Duration = Duration::from_millis(250);

/// Cap on buffered activity edges (bounded memory). If the consumer drains slowly the oldest edges
/// are dropped; a lost capture trigger is self-healed by the reconciliation sweep.
const MAX_EDGES: usize = 1024;

/// Read chunk size off a control client's stdout.
const READ_CHUNK: usize = 16 * 1024;

/// Bound the read loop per client per wake so one chatty pane cannot starve the others.
const MAX_CHUNKS_PER_READ: usize = 64;

/// How long the probe waits for its marker `%output` before declaring push unavailable. Only the
/// degraded path (nothing arrives) waits the full budget; available is detected in a few hundred ms.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2000);

/// The probe session's marker line and the command that emits it (~3 s, then self-exits as a
/// safety net so a probe session never lingers if the daemon dies mid-probe).
const PROBE_MARKER: &str = "TMA_PUSH_PROBE";
const PROBE_CMD: &str =
    "sh -c 'i=0; while [ $i -lt 60 ]; do echo TMA_PUSH_PROBE; sleep 0.05; i=$((i+1)); done'";

/// An active→quiet edge for one pane, surfaced to the capture tier: the quiet moment is when a
/// hookless `blocked` prompt is caught (it stops output). `at` is epoch milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEdge {
    pub pane: String,
    pub at: u64,
}

/// Activity and lifecycle hints, independent of protocol collection.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Notification {
    Output { pane: String },
    SessionsChanged,
    Lifecycle,
    Exit,
}

/// One long-lived `tmux -C attach-session -t <session_id>` control client. Owns the child and
/// reaps it on drop (kill + wait) so no control client leaks.
struct ControlClient {
    /// Held open, never written by pool clients: `tmux -C` exits on stdin EOF, so keeping the
    /// pipe's write end alive is what keeps the client attached.
    _stdin: ChildStdin,
    child: Child,
    stdout: ChildStdout,
    decoder: Decoder,
    serial: u64,
    /// `true` once the client has produced its first control-mode byte, which is the attach: tmux
    /// streams `%output` from the attach on, never replaying what a pane printed before it. Until
    /// then the pool holds a client but no coverage.
    attached: bool,
    /// EOF or a read error was seen: this client is defunct and will be reaped + re-attached
    /// on the next reconcile.
    dead: bool,
}

impl ControlClient {
    /// Spawn a control client attached to `session_id`. stdout/stdin are pipes; stdout is set
    /// non-blocking so the daemon's `poll` loop can drain it without blocking.
    fn spawn(tmux: &Tmux, session_id: &str, _session_name: &str) -> std::io::Result<ControlClient> {
        let mut cmd = tmux.control_client_command(session_id);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        if let Err(error) = set_nonblocking(&stdout) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(ControlClient {
            _stdin: stdin,
            child,
            stdout,
            decoder: Decoder::default(),
            serial: 0,
            attached: false,
            dead: false,
        })
    }

    /// Drain all currently-available bytes into notifications. Non-blocking: returns when the
    /// pipe would block, on EOF (marks `dead`), or after [`MAX_CHUNKS_PER_READ`] chunks.
    fn read_available(
        &mut self,
        out: &mut Vec<Notification>,
        mut protocol: Option<&mut Collector>,
        session: SessionId,
    ) {
        for _ in 0..MAX_CHUNKS_PER_READ {
            let mut chunk = [0u8; READ_CHUNK];
            match self.stdout.read(&mut chunk) {
                Ok(0) => {
                    if let Some(collector) = protocol.as_deref_mut() {
                        self.decoder
                            .finish(|event| collector.decoded(session, self.serial, event));
                    }
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    // First byte off a control client is its attach handshake: coverage of the
                    // session starts here, not at spawn.
                    self.attached = true;
                    self.ingest(&chunk[..n], out, protocol.as_deref_mut(), session);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    if let Some(collector) = protocol.as_deref_mut() {
                        self.decoder
                            .finish(|event| collector.decoded(session, self.serial, event));
                    }
                    self.dead = true;
                    return;
                }
            }
        }
    }

    fn ingest(
        &mut self,
        data: &[u8],
        out: &mut Vec<Notification>,
        mut protocol: Option<&mut Collector>,
        session: SessionId,
    ) {
        let enabled = protocol.is_some();
        let serial = self.serial;
        let dead = &mut self.dead;
        self.decoder.feed(data, enabled, |event| {
            match event {
                Decoded::Start(pane) => out.push(Notification::Output {
                    pane: format!("%{}", pane.get()),
                }),
                Decoded::Note(note) => out.push(match note {
                    Note::SessionsChanged => Notification::SessionsChanged,
                    Note::Lifecycle => Notification::Lifecycle,
                    Note::Exit => {
                        *dead = true;
                        if let Some(collector) = protocol.as_deref_mut() {
                            collector.client_gap(session, ObservationGap::Disconnected);
                        }
                        Notification::Exit
                    }
                }),
                Decoded::Fatal => *dead = true,
                _ => {}
            }
            if let Some(collector) = protocol.as_deref_mut() {
                collector.decoded(session, serial, event);
            }
        });
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        // Reap unconditionally (no leaked control clients). `kill` is a harmless no-op if the
        // child already exited (`%exit`/server-gone); `wait` reaps it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The per-session control-client pool and its derived push state. Owned by the daemon loop;
/// there is no interior mutability or locking (single-threaded).
pub struct ControlPool {
    /// Per-pane active→quiet threshold (config `[daemon] quiet_ms`; defaults to
    /// [`QUIET_THRESHOLD`]). Held here so the whole pool reads one canonical value.
    quiet_threshold: Duration,
    /// One client per session, keyed by stable `session_id` (`$N`).
    clients: HashMap<String, ControlClient>,
    /// Panes with `%output` seen but not yet gone quiet: pane id → last-output instant.
    active: HashMap<String, Instant>,
    /// Bounded active→quiet edge queue for the capture tier.
    edges: VecDeque<ActivityEdge>,
    /// Stable session IDs whose client has attached but whose panes [`seed_attached`] has not
    /// looked at yet: they were uncovered until that moment, so each needs one look.
    newly_attached: Vec<String>,
    /// Monotone counters for the introspection status file (tests + operators).
    edges_emitted: u64,
    recoveries: u64,
    /// How many post-attach seeds were deferred because their `list-panes` read failed. Nonzero
    /// means the box was slow enough to trip `TMUX_TIMEOUT` mid-attach, which is the
    /// one thing that turns a quiet-edge latency into a sweep-cadence one.
    seed_retries: u64,
    /// Whether the pool has ever held ≥1 client, so the initial startup populate is not
    /// miscounted as a zero-member recovery.
    ever_populated: bool,
    next_client: u64,
    protocol: Option<Collector>,
    protocol_epoch: u64,
}

impl Default for ControlPool {
    fn default() -> Self {
        Self::new(QUIET_THRESHOLD)
    }
}

impl ControlPool {
    pub fn new(quiet_threshold: Duration) -> ControlPool {
        ControlPool {
            quiet_threshold,
            clients: HashMap::new(),
            active: HashMap::new(),
            edges: VecDeque::new(),
            newly_attached: Vec::new(),
            edges_emitted: 0,
            recoveries: 0,
            seed_retries: 0,
            ever_populated: false,
            next_client: 0,
            protocol: None,
            protocol_epoch: 0,
        }
    }

    /// Swap the active→quiet threshold (SIGHUP reload of `[daemon] quiet_ms`). Live clients, the
    /// active-pane map, and the edge queue are preserved, so a reload keeps control-mode state.
    pub fn set_quiet_threshold(&mut self, quiet_threshold: Duration) {
        self.quiet_threshold = quiet_threshold;
    }

    /// Enable the optional protocol collector. Mode changes discard all previous
    /// candidates and queues. Call `reconcile` before establishing pane boundaries.
    /// Disabled pools retain activity and allocate no protocol scanner/store.
    pub fn set_program_collection(&mut self, enabled: bool) {
        if let Some(collector) = self.protocol.take() {
            self.protocol_epoch = collector.last_epoch();
        }
        self.protocol = enabled.then(|| Collector::with_epoch(self.protocol_epoch));
    }

    /// Discard everything observed before process binding. The current terminal
    /// string framing survives. A notification already in progress is quarantined
    /// through its newline, so an earlier octal escape cannot begin a new report.
    /// Drain invalidations first, then accept only this epoch above its floor.
    pub fn program_boundary(&mut self, pane: PaneId) -> Option<ObservationBoundary> {
        let (owner, serial) = self.protocol.as_ref()?.owner(pane)?;
        let in_payload = self
            .clients
            .get(&format!("${}", owner.get()))
            .is_some_and(|client| client.serial == serial && client.decoder.in_payload(pane));
        self.protocol.as_mut()?.boundary(pane, in_payload)
    }

    /// Revalidate a decision against losses since its last drain. Merely queued
    /// newer observations do not change the connection. Unavailable topology,
    /// whole-scope loss and disabled collection expose no usable stream.
    pub fn program_stream(&self, pane: PaneId) -> Option<ProgramStreamState> {
        self.protocol.as_ref()?.stream(pane)
    }

    /// The selected stream's stable session context, including linked panes.
    pub fn program_session(&self, pane: PaneId) -> Option<SessionId> {
        self.protocol
            .as_ref()?
            .owner(pane)
            .map(|(session, _)| session)
    }

    /// Runtime budget/identity revocation uses the same out-of-band loss seam.
    pub fn invalidate_program(&mut self, pane: PaneId, reason: ObservationGap) {
        if let Some(collector) = &mut self.protocol {
            collector.invalidate(pane, reason, false);
        }
    }

    /// Pending owned decoded payload plus partial terminal candidates, for the
    /// runtime's global 32 MiB budget. Allocator/container overhead is separate.
    pub fn pending_program_bytes(&self) -> usize {
        self.protocol.as_ref().map_or(0, Collector::pending_bytes)
    }

    /// Remaining pending-payload allowance after runtime-owned retained stores.
    /// Set before reading control clients and after reducing/draining records.
    /// Lowering it revokes affected panes through out-of-band budget gaps.
    pub fn set_program_payload_budget(&mut self, available: usize) {
        if let Some(collector) = &mut self.protocol {
            collector.set_payload_budget(available);
        }
    }

    /// Count-only diagnostics for this collection interval. Disabled collection
    /// has no stats or protocol storage; no raw terminal text is exposed.
    pub fn program_stats(&self) -> Option<ProgramStats> {
        self.protocol.as_ref().map(Collector::stats)
    }

    /// Apply `all_invalidated`, then pane invalidations, then observations.
    /// Empty when collection is disabled. Queries never generate terminal input.
    pub fn drain_program(&mut self) -> ProgramDrain {
        self.protocol
            .as_mut()
            .map_or_else(ProgramDrain::default, Collector::drain)
    }

    /// Sync pool membership to the live `list-sessions` set: reap dead + gone-session clients, spawn
    /// one client per session that lacks one. Idempotent; propagates `ServerGone` to exit cleanly.
    pub fn reconcile(&mut self, tmux: &Tmux) -> Result<(), TmuxError> {
        // Reap defunct clients first (EOF/`%exit`/error). `retain` drops the removed values,
        // and `ControlClient::drop` kills + waits, so nothing leaks or zombifies.
        if let Some(collector) = &mut self.protocol {
            for (session, _) in self.clients.iter().filter(|(_, client)| client.dead) {
                if let Ok(session) = SessionId::parse(session) {
                    collector.client_gap(session, ObservationGap::Disconnected);
                }
            }
        }
        self.clients.retain(|_, c| !c.dead);

        let sessions = match tmux.list_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                if let Some(collector) = &mut self.protocol {
                    collector.topology_failed();
                }
                return Err(error);
            }
        };
        let live: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        self.clients.retain(|sid, _| live.contains(sid.as_str()));

        // Zero-member recovery detection: sessions survive but every client was lost.
        let survivors = self.clients.len();
        if self.ever_populated && survivors == 0 && !sessions.is_empty() {
            self.recoveries += 1;
        }

        for s in &sessions {
            if !self.clients.contains_key(&s.id) {
                // Best-effort: a spawn failure retries on the next reconcile rather than
                // failing the daemon.
                if let Ok(mut client) = ControlClient::spawn(tmux, &s.id, &s.name) {
                    self.next_client = self
                        .next_client
                        .checked_add(1)
                        .expect("client serial exhausted");
                    client.serial = self.next_client;
                    self.clients.insert(s.id.clone(), client);
                }
            }
        }
        if !self.clients.is_empty() {
            self.ever_populated = true;
        }
        if let Some(collector) = &mut self.protocol {
            let topology = match tmux.control_topology() {
                Ok(topology) => topology,
                Err(error) => {
                    collector.topology_failed();
                    return Err(error);
                }
            };
            let clients: BTreeMap<_, _> = self
                .clients
                .iter()
                .filter_map(|(session, client)| {
                    Some((SessionId::parse(session).ok()?, client.serial))
                })
                .collect();
            collector.reconcile(&topology, &clients);
        }
        Ok(())
    }

    /// The `(session_id, stdout fd)` list for building the daemon's `poll` set this iteration; each
    /// `BorrowedFd` borrows its client's stdout in the pool, so the caller must end the borrow
    /// (extract the session ids) before mutating the pool.
    fn client_fds(&self) -> Vec<(String, BorrowedFd<'_>)> {
        self.clients
            .iter()
            .map(|(sid, c)| (sid.clone(), c.stdout.as_fd()))
            .collect()
    }

    /// Drain the client whose fd polled readable, appending its notifications to `out`. A client
    /// crossing into `attached` here records its session for the one post-attach look.
    fn read_client(&mut self, session_id: &str, out: &mut Vec<Notification>) {
        if let Some(c) = self.clients.get_mut(session_id) {
            let was_attached = c.attached;
            let Ok(session) = SessionId::parse(session_id) else {
                return;
            };
            c.read_available(out, self.protocol.as_mut(), session);
            if !was_attached && c.attached {
                self.newly_attached.push(session_id.to_owned());
            }
        }
    }

    /// `true` if any client hit EOF/error and needs reaping + re-attach on the next reconcile.
    fn has_dead_client(&self) -> bool {
        self.clients.values().any(|c| c.dead)
    }

    /// Record a `%output` activity mark for `pane` (re-activates a quiet pane).
    fn mark_active(&mut self, pane: String, now: Instant) {
        self.active.insert(pane, now);
    }

    /// Emit an active→quiet edge for every pane silent past [`QUIET_THRESHOLD`], removing it
    /// from the active set so exactly one edge fires per output burst. Bounded queue.
    fn check_quiet_edges(&mut self, now: Instant, now_epoch: u64) {
        let quiet: Vec<String> = self
            .active
            .iter()
            .filter(|(_, last)| now.duration_since(**last) >= self.quiet_threshold)
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in quiet {
            self.active.remove(&pane);
            if self.edges.len() >= MAX_EDGES {
                self.edges.pop_front();
            }
            self.edges.push_back(ActivityEdge {
                pane,
                at: now_epoch,
            });
            self.edges_emitted += 1;
        }
    }

    /// Poll timeout: the nearest active pane's quiet deadline (so the loop wakes to emit its edge),
    /// else the `sweep` cadence. Clamped to [10ms, sweep] so it never busy-spins. `now` is the
    /// caller's monotone clock, threaded in so the clamp is deterministic under test. A deferred
    /// post-attach seed shortens it to [`SEED_RETRY`]: nothing else in the pool is waiting on a
    /// timer then, so without this the retry would sit until the next sweep.
    fn poll_timeout(&self, now: Instant, sweep: Duration) -> Duration {
        let nearest = self
            .active
            .values()
            .map(|last| (*last + self.quiet_threshold).saturating_duration_since(now))
            .min();
        let base = match nearest {
            Some(rem) => rem.max(Duration::from_millis(10)).min(sweep),
            None => sweep,
        };
        if self.newly_attached.is_empty() {
            base
        } else {
            base.min(SEED_RETRY)
        }
    }

    /// Drain the buffered activity edges (the consumer seam): the daemon hands them to the capture
    /// tier each iteration, where hookless `blocked` is caught.
    pub fn drain_edges(&mut self) -> Vec<ActivityEdge> {
        self.edges.drain(..).collect()
    }

    /// Total active→quiet edges emitted over the pool's life (monotone; introspection).
    pub fn edges_emitted(&self) -> u64 {
        self.edges_emitted
    }

    /// `true` while a post-attach seed is still owed a successful `list-panes` (see
    /// [`seed_attached`]). The serve loop flushes status on it, so a run that hit the slow-box
    /// path says so in its counters instead of only looking mysteriously late.
    pub fn pending_seed(&self) -> bool {
        !self.newly_attached.is_empty()
    }

    /// `true` when the pool holds no clients. The daemon then shortens its poll timeout so a gone
    /// server is caught by the next `list-sessions` within ~1 s (the only periodic liveness recheck,
    /// and only while clientless; with clients a gone server surfaces promptly as `%exit`/EOF).
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Write the introspection status file (tests + operators): pool membership, probe verdict,
    /// sweep interval, edge count, active-pane gauge, recovery count, plus `extra`'s capture/notify
    /// `key=value` lines.
    ///
    /// The `active` gauge (panes seen output but not yet quiet) settles to zero and is written on
    /// every change. `active == 0` implies "no capture pending" only by serve-loop ordering: the
    /// daemon runs the drained edges' captures BEFORE this write. It does NOT prove tmux holds no
    /// read-but-undelivered output, so a quiescence poller adds a settle margin (see
    /// `wait_daemon_quiescent`). Reordering the capture after this write voids the implication.
    pub fn write_status(
        &self,
        path: &std::path::Path,
        probe: ProbeOutcome,
        sweep: Duration,
        extra: &str,
    ) {
        let mut sids: Vec<&String> = self.clients.keys().collect();
        sids.sort();
        let sessions = sids
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let body = format!(
            "probe={probe}\nsweep_ms={sweep_ms}\ndegraded={degraded}\nclients={clients}\n\
             sessions={sessions}\nedges={edges}\nactive={active}\nrecoveries={recoveries}\n\
             pending_seeds={pending_seeds}\nseed_retries={seed_retries}\n{extra}",
            probe = probe.as_str(),
            sweep_ms = sweep.as_millis(),
            degraded = u8::from(probe == ProbeOutcome::Unavailable),
            clients = self.clients.len(),
            edges = self.edges_emitted,
            active = self.active.len(),
            recoveries = self.recoveries,
            pending_seeds = self.newly_attached.len(),
            seed_retries = self.seed_retries,
        );
        // Best-effort, atomic-ish: write a temp then rename so a reader never sees a torn file.
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// The behavior-probe verdict for control-mode activity push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Session-scoped push works: the probe delivered its marker. Rely on quiet-edge detection at
    /// the normal sweep cadence.
    Available,
    /// The probe itself errored (could not create/attach the throwaway session), so delivery was
    /// never observed. Fail OPEN: treated like `Available` for sweep cadence (a transient hiccup
    /// must not pin the daemon to the degraded sweep), but reported distinctly so a genuinely broken
    /// push path whose probe errors is not silently logged as verified-available.
    AssumedAvailable,
    /// Push is silently useless (the cross-session-subscribe failure mode, or an old floor):
    /// degrade to the faster reconciliation sweep and restate the latency numbers.
    Unavailable,
}

impl ProbeOutcome {
    fn as_str(self) -> &'static str {
        match self {
            ProbeOutcome::Available => "available",
            ProbeOutcome::AssumedAvailable => "assumed-available",
            ProbeOutcome::Unavailable => "unavailable",
        }
    }
}

/// Behavior-probe activity push: test delivery, not command success (a cross-session subscribe
/// succeeds while delivering nothing). Spin up a throwaway marker-emitting probe session, attach a
/// control client, and confirm the marker's `%output` arrives within [`PROBE_TIMEOUT`]. The probe
/// pane is a fresh process in a session tma owns and kills after, not input injection.
///
/// `cross_session` forces the useless config (attach to a *different* live session than the probe
/// pane's) so the marker never arrives, exercising the degrade path.
pub fn probe_push(tmux: &Tmux, cross_session: bool) -> ProbeOutcome {
    let name = format!("__tma_probe_{}", std::process::id());
    let (probe_sid, _probe_pane) = match tmux.new_probe_session(&name, PROBE_CMD) {
        Ok(x) => x,
        // Could not create the probe session: fail OPEN, so a transient hiccup never pins the daemon
        // to the degraded sweep (the sweep still repairs real delivery failures). AssumedAvailable
        // keeps that cadence while recording that delivery was never actually observed.
        Err(_) => {
            // The error may be a `TMUX_TIMEOUT` expiry, which says only that the CALL did not
            // return in 3 s — tmux may well have created the session, and we never got its id.
            // Kill it by the name we chose, or the pool adopts a session the daemon does not know
            // it owns and every "this daemon has one client" reader sees two forever.
            let _ = tmux.kill_session(&name);
            return ProbeOutcome::AssumedAvailable;
        }
    };

    // Attach target: the probe session itself (should deliver its marker) or, for the forced
    // degrade, a different live session (must NOT deliver the probe session's output).
    let attach = if cross_session {
        tmux.list_sessions()
            .ok()
            .and_then(|list| list.into_iter().find(|s| s.id != probe_sid).map(|s| s.id))
    } else {
        Some(probe_sid.clone())
    };

    let outcome = match attach {
        Some(a) => match probe_watch(tmux, &a) {
            Ok(true) => ProbeOutcome::Available,
            Ok(false) => ProbeOutcome::Unavailable,
            Err(_) => ProbeOutcome::AssumedAvailable, // attach hiccup: fail open (see above)
        },
        // cross_session requested but the probe session is the only one ⇒ cannot demonstrate the
        // failure; report Available (the non-forced probe would pass here anyway).
        None => ProbeOutcome::Available,
    };

    // Retry the teardown by name once if the id-keyed kill failed: on a loaded box that failure is
    // a `TMUX_TIMEOUT` expiry rather than a gone session, and a probe session left behind is one
    // the pool will attach a client to and hold for the daemon's whole life.
    if tmux.kill_session(&probe_sid).is_err() {
        let _ = tmux.kill_session(&name);
    }
    outcome
}

/// Attach a control client to `attach_session` and watch its `%output` stream for
/// [`PROBE_MARKER`] within [`PROBE_TIMEOUT`]. The client is reaped on return.
fn probe_watch(tmux: &Tmux, attach_session: &str) -> std::io::Result<bool> {
    // The probe's client never joins the pool, so its session name is irrelevant here.
    let mut client = ControlClient::spawn(tmux, attach_session, "")?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    // A small rolling buffer: keep only enough tail to catch a marker split across reads.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let ts = poll_timeout(remaining);
        // The PollFd borrows `client.stdout`; scope it so the `read` below can take `&mut`.
        let ready = {
            let mut fds = [PollFd::new(&client.stdout, PollFlags::IN)];
            matches!(poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
        };
        if !ready {
            continue; // timeout slice, EINTR, or error: the outer deadline ends the loop
        }
        let mut chunk = [0u8; READ_CHUNK];
        match client.stdout.read(&mut chunk) {
            Ok(0) => return Ok(false), // EOF before the marker
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf
                    .windows(PROBE_MARKER.len())
                    .any(|w| w == PROBE_MARKER.as_bytes())
                {
                    return Ok(true);
                }
                // Keep the buffer bounded: retain only the last marker-length-minus-one bytes
                // so a match spanning the next read is still catchable.
                let keep = PROBE_MARKER.len().saturating_sub(1);
                if buf.len() > keep {
                    let cut = buf.len() - keep;
                    buf.drain(..cut);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    // `client` drops here → reaped.
}

/// Recompute each live window's `@agent_summary` and each session's `@agent_session_summary` from
/// stored pane `@agent_state` on a lifecycle event, writing only on drift, so a closed agent pane
/// leaves both rollups even with no `SessionEnd`.
pub fn reconcile_summaries(tmux: &Tmux) -> Result<(), TmuxError> {
    crate::stamp::reconcile_summaries(tmux)
}

/// Aggregated effect of the notifications drained in one loop iteration: the daemon acts on
/// these flags after reading every ready client.
#[derive(Default)]
pub struct LoopEffects {
    pub need_reconcile: bool,
    pub need_summary: bool,
}

/// Read every readable client, mark activity, and fold notifications into [`LoopEffects`]. `ready`
/// is the set of session ids whose fd showed `POLLIN` this iteration.
pub fn dispatch_ready(
    pool: &mut ControlPool,
    ready: &HashSet<String>,
    now: Instant,
) -> LoopEffects {
    let mut notes = Vec::new();
    for sid in ready {
        pool.read_client(sid, &mut notes);
    }
    let mut fx = LoopEffects::default();
    for n in notes {
        match n {
            Notification::Output { pane } => pool.mark_active(pane, now),
            Notification::SessionsChanged => fx.need_reconcile = true,
            Notification::Lifecycle => {
                fx.need_summary = true;
                fx.need_reconcile = true;
            }
            Notification::Exit => fx.need_reconcile = true,
        }
    }
    if pool.has_dead_client() {
        fx.need_reconcile = true;
    }
    fx
}

/// Mark every pane of a just-attached session active, so each emits one active→quiet edge and the
/// capture tier looks at it once. tmux streams `%output` only from the attach on, so a pane that
/// printed its blocked prompt while the client was still starting produces no further output and
/// would otherwise sit unnoticed until a sweep. Cost is one `list-panes` per attach (daemon start,
/// a new session, a client respawn) plus the capture tier's usual one-capture-per-agent-pane; a
/// pane still producing output just has its mark refreshed, so the edge still fires exactly once.
///
/// A failed read DEFERS the seed rather than dropping it: the queue is taken only once `list-panes`
/// has actually returned. That read is a `tmux` one-shot under the 3 s `TMUX_TIMEOUT`
/// cap, and on a CPU-saturated box (a 3-core CI runner mid-`cargo test`, where process spawn alone
/// measures p50 3.8 s) it times out routinely. Dropping the seed there costs the pane a full sweep
/// cadence — 45 s of an unnoticed blocked prompt — which is precisely the gap this function exists
/// to close, so a transient failure must not spend it. [`ControlPool::poll_timeout`] shortens the
/// next wake to [`SEED_RETRY`] while a seed is pending, so the retry is prompt rather than a sweep away.
pub fn seed_attached(pool: &mut ControlPool, tmux: &Tmux, now: Instant) {
    if pool.newly_attached.is_empty() {
        return;
    }
    let Ok(panes) = tmux.control_topology() else {
        pool.seed_retries += 1;
        return;
    };
    let sessions = std::mem::take(&mut pool.newly_attached);
    for (session, pane) in panes {
        if sessions.contains(&format!("${}", session.get())) {
            pool.mark_active(format!("%{}", pane.get()), now);
        }
    }
}

/// The poll timeout the pool wants right now, without advancing its timers. [`tick`] answers the
/// same question at the top of a loop iteration; this is for after the edge drain, which can re-arm
/// a pane ([`mark_recheck`]) whose quiet deadline that earlier answer could not have seen.
pub fn next_timeout(pool: &ControlPool, now: Instant, sweep: Duration) -> Duration {
    pool.poll_timeout(now, sweep)
}

/// Re-mark `panes` active so each emits one more active→quiet edge, giving the capture tier a
/// second look at a pane whose verdict turned on something other than its screen. Same mechanism as
/// [`seed_attached`] and no cheaper: a pane still producing output just has its mark refreshed, so
/// it still yields exactly one edge. The caller owns the retry budget; the pool counts no
/// difference between these marks and a `%output` one.
pub fn mark_recheck(pool: &mut ControlPool, panes: &[String], now: Instant) {
    for pane in panes {
        pool.mark_active(pane.clone(), now);
    }
}

/// Advance the pool's timers once per loop wake: emit any due quiet edges, then report the
/// poll timeout for the next wait. Time enters here at the boundary (the serve loop's clocks):
/// `now` is the monotone deadline clock and `now_epoch` the wall epoch stamped onto emitted edges;
/// `sweep` is the current (possibly degraded) sweep cadence. Threading both keeps the tick's
/// decision math clock-free for unit tests.
pub fn tick(pool: &mut ControlPool, now: Instant, now_epoch: u64, sweep: Duration) -> Duration {
    pool.check_quiet_edges(now, now_epoch);
    pool.poll_timeout(now, sweep)
}

/// The `(session_id, borrowed stdout fd)` poll set contribution of the pool this iteration. Each fd
/// borrows the pool, so end the borrow (extract session ids) before any mutable pool use.
pub fn pollfds(pool: &ControlPool) -> Vec<(String, BorrowedFd<'_>)> {
    pool.client_fds()
}

fn set_nonblocking(fd: &ChildStdout) -> std::io::Result<()> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_edge_per_burst_then_reactivation() {
        let mut pool = ControlPool::default();
        let t0 = Instant::now();
        // A burst: three output marks close together for the same pane.
        pool.mark_active("%1".into(), t0);
        pool.mark_active("%1".into(), t0 + Duration::from_millis(50));
        pool.mark_active("%1".into(), t0 + Duration::from_millis(100));
        // Before the threshold: no edge.
        pool.check_quiet_edges(t0 + Duration::from_millis(200), 1000);
        assert_eq!(pool.edges.len(), 0);
        // Past the threshold from the LAST mark: exactly one edge.
        pool.check_quiet_edges(t0 + QUIET_THRESHOLD + Duration::from_millis(150), 1000);
        assert_eq!(pool.edges.len(), 1);
        assert_eq!(pool.edges[0].pane, "%1");
        // Re-check after: still one (pane no longer active).
        pool.check_quiet_edges(t0 + QUIET_THRESHOLD * 3, 1000);
        assert_eq!(pool.edges.len(), 1);
        // A new burst re-activates and produces a second edge.
        let t1 = t0 + QUIET_THRESHOLD * 4;
        pool.mark_active("%1".into(), t1);
        pool.check_quiet_edges(t1 + QUIET_THRESHOLD + Duration::from_millis(10), 2000);
        assert_eq!(pool.edges.len(), 2);
        assert_eq!(pool.edges_emitted, 2);
    }

    #[test]
    fn edge_queue_is_bounded() {
        let mut pool = ControlPool::default();
        let t0 = Instant::now();
        for i in 0..(MAX_EDGES + 50) {
            let pane = format!("%{i}");
            pool.mark_active(pane, t0);
        }
        pool.check_quiet_edges(t0 + QUIET_THRESHOLD * 2, 1000);
        assert_eq!(pool.edges.len(), MAX_EDGES, "queue capped");
        assert_eq!(
            pool.edges_emitted as usize,
            MAX_EDGES + 50,
            "count is monotone"
        );
    }

    #[test]
    fn poll_timeout_is_sweep_when_idle_and_shorter_when_active() {
        let mut pool = ControlPool::default();
        let sweep = Duration::from_secs(45);
        let now = Instant::now();
        assert_eq!(pool.poll_timeout(now, sweep), sweep, "idle ⇒ long sweep");
        pool.mark_active("%1".into(), now);
        let t = pool.poll_timeout(now, sweep);
        assert!(
            t <= QUIET_THRESHOLD && t < sweep,
            "active ⇒ near quiet deadline, got {t:?}"
        );
    }

    /// A `Tmux` whose binary cannot be spawned, standing in for the read that trips the 3 s
    /// `TMUX_TIMEOUT` on a saturated box: both reach `seed_attached` as `Err`, which is the only
    /// distinction the function makes.
    fn unreadable_tmux() -> Tmux {
        Tmux::connect(&crate::tmux::Server {
            bin: Some("/nonexistent/tmux".into()),
            ..Default::default()
        })
    }

    #[test]
    fn a_failed_seed_read_keeps_the_attach_queue_and_shortens_the_next_wake() {
        let mut pool = ControlPool::default();
        pool.newly_attached.push("s1".into());
        let now = Instant::now();

        seed_attached(&mut pool, &unreadable_tmux(), now);

        // The queue survives: dropping it would leave every pane that printed during the attach
        // window to the next sweep, which is the whole latency the seed exists to avoid.
        assert_eq!(
            pool.newly_attached,
            ["s1"],
            "a failed read must not consume the queue"
        );
        assert_eq!(
            pool.seed_retries, 1,
            "the deferral is counted for the status file"
        );
        assert!(pool.pending_seed());
        // Nothing is active, so without the pending-seed clamp the loop would block a full sweep
        // before retrying.
        let t = pool.poll_timeout(now, SWEEP_NORMAL);
        assert_eq!(
            t, SEED_RETRY,
            "a pending seed must shorten the wake, got {t:?}"
        );
    }

    #[test]
    fn repeated_seed_deferrals_do_not_duplicate_the_queue_entry() {
        // The retry is idempotent by construction: the queue holds stable session IDs, and a repeat
        // deferral cannot enqueue the same session twice (only an attach does).
        let mut pool = ControlPool::default();
        pool.newly_attached.push("s1".into());
        let tmux = unreadable_tmux();
        seed_attached(&mut pool, &tmux, Instant::now());
        seed_attached(&mut pool, &tmux, Instant::now());
        assert_eq!(
            pool.newly_attached,
            ["s1"],
            "retries must not duplicate the entry"
        );
        assert_eq!(pool.seed_retries, 2);
    }

    #[test]
    fn poll_timeout_clamps_around_a_due_or_overdue_quiet_deadline() {
        let mut pool = ControlPool::default();
        let sweep = Duration::from_secs(45);
        let t0 = Instant::now();
        pool.mark_active("%1".into(), t0);
        // Halfway to the quiet threshold: the exact remaining time, unclamped.
        assert_eq!(
            pool.poll_timeout(t0 + QUIET_THRESHOLD / 2, sweep),
            QUIET_THRESHOLD / 2,
            "future deadline ⇒ wait its remaining time"
        );
        // Exactly due: remaining is zero, clamped up to the 10 ms floor (no busy-spin).
        assert_eq!(
            pool.poll_timeout(t0 + QUIET_THRESHOLD, sweep),
            Duration::from_millis(10),
            "due ⇒ 10 ms floor"
        );
        // Overdue: the deadline passed; still the floor, never zero.
        assert_eq!(
            pool.poll_timeout(t0 + QUIET_THRESHOLD * 2, sweep),
            Duration::from_millis(10),
            "overdue ⇒ 10 ms floor"
        );
    }

    #[test]
    fn poll_timeout_is_capped_at_the_sweep_when_the_deadline_is_far() {
        // A quiet threshold beyond the (degraded) sweep: the nearest deadline exceeds the sweep, so
        // the sweep cadence caps the wait and still fires on time.
        let mut pool = ControlPool::new(Duration::from_secs(60));
        let sweep = SWEEP_DEGRADED; // 5 s
        let now = Instant::now();
        pool.mark_active("%1".into(), now);
        assert_eq!(
            pool.poll_timeout(now, sweep),
            sweep,
            "deadline beyond sweep ⇒ capped at the sweep cadence"
        );
    }

    #[test]
    fn drain_edges_empties_the_queue() {
        let mut pool = ControlPool::default();
        let t0 = Instant::now();
        pool.mark_active("%1".into(), t0);
        pool.check_quiet_edges(t0 + QUIET_THRESHOLD * 2, 1234);
        let drained = pool.drain_edges();
        assert_eq!(
            drained,
            vec![ActivityEdge {
                pane: "%1".into(),
                at: 1234
            }]
        );
        assert!(pool.drain_edges().is_empty());
    }
}
