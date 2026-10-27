//! Daemon-owned protocol episodes and coverage. This module performs no tmux
//! writes except through the explicit publication adapter. Bounded events are validated between two coherent process samples;
//! only then does the pure store see them. Publication has a separate recheck.
//!
//! This orders our own observations, not the PTY's buffered bytes. Processes
//! sharing a pane can emit indistinguishable reports, including bytes already
//! in flight during a handoff. App aliases do not authenticate an emitter.

use std::collections::{BTreeMap, BTreeSet};
use tma_core::program_status::{
    AppId, EpisodeId, LiveEpisode, ObservationGap, ObservationSequence, ObservedEvent, PaneId,
    ProgramCommand, ProgramCommandRef, ProgramEvent, ProgramSelection, RecordStore,
    ResultDisposition, ResultIdentity, TerminalLifecycle, MAX_RECORDS,
};
use tma_core::snapshot::{ProcessIdentity, ProcessUnavailable};
use tma_tmux::control::{ControlPool, ObservationBoundary, ProgramDrain, ProgramStreamState};
use tma_tmux::tmux::{normalize_comm, ProcessSnapshot};

use crate::identity::{self, OutOfScope, PaneIdentity, Registration, TitleAnchor};
use crate::manifests::LoadedManifest;

pub mod publication;

pub const MAX_PROGRAM_BYTES: usize = 32 * 1024 * 1024;
const MAX_ANCESTRY: usize = 256;

/// The same production boundary/drain path is used by the daemon and controlled
/// tests. Callers must not read control clients concurrently with `process_batch`.
pub trait ProgramCollector {
    fn boundary(&mut self, pane: PaneId) -> Option<ObservationBoundary>;
    fn stream(&self, pane: PaneId) -> Option<ProgramStreamState>;
    fn invalidate(&mut self, pane: PaneId, reason: ObservationGap);
    fn pending_bytes(&self) -> usize;
    fn set_payload_budget(&mut self, available: usize);
    fn drain(&mut self) -> ProgramDrain;
}

impl ProgramCollector for ControlPool {
    fn boundary(&mut self, pane: PaneId) -> Option<ObservationBoundary> {
        self.program_boundary(pane)
    }
    fn stream(&self, pane: PaneId) -> Option<ProgramStreamState> {
        self.program_stream(pane)
    }
    fn invalidate(&mut self, pane: PaneId, reason: ObservationGap) {
        self.invalidate_program(pane, reason);
    }
    fn pending_bytes(&self) -> usize {
        self.pending_program_bytes()
    }
    fn set_payload_budget(&mut self, available: usize) {
        self.set_program_payload_budget(available);
    }
    fn drain(&mut self) -> ProgramDrain {
        self.drain_program()
    }
}

/// Fresh pane facts, independently of protocol text. Legacy PID-only title
/// anchors are deliberately absent from this interface.
pub struct ProgramPane<'a> {
    pub pane: PaneId,
    pub pane_pid: u32,
    pub current_command: &'a str,
    pub title: &'a str,
    pub ignored: bool,
    pub registration: Option<&'a Registration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryReason {
    BindingBoundary,
    ProcessUnavailable(ProcessUnavailable),
    Unrecognized,
    Ignored,
    Excluded(OutOfScope),
    NoAliases,
    NoStream,
    IdentityChanged,
    ProcessChangedDuringBatch,
    Observation(ObservationGap),
    ScopeLost,
    FullReset,
    ShellPrompt,
    AttachedProcessExit,
    PaneRemoved,
    Disabled,
    CapabilityUnavailable,
    LeaseLost,
    PersistenceLost,
    WorkBudgetExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageState {
    Unobserved,
    Recovering(RecoveryReason),
    Active,
}

/// B5 releases ownership and schedules fallback from these effects. No state
/// publication, lease acquisition, attention or action capability is implied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramRelease {
    pub pane: PaneId,
    pub owner: Option<EpisodeId>,
    pub reason: RecoveryReason,
    pub history_invalidation: Option<tma_core::persistence::HistoryInvalidation>,
}

#[derive(Default, Debug)]
pub struct ProgramBatch {
    pub releases: Vec<ProgramRelease>,
    pub applied_events: usize,
    callback_releases: usize,
}

