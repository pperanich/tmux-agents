//! Typed projection and presentation effects, independent of public state equality.

use super::store::{EpisodeId, LiveEpisode, RecordStore, StoredRecord};
use super::{BlockedKind, ProgramStatus, RecordId};
use crate::state::{AgentState, Detail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultKind {
    Completion,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResultGeneration(u64);
impl ResultGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkCycle(u64);
impl WorkCycle {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime has independently matched an existing persisted result. This is not
/// inferred from absence of attention or from arbitrary protocol message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultDisposition {
    Presented,
    Acknowledged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultIdentity {
    pub owner: EpisodeId,
    pub record: RecordId,
    pub generation: ResultGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramResult {
    pub identity: ResultIdentity,
    pub kind: ResultKind,
    pub cycle: WorkCycle,
    /// Needs guarded presentation and a turn-marker advance, even for idle -> idle.
    pub pending: bool,
}

/// Proof carried separately from the compatibility `hook` provenance. Only a live
/// selection can construct this; runtime validates the supplied episode facts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramAuthority {
    owner: EpisodeId,
}
impl ProgramAuthority {
    pub const fn owner(self) -> EpisodeId {
        self.owner
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramSelection {
    pub(super) authority: ProgramAuthority,
    pub(super) record_id: RecordId,
    pub(super) updated: u64,
    pub(super) at: u64,
    pub(super) protocol_state: ProgramStatus,
    pub(super) state: AgentState,
    pub(super) detail: Option<Detail>,
    pub(super) result: Option<ProgramResult>,
    pub(super) recovery_reset: bool,
}
impl ProgramSelection {
    /// This observer saw a reset of this record/subtree. Aggregate cycles remain on ProgramResult.
    pub const fn recovery_reset(&self) -> bool {
        self.recovery_reset
    }

    pub const fn authority(&self) -> ProgramAuthority {
        self.authority
    }
    pub fn record_id(&self) -> &RecordId {
        &self.record_id
    }
    pub const fn updated(&self) -> u64 {
        self.updated
    }
    pub const fn at(&self) -> u64 {
        self.at
    }
    pub const fn protocol_state(&self) -> ProgramStatus {
        self.protocol_state
    }
    pub const fn state(&self) -> AgentState {
        self.state
    }
    pub fn detail(&self) -> Option<&Detail> {
        self.detail.as_ref()
    }
    pub fn result(&self) -> Option<&ProgramResult> {
        self.result.as_ref()
    }
}

impl RecordStore {
    pub fn select(&self, live: LiveEpisode<'_>) -> Option<ProgramSelection> {
        let active = self.has_activity(live);
        let entry = self
            .records
            .iter()
            .filter(|entry| self.eligible(entry, live))
            .max_by(|left, right| {
                rank(left)
                    .cmp(&rank(right))
                    .then_with(|| left.updated.cmp(&right.updated))
                    .then_with(|| left.record.id.cmp(&right.record.id))
            })?;
        let fresh_result = entry
            .result
            .as_ref()
            .filter(|result| !result.acknowledged && !result.superseded);
        let result = if active {
            None
        } else {
            fresh_result
                .filter(|_| {
                    matches!(
                        entry.record.status,
                        ProgramStatus::Done | ProgramStatus::Error
                    )
                })
                .map(|result| ProgramResult {
                    identity: ResultIdentity {
                        owner: live.owner,
                        record: entry.record.id.clone(),
                        generation: result.generation,
                    },
                    kind: match entry.record.status {
                        ProgramStatus::Error => ResultKind::Failure,
                        _ => ResultKind::Completion,
                    },
                    cycle: result.cycle,
                    pending: !result.presented,
                })
        };
        let (state, detail) = match entry.record.status {
            ProgramStatus::Idle | ProgramStatus::Done => (AgentState::Idle, None),
            ProgramStatus::Working { .. } => (AgentState::Working, None),
            ProgramStatus::Blocked { kind, .. } => (
                AgentState::Blocked,
                kind.map(|kind| {
                    Detail::new(match kind {
                        BlockedKind::Permission => "permission",
                        BlockedKind::Question => "question",
                        BlockedKind::Auth => "auth",
                    })
                }),
            ),
            ProgramStatus::Error => (AgentState::Idle, fresh_result.map(|_| Detail::new("error"))),
        };
        Some(ProgramSelection {
            authority: ProgramAuthority { owner: live.owner },
            record_id: entry.record.id.clone(),
            updated: entry.updated,
            at: entry.at,
            protocol_state: entry.record.status,
            state,
            detail,
            result,
            recovery_reset: self.recovery_reset_for(entry.record.id.as_ref()),
        })
    }
}

fn rank(entry: &StoredRecord) -> u8 {
    match entry.record.status {
        ProgramStatus::Blocked { .. } => 4,
        ProgramStatus::Working { .. } => 3,
        ProgramStatus::Error
            if entry
                .result
                .as_ref()
                .is_some_and(|result| !result.acknowledged && !result.superseded) =>
        {
            2
        }
        ProgramStatus::Done
            if entry
                .result
                .as_ref()
                .is_some_and(|result| !result.acknowledged && !result.superseded) =>
        {
            1
        }
        ProgramStatus::Idle | ProgramStatus::Done | ProgramStatus::Error => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program_status::{AppId, AppIdRef, ProgramCommandRef, RecordIdRef, RecordRef};

    #[test]
    fn equal_ordinals_use_deterministic_record_id_order() {
        let aliases = [AppId::parse("pi").unwrap()];
        let live = LiveEpisode {
            owner: EpisodeId::new(1),
            apps: &aliases,
        };
        let mut store = RecordStore::new();
        for id in ["z", "a"] {
            let mut record = RecordRef::new(RecordIdRef::parse(id).unwrap(), ProgramStatus::Idle);
            record.app = Some(AppIdRef::parse("pi").unwrap());
            store.apply(ProgramCommandRef::Replace(record), 1, Some(live));
        }
        for entry in &mut store.records {
            entry.updated = 1;
        }
        assert_eq!(
            store.select(live).unwrap().record_id,
            RecordId::parse("z").unwrap()
        );
        store.records.reverse();
        assert_eq!(
            store.select(live).unwrap().record_id,
            RecordId::parse("z").unwrap()
        );
    }
}
