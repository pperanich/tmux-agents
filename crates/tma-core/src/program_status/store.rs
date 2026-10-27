//! Bounded terminal records and live-episode eligibility. No process or clock reads.

use super::project::{
    ProgramSelection, ResultDisposition, ResultGeneration, ResultIdentity, WorkCycle,
};
use super::{
    AppId, AppIdRef, ProgramCommandRef, ProgramStatus, Record, RecordIdRef, TerminalLifecycle,
    MAX_RECORDS,
};

/// Runtime supplies this only after process, stream, coverage and exclusion checks succeed.
/// A new binding must use a new value, including after a gap or mode change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpisodeId(u64);

impl EpisodeId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LiveEpisode<'a> {
    pub owner: EpisodeId,
    /// Aliases are immutable within an episode. Configuration changes invalidate it.
    pub apps: &'a [AppId],
}

impl LiveEpisode<'_> {
    pub(crate) fn matches(self, app: AppIdRef<'_>) -> bool {
        self.apps.iter().any(|alias| alias.as_ref() == app)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResultMemory {
    // Eligibility can be revoked temporarily without moving a result to another aggregate.
    pub owner: Option<EpisodeId>,
    pub generation: ResultGeneration,
    pub cycle: WorkCycle,
    pub presented: bool,
    pub acknowledged: bool,
    pub superseded: bool,
}

#[derive(Clone, Debug)]
pub struct StoredRecord {
    pub(crate) record: Record,
    pub(crate) updated: u64,
    pub(crate) at: u64,
    pub(crate) observed_owner: Option<EpisodeId>,
    pub(crate) owner: Option<EpisodeId>,
    pub(crate) historical: bool,
    pub(crate) generation: ResultGeneration,
    pub(crate) result: Option<ResultMemory>,
}

impl StoredRecord {
    pub fn record(&self) -> &Record {
        &self.record
    }
    pub const fn updated(&self) -> u64 {
        self.updated
    }
    pub const fn at(&self) -> u64 {
        self.at
    }
    pub const fn is_historical(&self) -> bool {
        self.historical
    }
}

/// Full replacement is the only record mutation. Ordinals, rather than wall time,
/// order updates and eviction. Rejected parser outcomes never enter this reducer.
#[derive(Clone, Debug, Default)]
pub struct RecordStore {
    pub(crate) records: Vec<StoredRecord>,
    ordinal: u64,
    cycle: WorkCycle,
    active_owner: Option<EpisodeId>,
    recovery_resets: Vec<(super::RecordId, bool)>,
    recovery_reset_all: bool,
}

impl RecordStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub const fn work_cycle(&self) -> WorkCycle {
        self.cycle
    }

    /// Match consumption history only inside this exact observer episode. A reset removes the
    /// record's result memory; an acknowledged result remains available for durable retention.
    pub fn retains_result(&self, identity: &ResultIdentity, cycle: WorkCycle) -> bool {
        self.records.iter().any(|entry| {
            entry.record.id == identity.record
                && entry.result.as_ref().is_some_and(|result| {
                    result.owner == Some(identity.owner)
                        && result.generation == identity.generation
                        && result.cycle == cycle
                        && !result.superseded
                })
        })
    }

    pub fn records(&self) -> &[StoredRecord] {
        &self.records
    }

    /// Payload bytes retained by the store, excluding allocation overhead.
    pub fn retained_bytes(&self) -> usize {
        self.records
            .iter()
            .map(|entry| {
                let record = entry.record.as_ref();
                record.id.as_str().len()
                    + record.app.map_or(0, |app| app.as_str().len())
                    + record.title().map_or(0, |text| text.as_str().len())
                    + record.msg.map_or(0, |text| text.as_str().len())
            })
            .sum::<usize>()
            + self.recovery_retained_bytes()
    }

    fn next_ordinal(&mut self) -> u64 {
        // Exhaustion requires more than 500 million years at 1000 updates/second.
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .expect("program update ordinal exhausted");
        self.ordinal
    }

    pub fn apply(
        &mut self,
        command: ProgramCommandRef<'_>,
        at: u64,
        episode: Option<LiveEpisode<'_>>,
    ) {
        if matches!(command, ProgramCommandRef::Query) {
            return;
        }
        let was_active = episode.is_some_and(|live| self.has_activity(live));
        let mut idle_reset = false;
        let mut dialog_restore = false;
        let mut reported_activity = false;
        match command {
            ProgramCommandRef::Replace(record) => {
                let ordinal = self.next_ordinal();
                let position = self
                    .records
                    .iter()
                    .position(|entry| entry.record.id.as_ref() == record.id);
                let previous = position.map(|index| self.records.remove(index));
                if self.records.len() == MAX_RECORDS {
                    let oldest = self
                        .records
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| entry.updated)
                        .map(|(index, _)| index)
                        .unwrap();
                    self.records.remove(oldest);
                }
                let observed_owner = episode.map(|live| live.owner);
                let same_lineage = previous
                    .as_ref()
                    .filter(|old| old.observed_owner == observed_owner && !old.historical);
                // A foreign replacement changes protocol data, not this owner's work generation.
                let report_eligible = episode.is_some_and(|live| {
                    record
                        .app
                        .or_else(|| self.effective_app(record.id))
                        .is_none_or(|app| live.matches(app))
                        && (record.app.is_some_and(|app| live.matches(app))
                            || same_lineage.is_some_and(|old| old.owner == Some(live.owner))
                            || self.has_anchor(record.id, live))
                });
                let reset = same_lineage.is_none_or(|old| {
                    report_eligible
                        && match (record.status, old.record.status) {
                            (ProgramStatus::Working { .. }, ProgramStatus::Working { .. })
                            | (ProgramStatus::Idle, ProgramStatus::Idle) => old.result.is_some(),
                            (ProgramStatus::Working { .. } | ProgramStatus::Idle, _) => true,
                            _ => false,
                        }
                });
                if report_eligible && matches!(record.status, ProgramStatus::Idle) {
                    self.note_recovery_reset(record.id, false);
                }
                idle_reset = report_eligible
                    && matches!(record.status, ProgramStatus::Idle)
                    && same_lineage.is_some_and(|old| old.result.is_some());
                dialog_restore = report_eligible
                    && matches!(record.status, ProgramStatus::Blocked { .. })
                    && same_lineage.is_some_and(|old| old.result.is_some());
                reported_activity = report_eligible
                    && (matches!(record.status, ProgramStatus::Working { .. })
                        || (matches!(record.status, ProgramStatus::Blocked { .. })
                            && !dialog_restore));
                let generation = if reset {
                    ResultGeneration::new(ordinal)
                } else {
                    same_lineage.unwrap().generation
                };
                let mut result = if reset {
                    None
                } else {
                    same_lineage.and_then(|old| old.result.clone())
                };
                if matches!(record.status, ProgramStatus::Done | ProgramStatus::Error)
                    && result.is_none()
                {
                    result = Some(ResultMemory {
                        owner: None,
                        generation,
                        cycle: self.cycle,
                        presented: false,
                        acknowledged: false,
                        superseded: false,
                    });
                }
                let owner = same_lineage.and_then(|old| old.owner);
                self.records.push(StoredRecord {
                    record: record.into_owned(),
                    updated: ordinal,
                    at,
                    observed_owner,
                    owner,
                    historical: false,
                    generation,
                    result,
                });
            }
            ProgramCommandRef::Clear(id) => {
                self.note_recovery_reset(id, true);
                self.records
                    .retain(|entry| !id.contains(entry.record.id.as_ref()));
            }
            ProgramCommandRef::Query => unreachable!(),
        }
        if let Some(live) = episode {
            self.reconcile_ownership(live);
            let is_active = self.has_activity(live);
            let changed_eligible = self
                .records
                .last()
                .is_some_and(|entry| self.eligible(entry, live));
            let starts_cycle = (is_active
                && ((!was_active && !dialog_restore)
                    || (self.active_owner != Some(live.owner)
                        && reported_activity
                        && changed_eligible)))
                || (idle_reset && !was_active && changed_eligible);
            if starts_cycle {
                self.cycle = WorkCycle::new(
                    self.cycle
                        .get()
                        .checked_add(1)
                        .expect("program work cycle exhausted"),
                );
            }
            for entry in &mut self.records {
                if let Some(result) = &mut entry.result {
                    if result.owner == Some(live.owner) && starts_cycle {
                        result.superseded = true;
                    } else if result.owner.is_none() && entry.owner == Some(live.owner) {
                        // Same-episode anonymous results can first become eligible alongside
                        // retained work. Join that aggregate instead of retiring them unseen.
                        result.owner = Some(live.owner);
                        result.cycle = self.cycle;
                    }
                }
            }
            // A restored result dialog is active for selection, but does not itself
            // open a work cycle. Actual new work during that dialog still can.
            if !is_active {
                self.active_owner = None;
            } else if starts_cycle {
                self.active_owner = Some(live.owner);
            }
        }
    }

    fn note_recovery_reset(&mut self, id: RecordIdRef<'_>, subtree: bool) {
        if self.recovery_reset_all
            || self.recovery_resets.iter().any(|(reset, wide)| {
                if *wide {
                    reset.as_ref().contains(id)
                } else {
                    !subtree && reset.as_ref() == id
                }
            })
        {
            return;
        }
        if subtree {
            self.recovery_resets
                .retain(|(reset, _)| !id.contains(reset.as_ref()));
        }
        if self.recovery_resets.len() == MAX_RECORDS || subtree && id.is_root() {
            self.recovery_resets.clear();
            self.recovery_reset_all = true;
        } else {
            self.recovery_resets.push((id.into_owned(), subtree));
        }
    }

    pub fn recovery_retained_bytes(&self) -> usize {
        self.recovery_resets
            .iter()
            .map(|(id, _)| id.as_ref().as_str().len())
            .sum()
    }

    /// Conservative pre-mutation reservation. Eligibility may discard an idle reset, and a
    /// subtree reset may remove older tombstones, so actual retention can only be smaller.
    pub fn recovery_reservation(&self, command: ProgramCommandRef<'_>) -> usize {
        let extra = match command {
            ProgramCommandRef::Replace(record) if matches!(record.status, ProgramStatus::Idle) => {
                record.id.as_str().len()
            }
            ProgramCommandRef::Clear(id) => id.as_str().len(),
            _ => 0,
        };
        self.recovery_retained_bytes() + extra
    }

    pub fn recovery_reset_for(&self, id: RecordIdRef<'_>) -> bool {
        self.recovery_reset_all
            || self.recovery_resets.iter().any(|(reset, wide)| {
                if *wide {
                    reset.as_ref().contains(id)
                } else {
                    reset.as_ref() == id
                }
            })
    }

    pub fn lifecycle(&mut self, event: TerminalLifecycle) {
        match event {
            TerminalLifecycle::FullReset => {
                self.records.clear();
                self.recovery_resets.clear();
                self.recovery_reset_all = true;
            }
            TerminalLifecycle::ShellPrompt | TerminalLifecycle::AttachedProcessExit => {
                self.recovery_resets.clear();
                self.recovery_reset_all = false;
                self.records.retain(|entry| {
                    matches!(
                        entry.record.status,
                        ProgramStatus::Done | ProgramStatus::Error
                    )
                });
                for entry in &mut self.records {
                    entry.historical = true;
                    entry.owner = None;
                    if let Some(result) = &mut entry.result {
                        result.owner = None;
                    }
                }
            }
        }
        self.active_owner = None;
    }

    /// Departure/gap retains terminal history, but cannot lend it to another episode.
    /// This is deliberately separate from terminal reset.
    pub fn invalidate_episode(&mut self) {
        self.recovery_resets.clear();
        self.recovery_reset_all = false;
        for entry in &mut self.records {
            entry.owner = None;
            entry.observed_owner = None;
            entry.historical = true;
            if let Some(result) = &mut entry.result {
                result.owner = None;
            }
        }
        self.active_owner = None;
    }

    /// Nearest explicit app, computed from the current records, never copied into a child.
    pub fn effective_app(&self, id: RecordIdRef<'_>) -> Option<AppIdRef<'_>> {
        self.records
            .iter()
            .filter(|entry| entry.record.id.as_ref().contains(id))
            .filter_map(|entry| {
                entry
                    .record
                    .app
                    .as_ref()
                    .map(|app| (entry.record.id.as_ref().as_str().len(), app.as_ref()))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, app)| app)
    }

    fn reconcile_ownership(&mut self, live: LiveEpisode<'_>) {
        // A matching explicit record is an independent lineage anchor. Anonymous reports
        // may follow an established same-record/ancestor lineage; a foreign ancestor revokes it.
        let owners: Vec<_> = self
            .records
            .iter()
            .map(|entry| {
                if entry.historical || entry.observed_owner != Some(live.owner) {
                    return None;
                }
                if self
                    .effective_app(entry.record.id.as_ref())
                    .is_some_and(|app| !live.matches(app))
                {
                    return None;
                }
                let anchored = self.has_anchor(entry.record.id.as_ref(), live);
                (anchored || entry.owner == Some(live.owner)).then_some(live.owner)
            })
            .collect();
        for (entry, owner) in self.records.iter_mut().zip(owners) {
            entry.owner = owner;
        }
    }

    fn has_anchor(&self, id: RecordIdRef<'_>, live: LiveEpisode<'_>) -> bool {
        self.records.iter().any(|ancestor| {
            ancestor.observed_owner == Some(live.owner)
                && !ancestor.historical
                && ancestor.record.id.as_ref().contains(id)
                && (ancestor.owner == Some(live.owner)
                    || ancestor
                        .record
                        .app
                        .as_ref()
                        .is_some_and(|app| live.matches(app.as_ref())))
        })
    }

    pub(crate) fn eligible(&self, entry: &StoredRecord, live: LiveEpisode<'_>) -> bool {
        !entry.historical
            && entry.owner == Some(live.owner)
            && self
                .effective_app(entry.record.id.as_ref())
                .is_none_or(|app| live.matches(app))
    }

    pub(crate) fn has_activity(&self, live: LiveEpisode<'_>) -> bool {
        self.records.iter().any(|entry| {
            self.eligible(entry, live)
                && matches!(
                    entry.record.status,
                    ProgramStatus::Working { .. } | ProgramStatus::Blocked { .. }
                )
        })
    }

    /// Call only after guarded publication succeeds. Returns false for stale projections.
    /// Other results in this settled aggregate become superseded, never acknowledged.
    pub fn mark_presented(&mut self, selection: &ProgramSelection, live: LiveEpisode<'_>) -> bool {
        let Some(effect) = selection.result.as_ref().filter(|result| result.pending) else {
            return false;
        };
        if live.owner != selection.authority().owner() {
            return false;
        }
        if self.has_activity(live) {
            return false;
        }
        let Some(current) = self.select(live) else {
            return false;
        };
        if current.record_id != selection.record_id
            || current.updated != selection.updated
            || current
                .result
                .as_ref()
                .filter(|result| result.pending)
                .map(|result| &result.identity)
                != Some(&effect.identity)
        {
            return false;
        }
        for entry in &mut self.records {
            if let Some(result) = &mut entry.result {
                if result.owner == Some(effect.identity.owner) {
                    if entry.record.id == effect.identity.record
                        && result.generation == effect.identity.generation
                    {
                        result.presented = true;
                    } else if result.cycle <= effect.cycle {
                        result.superseded = true;
                    }
                }
            }
        }
        true
    }

    /// An absent attention flag is not acknowledgement. Only a presented, exact result
    /// identity can be acknowledged, including when a new report arrived in the same wake.
    pub fn acknowledge(&mut self, identity: &ResultIdentity) -> bool {
        let Some(entry) = self.records.iter_mut().find(|entry| {
            entry.owner == Some(identity.owner) && entry.record.id == identity.record
        }) else {
            return false;
        };
        let Some(result) = entry
            .result
            .as_mut()
            .filter(|result| result.generation == identity.generation && result.presented)
        else {
            return false;
        };
        result.acknowledged = true;
        true
    }

    /// Reconcile a fresh selected result against independently validated persisted
    /// markers after recovery. No notification/turn effect should be committed first.
    /// Deferred results cannot pass the same settled-selection guard as presentation.
    pub fn reconcile_result(
        &mut self,
        selection: &ProgramSelection,
        live: LiveEpisode<'_>,
        disposition: ResultDisposition,
    ) -> bool {
        if !self.mark_presented(selection, live) {
            return false;
        }
        match disposition {
            ResultDisposition::Presented => true,
            ResultDisposition::Acknowledged => {
                self.acknowledge(&selection.result.as_ref().unwrap().identity)
            }
        }
    }
}