impl ProgramBatch {
    /// Consume this tail after `process_batch_with` returns. Earlier releases
    /// were already delivered synchronously; replaying `releases` duplicates
    /// them. Failed samples/empty input have no validated callback context, so
    /// their releases are returned here. `process_batch` returns every release.
    pub fn unstreamed_releases(&self) -> &[ProgramRelease] {
        &self.releases[self.callback_releases..]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStepKind {
    Replace,
    Clear,
    Query,
    Lifecycle(TerminalLifecycle),
    Release,
}

/// A synchronous view after one ordered effect. Apply semantic result effects
/// before coalescing state writes. The decision is transient metadata, never a
/// second retained report queue. Do not read control clients from this callback.
pub struct ProgramStep<'a> {
    pub pane: PaneId,
    pub kind: ProgramStepKind,
    pub decision: Option<ProgramDecision>,
    pub releases: &'a [ProgramRelease],
    pub history_invalidation: Option<tma_core::persistence::HistoryInvalidation>,
    pub checked_processes: &'a ProcessSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgramRecheckError {
    StaleDecision,
    Revoked(ProgramRelease),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Recognized {
    manifest: String,
    apps: Vec<AppId>,
    // Root first, independently identified agent last. Every intermediate PID
    // birth matters, since a reused parent could otherwise forge ancestry.
    chain: Vec<ProcessIdentity>,
}

#[derive(Clone, Debug)]
struct Binding {
    identity: Recognized,
    owner: EpisodeId,
    boundary: ObservationBoundary,
    sequence: ObservationSequence,
    cycle_floor: tma_core::program_status::WorkCycle,
}

/// Retained across a gap only for the exact same process ancestry. B5 must
/// independently match its persisted marker before reconciling a fresh result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedProgramResult {
    pub process: ProcessIdentity,
    pub selection: ProgramSelection,
    pub acknowledged: bool,
}

struct PaneProgram {
    store: RecordStore,
    binding: Option<Binding>,
    coverage: CoverageState,
    last_report: Option<u64>,
    presented: Option<PresentedProgramResult>,
    presented_chain: Vec<ProcessIdentity>,
}

impl Default for PaneProgram {
    fn default() -> Self {
        Self {
            store: RecordStore::new(),
            binding: None,
            coverage: CoverageState::Unobserved,
            last_report: None,
            presented: None,
            presented_chain: Vec::new(),
        }
    }
}

/// Immutable publication input. It carries process/stream/result identities so
/// a later writer can reject stale work and reconcile persisted result markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramDecision {
    pub pane: PaneId,
    pub manifest: String,
    pub process_chain: Vec<ProcessIdentity>,
    pub boundary: ObservationBoundary,
    pub selection: ProgramSelection,
    pub previous_presentation: Option<PresentedProgramResult>,
    pub observed_work_cycle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramDiagnostics {
    pub coverage: CoverageState,
    pub owner: Option<EpisodeId>,
    pub process: Option<ProcessIdentity>,
    pub boundary: Option<ObservationBoundary>,
    pub last_report: Option<u64>,
    pub retained_bytes: usize,
    pub records: usize,
}

#[derive(Default)]
pub struct ProgramRuntime {
    panes: BTreeMap<PaneId, PaneProgram>,
    next_owner: u64,
    retained_bytes: usize,
}

fn next_owner(counter: &mut u64) -> EpisodeId {
    *counter = counter
        .checked_add(1)
        .expect("program episode counter exhausted");
    EpisodeId::new(*counter)
}

fn ancestry(
    snapshot: &ProcessSnapshot,
    root: u32,
    agent: u32,
) -> Result<Vec<ProcessIdentity>, RecoveryReason> {
    let mut chain = Vec::new();
    let mut pid = agent;
    loop {
        let identity = snapshot
            .identity(pid)
            .map_err(RecoveryReason::ProcessUnavailable)?;
        if chain.contains(&identity) || chain.len() >= MAX_ANCESTRY {
            return Err(RecoveryReason::Unrecognized);
        }
        chain.push(identity);
        if pid == root {
            chain.reverse();
            return Ok(chain);
        }
        pid = snapshot
            .processes()
            .iter()
            .find(|process| process.pid == pid)
            .ok_or(RecoveryReason::Unrecognized)?
            .ppid;
    }
}

fn recognize(
    pane: &ProgramPane<'_>,
    snapshot: &ProcessSnapshot,
    manifests: &[LoadedManifest],
    prior: Option<&Binding>,
) -> Result<Recognized, RecoveryReason> {
    if pane.ignored {
        return Err(RecoveryReason::Ignored);
    }
    // Repeat exclusions before identification, including registered carve-outs.
    if let Some(scope) = identity::program_out_of_scope(pane.current_command) {
        return Err(RecoveryReason::Excluded(scope));
    }
    snapshot
        .identity(pane.pane_pid)
        .map_err(RecoveryReason::ProcessUnavailable)?;
    let anchor =
        prior
            .filter(|binding| {
                binding
                    .identity
                    .chain
                    .first()
                    .is_some_and(|root| root.pid == pane.pane_pid)
                    && binding.identity.chain.iter().all(|identity| {
                        snapshot.identity(identity.pid).ok().as_ref() == Some(identity)
                    })
            })
            .and_then(|binding| {
                binding.identity.chain.last().map(|agent| TitleAnchor {
                    agent: binding.identity.manifest.as_str(),
                    pid: agent.pid,
                })
            });
    let identified = match identity::identify(
        pane.pane_pid,
        normalize_comm(pane.current_command),
        pane.title,
        snapshot.processes(),
        manifests,
        anchor,
        pane.registration,
    ) {
        PaneIdentity::Agent(agent) if agent.agent_pid > 0 && agent.behind.is_none() => agent,
        _ => return Err(RecoveryReason::Unrecognized),
    };
    let apps = identified.manifest.program_status_apps();
    if apps.is_empty() {
        return Err(RecoveryReason::NoAliases);
    }
    Ok(Recognized {
        manifest: identified.manifest.name.clone(),
        apps,
        chain: ancestry(snapshot, pane.pane_pid, identified.agent_pid)?,
    })
}

fn payload_bytes(event: &ProgramEvent) -> usize {
    match event {
        ProgramEvent::Command(ProgramCommand::Replace(record)) => record_bytes(record.as_ref()),
        ProgramEvent::Command(ProgramCommand::Clear(id)) => id.as_ref().as_str().len(),
        _ => 0,
    }
}

fn record_bytes(record: tma_core::program_status::RecordRef<'_>) -> usize {
    record.id.as_str().len()
        + record.app.map_or(0, |app| app.as_str().len())
        + record.title().map_or(0, |text| text.as_str().len())
        + record.msg.map_or(0, |text| text.as_str().len())
}

fn stream_loss(
    collector: &impl ProgramCollector,
    pane: PaneId,
    connection: tma_core::program_status::ConnectionEpoch,
) -> Option<RecoveryReason> {
    match collector.stream(pane) {
        Some(current) if current.connection == connection => None,
        Some(current) => Some(RecoveryReason::Observation(
            current.pending_gap.unwrap_or(ObservationGap::OwnerChanged),
        )),
        None => Some(RecoveryReason::NoStream),
    }
}

fn replacement_bytes(store: &RecordStore, command: ProgramCommandRef<'_>) -> usize {
    let old = store.retained_bytes();
    match command {
        ProgramCommandRef::Replace(record) => {
            let replaced = store
                .records()
                .iter()
                .find(|entry| entry.record().id.as_ref() == record.id)
                .or_else(|| {
                    (store.records().len() == MAX_RECORDS).then(|| {
                        store
                            .records()
                            .iter()
                            .min_by_key(|entry| entry.updated())
                            .unwrap()
                    })
                });
            old - replaced.map_or(0, |entry| record_bytes(entry.record().as_ref()))
                + record_bytes(record)
                + store.recovery_reservation(command)
                - store.recovery_retained_bytes()
        }
        ProgramCommandRef::Clear(id) => {
            store
                .records()
                .iter()
                .filter(|entry| !id.contains(entry.record().id.as_ref()))
                .map(|entry| record_bytes(entry.record().as_ref()))
                .sum::<usize>()
                + store.recovery_reservation(command)
        }
        ProgramCommandRef::Query => old,
    }
}

impl ProgramRuntime {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Revoke a scheduler-discovered loss through the same process/stream fence.
    pub fn invalidate(
        &mut self,
        collector: &mut impl ProgramCollector,
        pane: PaneId,
        reason: RecoveryReason,
    ) -> ProgramBatch {
        let mut batch = ProgramBatch::default();
        self.revoke(pane, reason, false, &mut batch);
        collector.invalidate(
            pane,
            if reason == RecoveryReason::WorkBudgetExceeded {
                ObservationGap::WorkBudgetExceeded
            } else {
                ObservationGap::OwnerChanged
            },
        );
        self.prepare_collection(collector);
        batch
    }

