//! The concrete daemon-owned collector. Runtime owns attribution; this module
//! owns deadlines, exact persistent cleanup and ordered outward effects.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::sys::shutdown_requested;
use tma_core::persistence::{
    CollectorLease, CommitMarker, LeaseAction, LeaseSchedule, LeaseTime, PaneOwner,
    PersistedResult, ProjectionRead,
};
use tma_core::program_status::{PaneId, SessionId};
use tma_core::snapshot::ProcessUnavailable;
use tma_core::stamp::opt;
use tma_runtime::config::ProgramStatusMode;
use tma_runtime::identity::{self, Registration};
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::program_status::{
    publication::{release_persisted_checked, ProgramPublishError},
    ProgramDecision, ProgramPane, ProgramRecheckError, ProgramRelease, ProgramRuntime, ProgramStep,
    ProgramStepKind, RecoveryReason,
};
use tma_tmux::control::{self, ControlPool};
use tma_tmux::persistence::{self as store, LeaseOutcome};
use tma_tmux::tmux::{program_processes, PaneRecord, ProcessSnapshot, Tmux, TmuxError};

use super::subscribers::push_subscribers;
use crate::notify::NotifyState;
use std::os::unix::net::UnixStream;

mod acknowledgement;
mod effects;
mod maintenance;

struct RunInputs<'a> {
    tmux: &'a Tmux,
    pool: &'a mut ControlPool,
    manifests: &'a [LoadedManifest],
    notify: &'a mut NotifyState,
    subscribers: &'a mut Vec<UnixStream>,
}
enum PhaseOutcome {
    Continue(bool),
    Stop,
}
enum LeaseReadiness {
    Ready,
    Unavailable,
    Stopped,
}

const PROCESS_CADENCE: Duration = Duration::from_secs(1);
const DIAGNOSTIC_PANES: usize = 128;
const PANE_OPERATIONS: usize = 8;
const PANE_IO_SHARE: Duration = Duration::from_secs(2);
const BATCH_OPERATIONS: usize = 32;
const BATCH_IO_WINDOW: Duration = Duration::from_secs(5);
const CLEANUP_OPERATIONS: usize = 4;
const CLEANUP_WINDOW: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchStop {
    Shutdown,
    WorkBudget,
    LeaseLost,
    ProcessUnavailable(ProcessUnavailable),
    PersistenceLost,
}
impl BatchStop {
    fn reason(self) -> RecoveryReason {
        match self {
            Self::Shutdown => RecoveryReason::Disabled,
            Self::WorkBudget => RecoveryReason::WorkBudgetExceeded,
            Self::LeaseLost => RecoveryReason::LeaseLost,
            Self::PersistenceLost => RecoveryReason::PersistenceLost,
            Self::ProcessUnavailable(reason) => RecoveryReason::ProcessUnavailable(reason),
        }
    }
}
#[derive(Default)]
struct PaneWork {
    operations: usize,
    elapsed: Duration,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Allow,
    PaneExhausted,
    BatchExhausted,
}
struct WorkBudget {
    started: Instant,
    operations: usize,
    panes: BTreeMap<PaneId, PaneWork>,
    exhausted: BTreeSet<PaneId>,
}
impl WorkBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            operations: 0,
            panes: BTreeMap::new(),
            exhausted: BTreeSet::new(),
        }
    }
    fn admit(&mut self, pane: PaneId, operations: usize, elapsed: Duration) -> Admission {
        if operations == 0 {
            return Admission::Allow;
        }
        if self.operations + operations > BATCH_OPERATIONS || elapsed >= BATCH_IO_WINDOW {
            return Admission::BatchExhausted;
        }
        let work = self.panes.entry(pane).or_default();
        if self.exhausted.contains(&pane)
            || work.operations + operations > PANE_OPERATIONS
            || work.elapsed >= PANE_IO_SHARE
        {
            self.exhausted.insert(pane);
            return Admission::PaneExhausted;
        }
        work.operations += operations;
        self.operations += operations;
        Admission::Allow
    }
    fn charge(&mut self, pane: PaneId, elapsed: Duration) {
        self.panes.entry(pane).or_default().elapsed += elapsed;
    }
}

fn tmux_failure(error: &TmuxError) -> &'static str {
    match error {
        TmuxError::Spawn { .. } => "tmux spawn failed",
        TmuxError::ServerGone => "server gone",
        TmuxError::PaneGone { .. } => "pane gone",
        TmuxError::Failed { .. } => "tmux command rejected",
        TmuxError::Timeout { .. } => "tmux command timed out",
        TmuxError::NotInstalled => "tmux unavailable",
        TmuxError::ProtocolMismatch => "tmux protocol mismatch",
        TmuxError::Parse { .. } => "invalid tmux response",
    }
}
fn persistence_failure(error: &store::PersistenceError) -> &'static str {
    match error {
        store::PersistenceError::Tmux(error) => tmux_failure(error),
        store::PersistenceError::Random(_) => "token entropy unavailable",
        store::PersistenceError::Invalid(_) => "invalid persistence transaction",
        store::PersistenceError::Unsupported => "guarded writes unsupported",
    }
}

