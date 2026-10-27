//! Guarded publication, reconciliation and presentation ordering for the optional collector.
//! Scheduling and collector activation remain the caller's responsibility.
use super::{ProgramCollector, ProgramDecision, ProgramPane, ProgramRecheckError, ProgramRuntime};
use crate::manifests::LoadedManifest;
use tma_core::persistence::{self as model, PaneOwner, PersistedResult, ProcessChain, Token};
use tma_core::program_status::{ResultDisposition, SessionId};
use tma_core::render::{Guard, Publish};
use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, ReadResult};
use tma_tmux::persistence::{self as store, CommitOutcome, OptionEdit, PersistenceError};
use tma_tmux::tmux::{ProcessSnapshot, Tmux};

#[derive(Debug)]
pub enum ProgramPublishError {
    Recheck(ProgramRecheckError),
    Persistence(PersistenceError),
    ScheduleLost,
}
impl From<PersistenceError> for ProgramPublishError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

impl ProgramRuntime {
    /// Reconcile a durable exact ACK after independent process/stream validation.
    /// A missing attention bit alone cannot consume a deferred result.
    #[allow(clippy::too_many_arguments)]
    pub fn reconcile_acknowledgement(
        &mut self,
        collector: &mut impl ProgramCollector,
        decision: &ProgramDecision,
        pane: &ProgramPane<'_>,
        manifests: &[LoadedManifest],
        processes: &ProcessSnapshot,
        read: &store::PaneRead,
        collector_token: &Token,
        now: u64,
    ) -> Result<Option<ProgramDecision>, ProgramRecheckError> {
        self.recheck(collector, decision, pane, manifests, processes)?;
        let Some(result) = decision.selection.result() else {
            return Ok(None);
        };
        let Some(owner) = read.owner() else {
            return Ok(None);
        };
        if &owner.collector != collector_token
            || owner.episode != decision.selection.authority().owner()
            || owner.stream_epoch != decision.boundary.connection.get()
            || owner.processes.as_slice() != decision.process_chain
            || !matches!(read.projection(now), Ok(model::ProjectionRead::Live { .. }))
        {
            return Ok(None);
        }
        let Some(exact) = PersistedResult::new(owner, result) else {
            return Ok(None);
        };
        if read.program.result.as_ref() != Some(&exact)
            || read.program.acknowledged.as_ref() != Some(&exact)
            || read.program.invalidated.as_ref() == Some(&exact)
        {
            return Ok(None);
        }
        let acknowledged = if result.pending {
            self.reconcile_result(decision, ResultDisposition::Acknowledged)
        } else {
            self.acknowledge(decision.pane, &result.identity)
        };
        Ok(acknowledged.then(|| self.decision(decision.pane)).flatten())
    }
    /// Recheck fresh B4 facts, reconcile exact durable markers, reacquire the decision, publish,
    /// and only then mark its pending result presented. A loss never advances presentation.
    /// The caller must consume Recheck::Revoked releases and handle Unsupported by revoking
    /// collector coverage, falling back, and reacquiring a fresh token after capability returns.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_checked(
        &mut self,
        tmux: &Tmux,
        collector: &mut impl ProgramCollector,
        decision: &ProgramDecision,
        pane: &ProgramPane<'_>,
        manifests: &[LoadedManifest],
        processes: &ProcessSnapshot,
        collector_token: &Token,
        session: SessionId,
        clock: impl FnMut() -> u64,
    ) -> Result<CommitOutcome, ProgramPublishError> {
        self.publish_checked_with_completion(
            tmux,
            collector,
            decision,
            pane,
            manifests,
            processes,
            collector_token,
            session,
            clock,
            || true,
        )
    }

    /// The daemon validates continuation after preparation and before each new
    /// publication, then again after IO before presentation. A failed post-write
    /// check invalidates the exact committed owner.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_checked_with_completion(
        &mut self,
        tmux: &Tmux,
        collector: &mut impl ProgramCollector,
        decision: &ProgramDecision,
        pane: &ProgramPane<'_>,
        manifests: &[LoadedManifest],
        processes: &ProcessSnapshot,
        collector_token: &Token,
        session: SessionId,
        mut clock: impl FnMut() -> u64,
        mut completed: impl FnMut() -> bool,
    ) -> Result<CommitOutcome, ProgramPublishError> {
        self.recheck(collector, decision, pane, manifests, processes)
            .map_err(ProgramPublishError::Recheck)?;
        let target = format!("${}:.%{}", session.get(), decision.pane.get());
        let observed = store::read_pane(tmux, &target)?;
        if !completed() {
            return Err(ProgramPublishError::ScheduleLost);
        }
        let owner = PaneOwner {
            collector: collector_token.clone(),
            episode: decision.selection.authority().owner(),
            processes: ProcessChain::new(decision.process_chain.clone())
                .ok_or(PersistenceError::Invalid("invalid publication ancestry"))?,
            stream_epoch: decision.boundary.connection.get(),
        };
        let mut current = decision.clone();
        let mut recovered_history = None;
        if let Some(result) = current.selection.result() {
            let exact = PersistedResult::new(owner.clone(), result)
                .ok_or(PersistenceError::Invalid("invalid selected result"))?;
            let persisted = observed.result();
            let exact_match = persisted.as_ref() == Some(&exact)
                && observed.program.invalidated.as_ref() != Some(&exact);
            // Gap recovery has an independently retained B4 presentation context. History alone
            // never grants current authority; the fresh report and recheck above supply it.
            let recovered = persisted.as_ref().is_some_and(|persisted| {
                observed.program.invalidated.as_ref() != Some(persisted)
                    && persisted.owner.processes == owner.processes
                    && persisted.identity.record == exact.identity.record
                    && persisted.kind == exact.kind
                    && !current.selection.recovery_reset()
                    && !current.observed_work_cycle
                    && (observed.program.acknowledged.as_ref() == Some(persisted)
                        || observed
                            .program
                            .notified
                            .as_ref()
                            .is_some_and(|claim| &claim.result == persisted)
                        || observed
                            .options
                            .get(opt::TURN_AT)
                            .and_then(|value| value.parse::<u64>().ok())
                            .is_some_and(|turn| turn != 0))
            });
            if exact_match || recovered {
                let acknowledged = persisted
                    .as_ref()
                    .is_some_and(|result| observed.program.acknowledged.as_ref() == Some(result));
                let notified = persisted.as_ref().is_some_and(|result| {
                    observed
                        .program
                        .notified
                        .as_ref()
                        .is_some_and(|claim| &claim.result == result)
                });
                recovered_history = Some((exact, acknowledged, notified));
                let reconciled = if exact_match && acknowledged && !result.pending {
                    self.acknowledge(current.pane, &result.identity)
                } else {
                    self.reconcile_result(
                        &current,
                        if acknowledged {
                            ResultDisposition::Acknowledged
                        } else {
                            ResultDisposition::Presented
                        },
                    )
                };
                if reconciled {
                    current = self
                        .decision(decision.pane)
                        .ok_or(ProgramPublishError::Recheck(
                            ProgramRecheckError::StaleDecision,
                        ))?;
                }
            }
        }
        self.recheck(collector, &current, pane, manifests, processes)
            .map_err(ProgramPublishError::Recheck)?;
        let now = clock();
        let blocked_history = observed
            .options
            .get(model::opt::BLOCKED_HISTORY)
            .and_then(|raw| model::BlockedHistory::parse(raw));
        let blocked_continuity = current.selection.state() == AgentState::Blocked
            && blocked_history.as_ref().is_some_and(|history| {
                history.processes == owner.processes
                    && observed
                        .options
                        .get(model::opt::ATTENTION_EPISODE)
                        .map(String::as_str)
                        == Some(history.episode.as_str())
            })
            && observed.options.get(opt::STATE).map(String::as_str) == Some("blocked");
        let previous = model::current_stamp(&observed.options, now)
            .ok()
            .flatten()
            .map(ReadResult::into_inner)
            .or_else(|| {
                (recovered_history.is_some() || blocked_continuity)
                    .then(|| {
                        tma_core::StampedState::from_options(&observed.options)
                            .ok()
                            .flatten()
                            .map(ReadResult::into_inner)
                    })
                    .flatten()
            });
        let result = current
            .selection
            .result()
            .and_then(|result| PersistedResult::new(owner.clone(), result))
            .or_else(|| {
                recovered_history
                    .as_ref()
                    .map(|(result, _, _)| result.clone())
            });
        let recorded_result_current = observed.result().is_some_and(|history| {
            // Generation/cycle counters are comparable only within the exact same owner epoch.
            history.owner == owner
                && self
                    .records(current.pane)
                    .is_some_and(|records| records.retains_result(&history.identity, history.cycle))
        });
        let history = if let Some((_, acknowledged, notified)) = &recovered_history {
            model::ResultHistory::Rebind {
                acknowledged: *acknowledged,
                notified: *notified,
            }
        } else if !recorded_result_current
            && (current.observed_work_cycle
                || observed.result().is_some_and(|history| {
                    self.records(current.pane).is_some_and(|records| {
                        records.recovery_reset_for(history.identity.record.as_ref())
                    })
                }))
        {
            model::ResultHistory::Reset
        } else {
            model::ResultHistory::Retain
        };
        let pending = current
            .selection
            .result()
            .is_some_and(|result| result.pending);
        let exact_ack = result
            .as_ref()
            .is_some_and(|result| observed.program.acknowledged.as_ref() == Some(result));
        let same_result = result
            .as_ref()
            .is_some_and(|result| observed.program.result.as_ref() == Some(result));
        let blocked = current.selection.state() == AgentState::Blocked;
        let same_blocked = blocked
            && (observed.owner().as_ref() == Some(&owner) || blocked_continuity)
            && previous
                .as_ref()
                .is_some_and(|stamp| stamp.state == AgentState::Blocked);
        let attention = if blocked {
            if blocked_continuity {
                !blocked_history.as_ref().unwrap().acknowledged
            } else {
                !same_blocked || previous.as_ref().is_some_and(|stamp| stamp.attention)
            }
        } else {
            !exact_ack
                && (pending
                    || recovered_history
                        .as_ref()
                        .is_some_and(|(_, acknowledged, _)| !acknowledged)
                    || same_result
                        && previous.as_ref().is_some_and(|stamp| {
                            stamp.state == AgentState::Idle && stamp.attention
                        }))
        };
        let mut companions = vec![if attention {
            OptionEdit::set(opt::ATTENTION, "1")
        } else {
            OptionEdit::unset(opt::ATTENTION)
        }];
        if pending && !same_result && !exact_ack {
            // The public --since floor is exclusive. Equal wall clocks must still expose a new
            // selected result without changing the write-once state-run duration.
            let turn = now.max(
                previous
                    .as_ref()
                    .map_or(0, |stamp| stamp.episode_at().saturating_add(1)),
            );
            companions.push(OptionEdit::set(opt::TURN_AT, turn.to_string()));
        }
        let publish = Publish {
            pane_id: target.clone(),
            state: current.selection.state(),
            detail: current.selection.detail().cloned(),
            source: Provenance::Hook,
            evidence_at: current.selection.at(),
            since: current.selection.at(),
            stamped_at: now,
            hash: previous.as_ref().and_then(|stamp| stamp.hash),
            pid: owner.processes.agent().pid,
            name: current.manifest.clone(),
            set_attention: false,
            episode_reset: observed
                .owner()
                .as_ref()
                .is_some_and(|previous| previous.processes != owner.processes),
            guard: Guard::Unconditional,
        };
        if !completed() {
            return Err(ProgramPublishError::ScheduleLost);
        }
        let outcome = match store::publish_program_with_history(
            tmux,
            &observed,
            &owner,
            &mut clock,
            &publish,
            result.as_ref(),
            &companions,
            &history,
            Some(current.selection.record_id()),
        ) {
            Err(PersistenceError::Unsupported) => {
                // A single exact-owner mutation remains safe under yielding option hooks.
                if !completed() {
                    return Err(ProgramPublishError::ScheduleLost);
                }
                if let Some(previous) = observed.owner() {
                    store::revoke_program(tmux, &target, &previous)?;
                }
                return Err(PersistenceError::Unsupported.into());
            }
            outcome => outcome?,
        };
        if outcome.won() {
            if !completed() {
                store::revoke_program(tmux, &target, &owner)?;
                return Err(ProgramPublishError::ScheduleLost);
            }
            if pending {
                self.mark_presented(&current);
            }
        }
        // Summary failure does not undo a committed result or authorize repeating presentation.
        if !completed() {
            return Err(ProgramPublishError::ScheduleLost);
        }
        let _ = tma_tmux::stamp::reconcile_summaries(tmux);
        Ok(outcome)
    }
}