    /// Must precede every control read. The collector checks this remaining
    /// allowance as bytes arrive; it retains its own smaller queue caps too.
    pub fn prepare_collection(&self, collector: &mut impl ProgramCollector) {
        collector.set_payload_budget(
            MAX_PROGRAM_BYTES
                .checked_sub(self.retained_bytes)
                .expect("retained program payload exceeds budget"),
        );
    }

    fn revoke(
        &mut self,
        pane: PaneId,
        reason: RecoveryReason,
        departure: bool,
        batch: &mut ProgramBatch,
    ) {
        let entry = self.panes.entry(pane).or_default();
        let owner = entry.binding.as_ref().map(|binding| binding.owner);
        let old = entry.store.retained_bytes();
        if reason == RecoveryReason::Observation(ObservationGap::BudgetExceeded) {
            entry.store = RecordStore::new();
        } else if departure {
            entry
                .store
                .lifecycle(TerminalLifecycle::AttachedProcessExit);
        } else {
            entry.store.invalidate_episode();
        }
        self.retained_bytes = self.retained_bytes - old + entry.store.retained_bytes();
        entry.binding = None;
        entry.coverage = CoverageState::Recovering(reason);
        if owner.is_some() {
            batch.releases.push(ProgramRelease {
                history_invalidation: None,
                pane,
                owner,
                reason,
            });
        }
    }