fn sample_time(origin: Instant) -> LeaseTime {
    LeaseTime {
        monotonic_ms: origin.elapsed().as_millis().min(u64::MAX as u128) as u64,
        epoch_ms: tma_runtime::now_ms(),
    }
}

struct Lease {
    persisted: CollectorLease,
    schedule: LeaseSchedule,
    target: Target,
}
impl Lease {
    fn live(&mut self, origin: Instant) -> bool {
        self.schedule.action(sample_time(origin)) != LeaseAction::Revoke
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    session: SessionId,
    pane: PaneId,
}
impl Target {
    fn address(self) -> String {
        format!("${}:.%{}", self.session.get(), self.pane.get())
    }
}

struct Committed {
    decision: ProgramDecision,
    owner: PaneOwner,
    commit: CommitMarker,
    acknowledged: Option<PersistedResult>,
}
fn same_selection(a: &ProgramDecision, b: &ProgramDecision) -> bool {
    a.pane == b.pane
        && a.manifest == b.manifest
        && a.process_chain == b.process_chain
        && a.boundary == b.boundary
        && a.selection.authority() == b.selection.authority()
        && a.selection.record_id() == b.selection.record_id()
        && a.selection.state() == b.selection.state()
        && a.selection.detail() == b.selection.detail()
        && a.selection.result() == b.selection.result()
        && a.selection.recovery_reset() == b.selection.recovery_reset()
        && a.observed_work_cycle == b.observed_work_cycle
}
fn facts<'a>(
    row: &'a PaneRecord,
    registration: Option<&'a Registration>,
) -> Option<ProgramPane<'a>> {
    Some(ProgramPane {
        pane: PaneId::parse(&row.pane_id).ok()?,
        pane_pid: row.pane_pid,
        current_command: &row.current_command,
        title: &row.title,
        ignored: identity::is_ignored(&row.options),
        registration,
    })
}
fn registration(row: &PaneRecord) -> Option<Registration> {
    Some(Registration {
        agent_name: row.options.get(opt::NAME)?.clone(),
        session: Some(row.options.get(opt::SESSION)?.clone()),
    })
}

#[derive(Default)]
struct Counters {
    batches: u64,
    process_samples: u64,
    publications: u64,
    duplicates: u64,
    releases: u64,
    errors: u64,
    lease_acquisitions: u64,
    lease_renewals: u64,
    lease_losses: u64,
    capability_losses: u64,
    notify_passes: u64,
    reports: u64,
    queries: u64,
    max_publication_ms: u128,
    max_notification_ms: u128,
    work_pane_losses: u64,
    work_batch_losses: u64,
    work_max_operations: usize,
    work_max_batch_ms: u128,
    delayed_process_samples: u64,
    lease_retirements: u64,
    lease_release_wins: u64,
    lease_retirement_uncertain: u64,
    cleanup_attempts: u64,
    cleanup_failures: u64,
}