/// Consume each B4 step's history invalidation before publishing its decision. Release-only
/// steps carry the same invalidation on ProgramRelease; consume callback releases once, then
/// only ProgramBatch::unstreamed_releases after return. Gaps retain history; RIS/clear fence it.
pub fn release_persisted(
    tmux: &Tmux,
    session: SessionId,
    collector: &Token,
    release: &super::ProgramRelease,
) -> Result<(), PersistenceError> {
    release_persisted_checked(tmux, session, collector, release, || true).map(|_| ())
}

/// Stop between independent persistence operations when scheduling permission
/// is lost. False leaves exact collector retirement and bounded cleanup to the caller.
pub fn release_persisted_checked(
    tmux: &Tmux,
    session: SessionId,
    collector: &Token,
    release: &super::ProgramRelease,
    mut allowed: impl FnMut() -> bool,
) -> Result<bool, PersistenceError> {
    let target = format!("${}:.%{}", session.get(), release.pane.get());
    let observed = store::read_pane(tmux, &target)?;
    let Some(owner) = observed
        .owner()
        .filter(|owner| owner.collector == *collector && Some(owner.episode) == release.owner)
    else {
        return Ok(true);
    };
    if !allowed() {
        return Ok(false);
    }
    store::revoke_program(tmux, &target, &owner)?;
    if let Some(invalidation) = &release.history_invalidation {
        if !allowed() {
            return Ok(false);
        }
        store::invalidate_history(tmux, &observed, &owner, invalidation)?;
    }
    if !allowed() {
        return Ok(false);
    }
    match store::release_program(tmux, &observed, &owner, &[]) {
        Ok(_) => Ok(true),
        Err(PersistenceError::Unsupported) => {
            if !allowed() {
                return Ok(false);
            }
            store::revoke_program(tmux, &target, &owner).map(|_| true)
        }
        Err(error) => Err(error),
    }
}