    /// Samples exactly twice for a nonempty batch, never once per report.
    /// A fleet enumeration failure is an explicit loss, not an empty success.
    pub fn process_batch(
        &mut self,
        collector: &mut impl ProgramCollector,
        panes: &[ProgramPane<'_>],
        manifests: &[LoadedManifest],
        sample: &mut impl FnMut() -> Result<ProcessSnapshot, ProcessUnavailable>,
        at: u64,
    ) -> ProgramBatch {
        let mut batch =
            self.process_batch_with(collector, panes, manifests, sample, at, |_, _, _| {});
        batch.callback_releases = 0;
        batch
    }

    /// Ordered synchronous consumer path for B5/B6. The callback may reconcile,
    /// acknowledge or mark a successfully guarded presentation before the next
    /// event. Immediate rechecks use `checked_processes`; deferred publication
    /// requires newly sampled process and pane facts.
    pub fn process_batch_with<C: ProgramCollector>(
        &mut self,
        collector: &mut C,
        panes: &[ProgramPane<'_>],
        manifests: &[LoadedManifest],
        sample: &mut impl FnMut() -> Result<ProcessSnapshot, ProcessUnavailable>,
        at: u64,
        mut on_step: impl FnMut(&mut Self, &mut C, ProgramStep<'_>),
    ) -> ProgramBatch {
        let mut batch = ProgramBatch::default();
        let live: BTreeSet<_> = panes.iter().map(|pane| pane.pane).collect();
        let removed: Vec<_> = self
            .panes
            .keys()
            .filter(|pane| !live.contains(pane))
            .copied()
            .collect();
        for pane in removed {
            self.remove_pane(collector, pane, &mut batch);
        }
        if panes.is_empty() {
            self.prepare_collection(collector);
            return batch;
        }
        let before = match sample() {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                for pane in panes {
                    self.revoke(
                        pane.pane,
                        RecoveryReason::ProcessUnavailable(reason),
                        false,
                        &mut batch,
                    );
                    collector.invalidate(pane.pane, ObservationGap::OwnerChanged);
                }
                // Publication is unavailable, but a pending budget eviction
                // must still release history before this loss is consumed.
                for invalidation in collector.drain().invalidations {
                    self.apply_gap(invalidation, &mut batch);
                }
                self.prepare_collection(collector);
                return batch;
            }
        };
        for pane in panes {
            let prior = self
                .panes
                .get(&pane.pane)
                .and_then(|entry| entry.binding.as_ref());
            match recognize(pane, &before, manifests, prior) {
                Ok(identity) => {
                    if prior.is_some_and(|binding| binding.identity == identity) {
                        continue;
                    }
                    let owner = prior.map(|binding| binding.owner);
                    if owner.is_some() {
                        batch.releases.push(ProgramRelease {
                            history_invalidation: None,
                            pane: pane.pane,
                            owner,
                            reason: RecoveryReason::IdentityChanged,
                        });
                    }
                    let entry = self.panes.entry(pane.pane).or_default();
                    let prior_bytes = entry.store.retained_bytes();
                    entry.store.invalidate_episode();
                    self.retained_bytes =
                        self.retained_bytes - prior_bytes + entry.store.retained_bytes();
                    if entry.presented_chain != identity.chain {
                        entry.presented = None;
                        entry.presented_chain.clear();
                    }
                    entry.binding = collector.boundary(pane.pane).map(|boundary| Binding {
                        identity,
                        owner: next_owner(&mut self.next_owner),
                        sequence: boundary.sequence_floor,
                        cycle_floor: entry.store.work_cycle(),
                        boundary,
                    });
                    entry.coverage = CoverageState::Recovering(if entry.binding.is_some() {
                        RecoveryReason::BindingBoundary
                    } else {
                        RecoveryReason::NoStream
                    });
                }
                Err(reason) => {
                    self.revoke(pane.pane, reason, true, &mut batch);
                    collector.invalidate(pane.pane, ObservationGap::OwnerChanged);
                }
            }
        }
        self.prepare_collection(collector);
        let drain = collector.drain();
        if drain.all_invalidated {
            for pane in panes {
                self.revoke(pane.pane, RecoveryReason::ScopeLost, false, &mut batch);
                collector.invalidate(pane.pane, ObservationGap::QueueOverflow);
            }
        }
        for invalidation in drain.invalidations {
            self.apply_gap(invalidation, &mut batch);
        }
        // Validate the ordered bounded handoff before the second fleet sample.
        // No store mutation or outward publication occurs in this phase.
        let mut accepted = Vec::with_capacity(drain.observations.len());
        for observed in drain.observations {
            let Some(binding) = self
                .panes
                .get(&observed.pane)
                .and_then(|entry| entry.binding.as_ref())
            else {
                continue;
            };
            if observed.connection == binding.boundary.connection
                && observed.sequence > binding.boundary.sequence_floor
            {
                accepted.push(observed);
            }
        }
        let after = sample();
        for pane in panes {
            let Some(binding) = self
                .panes
                .get(&pane.pane)
                .and_then(|entry| entry.binding.as_ref())
            else {
                continue;
            };
            let agrees = after
                .as_ref()
                .ok()
                .and_then(|snapshot| recognize(pane, snapshot, manifests, Some(binding)).ok())
                .is_some_and(|identity| identity == binding.identity);
            if !agrees {
                self.revoke(
                    pane.pane,
                    RecoveryReason::ProcessChangedDuringBatch,
                    true,
                    &mut batch,
                );
                collector.invalidate(pane.pane, ObservationGap::OwnerChanged);
            }
        }
        let Ok(after) = after else {
            self.prepare_collection(collector);
            return batch;
        };
        // Boundary/gap releases precede every later event, as in B3's drain.
        let initial_releases = batch.releases.clone();
        for release in &initial_releases {
            on_step(
                self,
                collector,
                ProgramStep {
                    history_invalidation: None,
                    pane: release.pane,
                    kind: ProgramStepKind::Release,
                    decision: None,
                    releases: std::slice::from_ref(release),
                    checked_processes: &after,
                },
            );
            batch.callback_releases += 1;
        }
        let mut remaining: usize = accepted
            .iter()
            .map(|event| payload_bytes(&event.event))
            .sum();
        for observed in accepted {
            remaining -= payload_bytes(&observed.event);
            let pane = observed.pane;
            let history_invalidation = match &observed.event {
                ProgramEvent::Command(ProgramCommand::Clear(id)) => Some(
                    tma_core::persistence::HistoryInvalidation::Subtree(id.clone()),
                ),
                ProgramEvent::Lifecycle(TerminalLifecycle::FullReset) => {
                    Some(tma_core::persistence::HistoryInvalidation::All)
                }
                _ => None,
            };
            let kind = match observed.event {
                ProgramEvent::Command(ProgramCommand::Replace(_)) => ProgramStepKind::Replace,
                ProgramEvent::Command(ProgramCommand::Clear(_)) => ProgramStepKind::Clear,
                ProgramEvent::Command(ProgramCommand::Query) => ProgramStepKind::Query,
                ProgramEvent::Lifecycle(event) => ProgramStepKind::Lifecycle(event),
                ProgramEvent::Gap(_) => ProgramStepKind::Release,
            };
            let releases = batch.releases.len();
            let applied = batch.applied_events;
            self.apply_observation(collector, observed, at, remaining, &mut batch);
            if applied != batch.applied_events || releases != batch.releases.len() {
                let decision = self.decision(pane);
                on_step(
                    self,
                    collector,
                    ProgramStep {
                        history_invalidation,
                        pane,
                        kind: if applied == batch.applied_events {
                            ProgramStepKind::Release
                        } else {
                            kind
                        },
                        decision,
                        releases: &batch.releases[releases..],
                        checked_processes: &after,
                    },
                );
                batch.callback_releases += batch.releases.len() - releases;
            }
        }
        self.prepare_collection(collector);
        batch
    }

    fn apply_gap(&mut self, observed: ObservedEvent, batch: &mut ProgramBatch) {
        let ProgramEvent::Gap(reason) = observed.event else {
            return;
        };
        let Some(entry) = self.panes.get_mut(&observed.pane) else {
            return;
        };
        if reason == ObservationGap::BudgetExceeded {
            // Eviction survives rebinding and unbound historical stores. A
            // boundary may already account for the epoch, but not these bytes.
            self.retained_bytes -= entry.store.retained_bytes();
            entry.store = RecordStore::new();
        }
        let Some(binding) = entry.binding.as_mut() else {
            return;
        };
        // program_boundary itself supplies this baseline gap. It is already a
        // fresh episode and must not discard its later observations a second time.
        if binding.boundary.connection == observed.connection
            && binding.boundary.sequence_floor == observed.sequence
        {
            return;
        }
        let prior_bytes = entry.store.retained_bytes();
        entry.store.invalidate_episode();
        self.retained_bytes = self.retained_bytes - prior_bytes + entry.store.retained_bytes();
        batch.releases.push(ProgramRelease {
            history_invalidation: None,
            pane: observed.pane,
            owner: Some(binding.owner),
            reason: RecoveryReason::Observation(reason),
        });
        binding.owner = next_owner(&mut self.next_owner);
        binding.cycle_floor = entry.store.work_cycle();
        binding.boundary = ObservationBoundary {
            connection: observed.connection,
            sequence_floor: observed.sequence,
        };
        binding.sequence = observed.sequence;
        entry.coverage = CoverageState::Recovering(RecoveryReason::Observation(reason));
    }

    fn apply_observation(
        &mut self,
        collector: &mut impl ProgramCollector,
        observed: ObservedEvent,
        at: u64,
        remaining: usize,
        batch: &mut ProgramBatch,
    ) {
        let Some(connection) = self
            .panes
            .get(&observed.pane)
            .and_then(|entry| entry.binding.as_ref())
            .map(|binding| binding.boundary.connection)
        else {
            return;
        };
        if let Some(reason) = stream_loss(collector, observed.pane, connection) {
            self.revoke(observed.pane, reason, false, batch);
            return;
        }
        let Some(entry) = self.panes.get_mut(&observed.pane) else {
            return;
        };
        let Some(binding) = entry.binding.as_mut() else {
            return;
        };
        if observed.connection != binding.boundary.connection
            || observed.sequence <= binding.sequence
        {
            return;
        }
        binding.sequence = observed.sequence;
        let old = entry.store.retained_bytes();
        match &observed.event {
            ProgramEvent::Command(command) => {
                let new = replacement_bytes(&entry.store, command.as_ref());
                let total = self.retained_bytes - old + new + remaining + collector.pending_bytes();
                if total > MAX_PROGRAM_BYTES {
                    let owner = Some(binding.owner);
                    entry.store = RecordStore::new();
                    self.retained_bytes -= old;
                    entry.binding = None;
                    entry.coverage = CoverageState::Recovering(RecoveryReason::Observation(
                        ObservationGap::BudgetExceeded,
                    ));
                    batch.releases.push(ProgramRelease {
                        history_invalidation: None,
                        pane: observed.pane,
                        owner,
                        reason: RecoveryReason::Observation(ObservationGap::BudgetExceeded),
                    });
                    collector.invalidate(observed.pane, ObservationGap::BudgetExceeded);
                    return;
                }
                entry.store.apply(
                    command.as_ref(),
                    at,
                    Some(LiveEpisode {
                        owner: binding.owner,
                        apps: &binding.identity.apps,
                    }),
                );
                if !matches!(command, ProgramCommand::Query) {
                    entry.last_report = Some(at);
                }
            }
            ProgramEvent::Lifecycle(event) => {
                entry.store.lifecycle(*event);
                if matches!(event, TerminalLifecycle::FullReset) {
                    entry.presented = None;
                    entry.presented_chain.clear();
                }
                let reason = match event {
                    TerminalLifecycle::FullReset => RecoveryReason::FullReset,
                    TerminalLifecycle::ShellPrompt => RecoveryReason::ShellPrompt,
                    TerminalLifecycle::AttachedProcessExit => RecoveryReason::AttachedProcessExit,
                };
                batch.releases.push(ProgramRelease {
                    history_invalidation: matches!(event, TerminalLifecycle::FullReset)
                        .then_some(tma_core::persistence::HistoryInvalidation::All),
                    pane: observed.pane,
                    owner: Some(binding.owner),
                    reason,
                });
                entry.coverage = CoverageState::Recovering(reason);
                binding.owner = next_owner(&mut self.next_owner);
                binding.cycle_floor = entry.store.work_cycle();
                binding.boundary.sequence_floor = observed.sequence;
                if matches!(event, TerminalLifecycle::AttachedProcessExit) {
                    entry.binding = None;
                    collector.invalidate(observed.pane, ObservationGap::OwnerChanged);
                }
            }
            ProgramEvent::Gap(_) => return,
        }
        self.retained_bytes = self.retained_bytes - old + entry.store.retained_bytes();
        if let Some(binding) = &entry.binding {
            if entry
                .store
                .select(LiveEpisode {
                    owner: binding.owner,
                    apps: &binding.identity.apps,
                })
                .is_some()
            {
                entry.coverage = CoverageState::Active;
            } else if matches!(entry.coverage, CoverageState::Active) {
                entry.coverage = CoverageState::Recovering(RecoveryReason::BindingBoundary);
                batch.releases.push(ProgramRelease {
                    history_invalidation: match &observed.event {
                        ProgramEvent::Command(ProgramCommand::Clear(id)) => Some(
                            tma_core::persistence::HistoryInvalidation::Subtree(id.clone()),
                        ),
                        _ => None,
                    },
                    pane: observed.pane,
                    owner: Some(binding.owner),
                    reason: RecoveryReason::BindingBoundary,
                });
            }
        }
        batch.applied_events += 1;
    }

    pub fn decision(&self, pane: PaneId) -> Option<ProgramDecision> {
        let entry = self.panes.get(&pane)?;
        if entry.coverage != CoverageState::Active {
            return None;
        }
        let binding = entry.binding.as_ref()?;
        let selection = entry.store.select(LiveEpisode {
            owner: binding.owner,
            apps: &binding.identity.apps,
        })?;
        Some(ProgramDecision {
            pane,
            manifest: binding.identity.manifest.clone(),
            process_chain: binding.identity.chain.clone(),
            boundary: binding.boundary,
            selection,
            previous_presentation: entry.presented.clone(),
            observed_work_cycle: entry.store.work_cycle() > binding.cycle_floor,
        })
    }

    /// Fresh process AND pane facts are required immediately before delayed
    /// publication. A failed recheck revokes coverage through the same collector.
    pub fn recheck(
        &mut self,
        collector: &mut impl ProgramCollector,
        decision: &ProgramDecision,
        pane: &ProgramPane<'_>,
        manifests: &[LoadedManifest],
        snapshot: &ProcessSnapshot,
    ) -> Result<(), ProgramRecheckError> {
        if self.decision(decision.pane).as_ref() != Some(decision) {
            return Err(ProgramRecheckError::StaleDecision);
        }
        let loss = stream_loss(collector, decision.pane, decision.boundary.connection);
        let valid = loss.is_none()
            && pane.pane == decision.pane
            && self
                .panes
                .get(&pane.pane)
                .and_then(|entry| entry.binding.as_ref())
                .is_some_and(|binding| {
                    recognize(pane, snapshot, manifests, Some(binding))
                        .ok()
                        .as_ref()
                        == Some(&binding.identity)
                });
        if valid {
            return Ok(());
        }
        let mut batch = ProgramBatch::default();
        let reason = loss.unwrap_or(RecoveryReason::IdentityChanged);
        self.revoke(decision.pane, reason, loss.is_none(), &mut batch);
        if loss.is_none() {
            collector.invalidate(decision.pane, ObservationGap::OwnerChanged);
        }
        self.prepare_collection(collector);
        Err(ProgramRecheckError::Revoked(ProgramRelease {
            history_invalidation: None,
            pane: decision.pane,
            owner: Some(decision.selection.authority().owner()),
            reason,
        }))
    }

    /// Only call after guarded publication wins. This has no persistence effect.
    pub fn mark_presented(&mut self, decision: &ProgramDecision) -> bool {
        self.record_presentation(decision, None)
    }

    /// B5 supplies an independently matched persisted result disposition before
    /// it raises attention or advances a turn after recovery.
    pub fn reconcile_result(
        &mut self,
        decision: &ProgramDecision,
        disposition: ResultDisposition,
    ) -> bool {
        self.record_presentation(decision, Some(disposition))
    }

    fn record_presentation(
        &mut self,
        decision: &ProgramDecision,
        disposition: Option<ResultDisposition>,
    ) -> bool {
        let Some(entry) = self.panes.get_mut(&decision.pane) else {
            return false;
        };
        let Some(binding) = &entry.binding else {
            return false;
        };
        if decision.boundary != binding.boundary || decision.process_chain != binding.identity.chain
        {
            return false;
        }
        let live = LiveEpisode {
            owner: binding.owner,
            apps: &binding.identity.apps,
        };
        let won = match disposition {
            Some(disposition) => {
                entry
                    .store
                    .reconcile_result(&decision.selection, live, disposition)
            }
            None => entry.store.mark_presented(&decision.selection, live),
        };
        if won {
            entry.presented = Some(PresentedProgramResult {
                process: *binding.identity.chain.last().unwrap(),
                selection: decision.selection.clone(),
                acknowledged: disposition == Some(ResultDisposition::Acknowledged),
            });
            entry.presented_chain = binding.identity.chain.clone();
        }
        won
    }

    pub fn acknowledge(&mut self, pane: PaneId, result: &ResultIdentity) -> bool {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return false;
        };
        if !entry.store.acknowledge(result) {
            return false;
        }
        if let Some(presented) = entry.presented.as_mut().filter(|presented| {
            presented.selection.result().map(|result| &result.identity) == Some(result)
        }) {
            presented.acknowledged = true;
        }
        true
    }

    pub fn diagnostics(&self, pane: PaneId) -> Option<ProgramDiagnostics> {
        let entry = self.panes.get(&pane)?;
        Some(ProgramDiagnostics {
            coverage: entry.coverage,
            owner: entry.binding.as_ref().map(|binding| binding.owner),
            process: entry
                .binding
                .as_ref()
                .and_then(|binding| binding.identity.chain.last().copied()),
            boundary: entry.binding.as_ref().map(|binding| binding.boundary),
            last_report: entry.last_report,
            retained_bytes: entry.store.retained_bytes(),
            records: entry.store.records().len(),
        })
    }

    pub fn records(&self, pane: PaneId) -> Option<&RecordStore> {
        self.panes.get(&pane).map(|entry| &entry.store)
    }

    pub fn remove_pane(
        &mut self,
        collector: &mut impl ProgramCollector,
        pane: PaneId,
        batch: &mut ProgramBatch,
    ) {
        if let Some(entry) = self.panes.remove(&pane) {
            self.retained_bytes -= entry.store.retained_bytes();
            batch.releases.push(ProgramRelease {
                history_invalidation: None,
                pane,
                owner: entry.binding.map(|binding| binding.owner),
                reason: RecoveryReason::PaneRemoved,
            });
            collector.invalidate(pane, ObservationGap::OwnerChanged);
        }
    }

    /// Call before disabling/replacing collection or changing mode/aliases.
    /// The next enabled interval must establish new collector boundaries.
    pub fn reset(&mut self, collector: &mut impl ProgramCollector) -> ProgramBatch {
        let releases = self
            .panes
            .iter()
            .map(|(&pane, entry)| {
                collector.invalidate(pane, ObservationGap::OwnerChanged);
                ProgramRelease {
                    history_invalidation: None,
                    pane,
                    owner: entry.binding.as_ref().map(|binding| binding.owner),
                    reason: RecoveryReason::Disabled,
                }
            })
            .collect();
        self.panes.clear();
        self.retained_bytes = 0;
        collector.drain();
        self.prepare_collection(collector);
        ProgramBatch {
            releases,
            applied_events: 0,
            callback_releases: 0,
        }
    }
}

#[cfg(test)]
mod tests;