pub(super) struct ProgramService {
    mode: ProgramStatusMode,
    runtime: ProgramRuntime,
    origin: Instant,
    next_process: Instant,
    lease: Option<Lease>,
    targets: BTreeMap<PaneId, Target>,
    committed: BTreeMap<PaneId, Committed>,
    counts: Counters,
    last_loss: Option<RecoveryReason>,
    cleanup_pending: bool,
    shutdown_cleanup_done: bool,
}
impl ProgramService {
    pub(super) fn new(mode: ProgramStatusMode, pool: &mut ControlPool) -> Option<Self> {
        pool.set_program_collection(mode != ProgramStatusMode::Off);
        (mode != ProgramStatusMode::Off).then(|| Self {
            mode,
            runtime: ProgramRuntime::new(),
            origin: Instant::now(),
            next_process: Instant::now(),
            lease: None,
            targets: BTreeMap::new(),
            committed: BTreeMap::new(),
            counts: Counters::default(),
            last_loss: None,
            cleanup_pending: false,
            shutdown_cleanup_done: false,
        })
    }
    pub(super) fn prepare(&self, pool: &mut ControlPool) {
        self.runtime.prepare_collection(pool);
    }
    pub(super) fn timeout(&self) -> Duration {
        if self.cleanup_pending {
            return Duration::from_millis(10);
        }
        self.next_process
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(10))
    }
    pub(super) fn due(&self, pool: &ControlPool) -> bool {
        self.cleanup_pending
            || Instant::now() >= self.next_process
            || pool
                .program_stats()
                .is_some_and(|s| s.queued_events != 0 || s.pending_gaps != 0)
    }
    fn error(&mut self, context: &str, error: store::PersistenceError) {
        self.counts.errors += 1;
        eprintln!("tma: program {context}: {}", persistence_failure(&error));
    }
    fn release(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        release: &ProgramRelease,
        work: &WorkBudget,
    ) -> bool {
        if release.owner.is_none() {
            return true;
        }
        self.last_loss = Some(release.reason);
        if self.lease.is_none() {
            return true;
        }
        let mut stopped = None;
        if !release_or_stop(
            tmux,
            &mut self.lease,
            &self.targets,
            &mut self.committed,
            &mut self.counts,
            release,
            &mut stopped,
            self.origin,
            work.started,
        ) {
            self.reset_retired(tmux, pool, stopped.unwrap_or(BatchStop::PersistenceLost));
            return false;
        }
        true
    }
    fn invalidate_pane(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        pane: PaneId,
        reason: RecoveryReason,
        work: &WorkBudget,
    ) -> bool {
        let batch = self.runtime.invalidate(pool, pane, reason);
        for release in batch.unstreamed_releases() {
            if !self.release(tmux, pool, release, work) {
                return false;
            }
        }
        control::mark_recheck(pool, &[format!("%{}", pane.get())], Instant::now());
        true
    }
    fn cleanup(&mut self, tmux: &Tmux) {
        if shutdown_requested() && self.shutdown_cleanup_done {
            return;
        }
        let started = Instant::now();
        for _ in 0..CLEANUP_OPERATIONS {
            if started.elapsed() >= CLEANUP_WINDOW {
                break;
            }
            let Some((pane, committed)) = self.committed.pop_first() else {
                break;
            };
            self.counts.cleanup_attempts += 1;
            if let Some(target) = self.targets.get(&pane) {
                if let Err(error) = revoke_exact(tmux, &target.address(), &committed.owner) {
                    self.counts.cleanup_failures += 1;
                    self.error("retired owner cleanup", error);
                }
            }
            if shutdown_requested() {
                self.shutdown_cleanup_done = true;
                break;
            }
        }
        if shutdown_requested() {
            self.shutdown_cleanup_done = true;
        }
        self.cleanup_pending = !self.committed.is_empty();
    }
    pub(super) fn stop(&mut self, tmux: &Tmux, pool: &mut ControlPool) {
        retire_lease(tmux, &mut self.lease, &mut self.counts);
        self.runtime.reset(pool);
        self.cleanup_pending = true;
        self.cleanup(tmux);
        if self.mode == ProgramStatusMode::Prefer {
            let due: Vec<_> = self
                .targets
                .keys()
                .map(|pane| format!("%{}", pane.get()))
                .collect();
            control::mark_recheck(pool, &due, Instant::now());
        }
    }
    fn continue_run(&mut self, tmux: &Tmux, pool: &mut ControlPool, work: &WorkBudget) -> bool {
        if self.cleanup_pending {
            return false;
        }
        if let Some(reason) = boundary_reason(&mut self.lease, self.origin, work.started) {
            self.counts.work_max_operations = self.counts.work_max_operations.max(work.operations);
            self.counts.work_max_batch_ms = self
                .counts
                .work_max_batch_ms
                .max(work.started.elapsed().as_millis());
            self.abort_batch(tmux, pool, reason);
            false
        } else {
            true
        }
    }
    fn reset_retired(&mut self, tmux: &Tmux, pool: &mut ControlPool, reason: BatchStop) {
        self.runtime.reset(pool);
        self.last_loss = Some(reason.reason());
        self.cleanup_pending = !self.committed.is_empty();
        if reason != BatchStop::Shutdown {
            self.cleanup(tmux);
        }
        let due: Vec<_> = self
            .targets
            .keys()
            .map(|pane| format!("%{}", pane.get()))
            .collect();
        control::mark_recheck(pool, &due, Instant::now());
        self.next_process = Instant::now() + PROCESS_CADENCE;
    }
    fn abort_batch(&mut self, tmux: &Tmux, pool: &mut ControlPool, reason: BatchStop) {
        if reason == BatchStop::WorkBudget {
            self.counts.work_batch_losses += 1;
        }
        retire_lease(tmux, &mut self.lease, &mut self.counts);
        self.reset_retired(tmux, pool, reason);
    }
    fn lose(&mut self, tmux: &Tmux, pool: &mut ControlPool, reason: RecoveryReason) {
        self.counts.lease_losses += 1;
        self.last_loss = Some(reason);
        let due: Vec<_> = self
            .targets
            .keys()
            .map(|pane| format!("%{}", pane.get()))
            .collect();
        self.stop(tmux, pool);
        self.last_loss = Some(reason);
        control::mark_recheck(pool, &due, Instant::now());
        self.next_process = Instant::now() + PROCESS_CADENCE;
    }
    fn maintain_lease(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        work: &WorkBudget,
    ) -> LeaseReadiness {
        if self.mode != ProgramStatusMode::Prefer {
            return LeaseReadiness::Ready;
        }
        if let Some(lease) = self.lease.as_mut() {
            match lease.schedule.action(sample_time(self.origin)) {
                LeaseAction::Wait => return LeaseReadiness::Ready,
                LeaseAction::Revoke => {
                    self.lose(tmux, pool, RecoveryReason::LeaseLost);
                    return LeaseReadiness::Stopped;
                }
                LeaseAction::Renew { .. } => {}
            }
        }
        let mut supported = None;
        let targets: Vec<_> = self.targets.values().copied().collect();
        for target in targets {
            if !self.continue_run(tmux, pool, work) {
                return LeaseReadiness::Stopped;
            }
            let capable = match store::probe_capability(tmux, &target.address()) {
                Ok(supported) => supported,
                Err(error) => {
                    self.error("capability probe", error);
                    false
                }
            };
            if !self.continue_run(tmux, pool, work) {
                return LeaseReadiness::Stopped;
            }
            if capable {
                supported = Some(target);
                break;
            }
        }
        let Some(target) = supported else {
            if self.lease.is_some() {
                self.lose(tmux, pool, RecoveryReason::CapabilityUnavailable);
                return LeaseReadiness::Stopped;
            }
            return LeaseReadiness::Unavailable;
        };
        let mut submission = None;
        let origin = self.origin;
        let mut clock = || {
            let at = sample_time(origin);
            submission.get_or_insert(at);
            at.epoch_ms
        };
        if let Some(lease) = self.lease.as_mut() {
            let outcome =
                store::renew_collector(tmux, &target.address(), &lease.persisted, &mut clock);
            let completion = sample_time(origin);
            if let (Ok(LeaseOutcome::Won(Some(persisted))), Some(submission)) =
                (&outcome, submission)
            {
                if lease.schedule.confirm(submission, completion) {
                    lease.persisted = persisted.clone();
                    lease.target = target;
                    self.counts.lease_renewals += 1;
                    return if self.continue_run(tmux, pool, work) {
                        LeaseReadiness::Ready
                    } else {
                        LeaseReadiness::Stopped
                    };
                }
            }
            if let Err(error) = outcome {
                self.error("lease renewal", error);
            }
            self.lose(tmux, pool, RecoveryReason::LeaseLost);
            LeaseReadiness::Stopped
        } else {
            let token = match store::fresh_token() {
                Ok(token) => token,
                Err(error) => {
                    self.error("token", error);
                    return LeaseReadiness::Stopped;
                }
            };
            let outcome = store::acquire_collector(tmux, &target.address(), &token, &mut clock);
            let completion = sample_time(origin);
            if let (Ok(LeaseOutcome::Won(Some(persisted))), Some(submission)) =
                (&outcome, submission)
            {
                let mut schedule = LeaseSchedule::committed(submission);
                if schedule.action(completion) != LeaseAction::Revoke {
                    self.lease = Some(Lease {
                        persisted: persisted.clone(),
                        schedule,
                        target,
                    });
                    self.counts.lease_acquisitions += 1;
                    // Reports retained while writes were unavailable cannot cross
                    // the fresh collector-token eligibility boundary.
                    self.runtime.reset(pool);
                    return if self.continue_run(tmux, pool, work) {
                        LeaseReadiness::Ready
                    } else {
                        LeaseReadiness::Stopped
                    };
                }
            }
            if let Err(error) = outcome {
                self.error("lease acquisition", error);
            }
            if let Err(error) = store::release_collector(tmux, &target.address(), &token) {
                self.error("failed acquisition release", error);
            }
            LeaseReadiness::Unavailable
        }
    }
    /// Run before quiet folding and after each bounded control/IPC dispatch. No
    /// control fd is read from a runtime callback.
    pub(super) fn run(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        manifests: &[LoadedManifest],
        notify: &mut NotifyState,
        subscribers: &mut Vec<UnixStream>,
        force: bool,
    ) -> Result<bool, TmuxError> {
        let mut inputs = RunInputs {
            tmux,
            pool,
            manifests,
            notify,
            subscribers,
        };
        self.collect(&mut inputs, force)
    }
    fn collect(&mut self, inputs: &mut RunInputs<'_>, force: bool) -> Result<bool, TmuxError> {
        let RunInputs {
            tmux,
            pool,
            manifests,
            notify,
            subscribers,
        } = inputs;
        let tmux = *tmux;
        let pool = &mut **pool;
        let manifests = *manifests;
        let notify = &mut **notify;
        let subscribers = &mut **subscribers;
        if shutdown_requested() {
            retire_lease(tmux, &mut self.lease, &mut self.counts);
            self.runtime.reset(pool);
            self.cleanup_pending = !self.committed.is_empty();
            return Ok(true);
        }
        if self.cleanup_pending {
            self.cleanup(tmux);
            if !self.cleanup_pending {
                self.next_process = Instant::now();
            }
            return Ok(true);
        }
        if !force && !self.due(pool) {
            return Ok(false);
        }
        if self
            .lease
            .as_mut()
            .is_some_and(|lease| !lease.live(self.origin))
        {
            self.lose(tmux, pool, RecoveryReason::LeaseLost);
            return Ok(true);
        }
        let mut work = WorkBudget::new();
        let rows = match tmux.list_panes() {
            Ok(rows) => rows,
            Err(error) => {
                self.lose(tmux, pool, RecoveryReason::LeaseLost);
                return Err(error);
            }
        };
        if !self.continue_run(tmux, pool, &work) {
            return Ok(true);
        }
        // Complete successful fleet membership proves these panes no longer
        // exist. Discard their exact private cleanup work before any early exit.
        let Ok(present) = rows
            .iter()
            .map(|row| PaneId::parse(&row.pane_id))
            .collect::<Result<BTreeSet<_>, _>>()
        else {
            self.counts.errors += 1;
            self.abort_batch(tmux, pool, BatchStop::PersistenceLost);
            return Ok(true);
        };
        let departed: BTreeSet<_> = self
            .targets
            .keys()
            .chain(self.committed.keys())
            .filter(|pane| !present.contains(pane))
            .copied()
            .collect();
        let mut removed = Default::default();
        for pane in &departed {
            self.runtime.remove_pane(pool, *pane, &mut removed);
            self.targets.remove(pane);
            self.committed.remove(pane);
        }
        for row in &rows {
            if let Ok(pane) = PaneId::parse(&row.pane_id) {
                if let Some(session) = pool.program_session(pane) {
                    self.targets.insert(pane, Target { session, pane });
                }
            }
        }
        if let Some(lease) = self.lease.as_mut() {
            if self.targets.get(&lease.target.pane) != Some(&lease.target) {
                if let Some(target) = self.targets.values().next() {
                    // Retirement is a single exact-token mutation, safe under
                    // option hooks too. Renewal still probes its chosen target.
                    lease.target = *target;
                }
            }
        }
        let periodic = Instant::now() >= self.next_process;
        if periodic {
            self.next_process = Instant::now() + PROCESS_CADENCE;
        }
        if (periodic || self.lease.is_some())
            && matches!(
                self.maintain_lease(tmux, pool, &work),
                LeaseReadiness::Stopped
            )
        {
            return Ok(true);
        }
        if self.cleanup_pending {
            return Ok(true);
        }
        let registrations: Vec<_> = rows.iter().map(registration).collect();
        let panes: Vec<_> = rows
            .iter()
            .zip(&registrations)
            .filter_map(|(row, registration)| facts(row, registration.as_ref()))
            .collect();
        let mut truth: BTreeMap<PaneId, store::PaneRead> = rows
            .iter()
            .filter_map(|row| {
                Some((
                    PaneId::parse(&row.pane_id).ok()?,
                    store::PaneRead::from_options(row.pane_id.clone(), row.options.clone()),
                ))
            })
            .collect();
        let mut changed = !departed.is_empty();
        changed |= match self.maintain_truth(
            &mut RunInputs {
                tmux,
                pool,
                manifests,
                notify,
                subscribers,
            },
            &mut truth,
            &work,
            periodic,
        ) {
            PhaseOutcome::Continue(changed) => changed,
            PhaseOutcome::Stop => return Ok(true),
        };
        if !self.continue_run(tmux, pool, &work) {
            return Ok(true);
        }
        let outcome = {
            let Self {
                runtime,
                lease,
                committed,
                targets,
                counts,
                origin,
                mode,
                last_loss,
                ..
            } = self;
            counts.batches += 1;
            let mut effects = effects::BatchEffects {
                tmux,
                manifests,
                notify,
                subscribers,
                panes: &panes,
                lease,
                committed,
                targets,
                counts,
                origin: *origin,
                mode: *mode,
                last_loss,
                truth: &mut truth,
                work: &mut work,
                batch_stop: None,
                refreshed_processes: None,
                checked_at: None,
                releases_due: Vec::new(),
                changed: false,
            };
            let mut samples = 0;
            let batch = runtime.process_batch_with(
                pool,
                &panes,
                manifests,
                &mut || {
                    samples += 1;
                    program_processes()
                },
                tma_runtime::now_ms(),
                |runtime, pool, step| effects.step(runtime, pool, step),
            );
            effects.counts.process_samples += samples;
            if effects.batch_stop.is_none() {
                effects.tail(runtime, pool, batch.unstreamed_releases());
            }
            effects.finish()
        };
        changed |= outcome.changed;
        if let Some(stopped) = outcome.stopped {
            self.reset_retired(tmux, pool, stopped);
            return Ok(true);
        }
        if !self.continue_run(tmux, pool, &work) {
            return Ok(true);
        }
        let releases_due = outcome.releases_due;
        control::mark_recheck(pool, &releases_due, Instant::now());
        match self.reconcile_acks(
            &mut RunInputs {
                tmux,
                pool,
                manifests,
                notify,
                subscribers,
            },
            &truth,
            &mut work,
            periodic,
        )? {
            PhaseOutcome::Continue(ack_changed) => changed |= ack_changed,
            PhaseOutcome::Stop => return Ok(true),
        }
        if !self.continue_run(tmux, pool, &work) {
            return Ok(true);
        }
        self.counts.work_max_operations = self.counts.work_max_operations.max(work.operations);
        self.counts.work_max_batch_ms = self
            .counts
            .work_max_batch_ms
            .max(work.started.elapsed().as_millis());
        let live: std::collections::BTreeSet<_> = panes.iter().map(|pane| pane.pane).collect();
        self.targets.retain(|pane, _| live.contains(pane));
        self.committed.retain(|pane, _| live.contains(pane));
        Ok(changed)
    }
    pub(super) fn notify(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        notify: &mut NotifyState,
    ) -> Result<(), TmuxError> {
        self.notify_with_deadline(tmux, pool, notify, None)
    }
    fn notify_with_deadline(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        notify: &mut NotifyState,
        deadline: Option<Instant>,
    ) -> Result<(), TmuxError> {
        let origin = self.origin;
        let result = notify.reconcile_checked(tmux, |read| {
            !read.program.program_origin
                || (!shutdown_requested()
                    && deadline.is_none_or(|deadline| Instant::now() < deadline)
                    && self.lease.as_mut().is_some_and(|lease| {
                        read.program
                            .owner
                            .as_ref()
                            .is_some_and(|owner| owner.collector == lease.persisted.token)
                            && lease.live(origin)
                    }))
        });
        if self.lease.as_mut().is_some_and(|lease| !lease.live(origin)) {
            self.lose(tmux, pool, RecoveryReason::LeaseLost);
        }
        result
    }
    pub(super) fn status(&self, pool: &ControlPool) -> String {
        let stats = pool.program_stats().unwrap_or_default();
        let mut body = format!("program_mode={}\nprogram_stores={}\nprogram_retained_bytes={}\nprogram_queued_bytes={}\nprogram_queued_events={}\nprogram_pending_gaps={}\nprogram_rejected={}\nprogram_overflows={}\nprogram_batches={}\nprogram_process_samples={}\nprogram_publications={}\nprogram_duplicates={}\nprogram_releases={}\nprogram_errors={}\nprogram_lease_acquisitions={}\nprogram_lease_renewals={}\nprogram_lease_losses={}\nprogram_capability_losses={}\nprogram_notify_passes={}\nprogram_lease={}\nprogram_last_loss={:?}\n", self.mode.word(), self.targets.len(), self.runtime.retained_bytes(), stats.pending_payload_bytes, stats.queued_events, stats.pending_gaps, stats.rejected_frames, stats.queue_overflows, self.counts.batches, self.counts.process_samples, self.counts.publications, self.counts.duplicates, self.counts.releases, self.counts.errors, self.counts.lease_acquisitions, self.counts.lease_renewals, self.counts.lease_losses, self.counts.capability_losses, self.counts.notify_passes, self.lease.as_ref().map_or("none", |lease| lease.persisted.token.as_str()), self.last_loss);
        body.push_str(&format!("program_reports={}\nprogram_queries={}\nprogram_max_publication_ms={}\nprogram_max_notification_ms={}\n", self.counts.reports, self.counts.queries, self.counts.max_publication_ms, self.counts.max_notification_ms));
        body.push_str(&format!("program_work_pane_losses={}\nprogram_work_batch_losses={}\nprogram_work_max_operations={}\nprogram_work_max_batch_ms={}\nprogram_delayed_process_samples={}\nprogram_lease_retirements={}\nprogram_cleanup_attempts={}\nprogram_cleanup_failures={}\nprogram_cleanup_remaining={}\n", self.counts.work_pane_losses, self.counts.work_batch_losses, self.counts.work_max_operations, self.counts.work_max_batch_ms, self.counts.delayed_process_samples, self.counts.lease_retirements, self.counts.cleanup_attempts, self.counts.cleanup_failures, if self.cleanup_pending { self.committed.len() } else { 0 }));
        body.push_str(&format!(
            "program_lease_release_wins={}\nprogram_lease_retirement_uncertain={}\n",
            self.counts.lease_release_wins, self.counts.lease_retirement_uncertain
        ));
        for pane in self.targets.keys().take(DIAGNOSTIC_PANES) {
            if let Some(diag) = self.runtime.diagnostics(*pane) {
                body.push_str(&format!("program_pane_%{}={:?},owner={:?},epoch={:?},last_report={:?},records={},bytes={},selection={:?}\n", pane.get(), diag.coverage, diag.owner, diag.boundary.map(|boundary| boundary.connection), diag.last_report, diag.records, diag.retained_bytes, self.runtime.decision(*pane).map(|decision| decision.selection.protocol_state())));
            }
        }
        body.push_str(&format!(
            "program_diagnostic_omitted={}\n",
            self.targets.len().saturating_sub(DIAGNOSTIC_PANES)
        ));
        body
    }
}
fn boundary_reason(
    lease: &mut Option<Lease>,
    origin: Instant,
    started: Instant,
) -> Option<BatchStop> {
    if shutdown_requested() {
        Some(BatchStop::Shutdown)
    } else if started.elapsed() >= BATCH_IO_WINDOW {
        Some(BatchStop::WorkBudget)
    } else if lease.as_mut().is_some_and(|lease| !lease.live(origin)) {
        Some(BatchStop::LeaseLost)
    } else {
        None
    }
}
fn check_io(
    tmux: &Tmux,
    lease: &mut Option<Lease>,
    counts: &mut Counters,
    stopped: &mut Option<BatchStop>,
    origin: Instant,
    started: Instant,
) -> bool {
    if stopped.is_some() {
        return false;
    }
    if let Some(reason) = boundary_reason(lease, origin, started) {
        stop_batch(tmux, lease, counts, stopped, reason);
        false
    } else {
        true
    }
}
fn stop_batch(
    tmux: &Tmux,
    lease: &mut Option<Lease>,
    counts: &mut Counters,
    stopped: &mut Option<BatchStop>,
    reason: BatchStop,
) {
    if stopped.is_none() {
        *stopped = Some(reason);
        if reason == BatchStop::WorkBudget {
            counts.work_batch_losses += 1;
        }
        if reason == BatchStop::LeaseLost {
            counts.lease_losses += 1;
        }
        retire_lease(tmux, lease, counts);
    }
}
fn refresh_processes(
    cache: &mut Option<Result<ProcessSnapshot, ProcessUnavailable>>,
    checked_at: &mut Option<Instant>,
    counts: &mut Counters,
) -> Result<(), ProcessUnavailable> {
    match checked_at {
        None => {
            *checked_at = Some(Instant::now());
        }
        Some(last) if last.elapsed() >= PROCESS_CADENCE => {
            counts.process_samples += 1;
            counts.delayed_process_samples += 1;
            *cache = Some(program_processes());
            *checked_at = Some(Instant::now());
        }
        Some(_) => {}
    }
    match cache {
        Some(Err(reason)) => Err(*reason),
        _ => Ok(()),
    }
}
fn retire_lease(tmux: &Tmux, lease: &mut Option<Lease>, counts: &mut Counters) {
    if let Some(lease) = lease.take() {
        counts.lease_retirements += 1;
        match store::release_collector(tmux, &lease.target.address(), &lease.persisted.token) {
            Ok(LeaseOutcome::Won(_)) => counts.lease_release_wins += 1,
            Ok(LeaseOutcome::Lost(current)) => {
                if current.is_some_and(|current| {
                    current.token == lease.persisted.token && current.live_at(tma_runtime::now_ms())
                }) {
                    counts.lease_retirement_uncertain += 1;
                    eprintln!("tma: program lease retirement unconfirmed; old token will not renew and remains bounded by expiry");
                }
            }
            Err(error) => {
                counts.errors += 1;
                counts.lease_retirement_uncertain += 1;
                eprintln!(
                    "tma: program lease retirement: {}; old token will not renew",
                    persistence_failure(&error)
                );
            }
        }
    }
}
fn duplicate(
    committed: Option<&Committed>,
    read: Option<&store::PaneRead>,
    decision: &ProgramDecision,
    history_invalidation: bool,
) -> bool {
    let (Some(previous), Some(read)) = (committed, read) else {
        return false;
    };
    same_selection(&previous.decision, decision)
        && read.owner().as_ref() == Some(&previous.owner)
        && read.program.commit.as_ref() == Some(&previous.commit)
        && matches!(read.projection(tma_runtime::now_ms()), Ok(ProjectionRead::Live { stamp, .. }) if stamp.state == decision.selection.state() && stamp.detail.as_ref() == decision.selection.detail())
        && read.program.acknowledged == previous.acknowledged
        && !history_invalidation
}
fn revoke_exact(
    tmux: &Tmux,
    target: &str,
    owner: &PaneOwner,
) -> Result<(), store::PersistenceError> {
    let current = store::revoke_program_readback(tmux, target, owner)?;
    if current.owner().as_ref() != Some(owner)
        || !matches!(current.projection(tma_runtime::now_ms()), Ok(ProjectionRead::Live { owner: current, .. }) if &current == owner)
    {
        Ok(())
    } else {
        Err(store::PersistenceError::Invalid(
            "exact owner revocation unconfirmed",
        ))
    }
}
#[allow(clippy::too_many_arguments)]
fn release_or_stop(
    tmux: &Tmux,
    lease: &mut Option<Lease>,
    targets: &BTreeMap<PaneId, Target>,
    committed: &mut BTreeMap<PaneId, Committed>,
    counts: &mut Counters,
    release: &ProgramRelease,
    stopped: &mut Option<BatchStop>,
    origin: Instant,
    started: Instant,
) -> bool {
    if release.owner.is_none() || lease.is_none() {
        return true;
    }
    if !check_io(tmux, lease, counts, stopped, origin, started) {
        return false;
    }
    counts.releases += 1;
    let outcome = match (lease.as_ref(), targets.get(&release.pane)) {
        (Some(current), Some(target)) => {
            let token = current.persisted.token.clone();
            let target = *target;
            match release_persisted_checked(tmux, target.session, &token, release, || {
                boundary_reason(lease, origin, started).is_none()
            }) {
                Ok(complete) => {
                    if !check_io(tmux, lease, counts, stopped, origin, started) {
                        return false;
                    }
                    if !complete {
                        stop_batch(tmux, lease, counts, stopped, BatchStop::PersistenceLost);
                        return false;
                    }
                    let read = store::read_pane(tmux, &target.address());
                    read.and_then(|read| {
                        if read.owner().is_some_and(|owner| {
                            owner.collector == token && Some(owner.episode) == release.owner
                        }) && matches!(
                            read.projection(tma_runtime::now_ms()),
                            Ok(ProjectionRead::Live { .. })
                        ) {
                            Err(store::PersistenceError::Invalid(
                                "owned release unconfirmed",
                            ))
                        } else {
                            Ok(())
                        }
                    })
                }
                Err(error) => Err(error),
            }
        }
        _ => Err(store::PersistenceError::Invalid(
            "missing exact release target",
        )),
    };
    if let Err(error) = outcome {
        counts.errors += 1;
        eprintln!("tma: program release: {}", persistence_failure(&error));
        stop_batch(tmux, lease, counts, stopped, BatchStop::PersistenceLost);
        return false;
    }
    if committed
        .get(&release.pane)
        .is_some_and(|current| Some(current.owner.episode) == release.owner)
    {
        committed.remove(&release.pane);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_work_is_pane_local_and_duplicates_spend_no_io_budget() {
        let mut work = WorkBudget::new();
        let hot = PaneId::new(1);
        let healthy = PaneId::new(2);
        for _ in 0..PANE_OPERATIONS {
            assert_eq!(work.admit(hot, 1, Duration::ZERO), Admission::Allow);
        }
        assert_eq!(work.admit(hot, 0, Duration::ZERO), Admission::Allow);
        assert_eq!(work.operations, PANE_OPERATIONS);
        assert_eq!(work.admit(hot, 1, Duration::ZERO), Admission::PaneExhausted);
        assert_eq!(work.admit(healthy, 1, Duration::ZERO), Admission::Allow);
        work.charge(healthy, PANE_IO_SHARE);
        assert_eq!(
            work.admit(healthy, 1, Duration::ZERO),
            Admission::PaneExhausted
        );
    }

    #[test]
    fn global_work_and_elapsed_limits_are_independent_of_pane_exhaustion() {
        let mut work = WorkBudget::new();
        for pane in 0..4 {
            for _ in 0..PANE_OPERATIONS {
                assert_eq!(
                    work.admit(PaneId::new(pane), 1, Duration::ZERO),
                    Admission::Allow
                );
            }
        }
        assert_eq!(work.operations, BATCH_OPERATIONS);
        assert_eq!(
            work.admit(PaneId::new(5), 1, Duration::ZERO),
            Admission::BatchExhausted
        );
        let mut work = WorkBudget::new();
        assert_eq!(
            work.admit(PaneId::new(1), 1, BATCH_IO_WINDOW),
            Admission::BatchExhausted
        );
        assert_eq!(
            work.admit(PaneId::new(1), 0, BATCH_IO_WINDOW),
            Admission::Allow
        );
        assert_eq!(work.operations, 0);
    }
}
