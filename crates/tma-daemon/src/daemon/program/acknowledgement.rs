use super::*;

struct AckFacts {
    rows: Vec<PaneRecord>,
    processes: ProcessSnapshot,
    checked_at: Instant,
}

impl ProgramService {
    pub(super) fn reconcile_acks(
        &mut self,
        inputs: &mut RunInputs<'_>,
        truth: &BTreeMap<PaneId, store::PaneRead>,
        work: &mut WorkBudget,
        periodic: bool,
    ) -> Result<PhaseOutcome, TmuxError> {
        if !periodic || self.mode != ProgramStatusMode::Prefer {
            return Ok(PhaseOutcome::Continue(false));
        }
        let pending: Vec<_> = self
            .committed
            .keys()
            .filter_map(|&pane| {
                truth.get(&pane)?.program.acknowledged.as_ref()?;
                let decision = self.runtime.decision(pane)?;
                decision.selection.result()?;
                Some(decision)
            })
            .collect();
        if pending.is_empty() {
            return Ok(PhaseOutcome::Continue(false));
        }
        if !self.continue_run(inputs.tmux, inputs.pool, work) {
            return Ok(PhaseOutcome::Stop);
        }
        let mut facts = None;
        let mut changed = false;
        for decision in pending {
            if !self.continue_run(inputs.tmux, inputs.pool, work) {
                return Ok(PhaseOutcome::Stop);
            }
            match work.admit(decision.pane, 2, work.started.elapsed()) {
                Admission::Allow => {}
                Admission::PaneExhausted => {
                    self.counts.work_pane_losses += 1;
                    if !self.invalidate_pane(
                        inputs.tmux,
                        inputs.pool,
                        decision.pane,
                        RecoveryReason::WorkBudgetExceeded,
                        work,
                    ) {
                        return Ok(PhaseOutcome::Stop);
                    }
                    changed = true;
                    continue;
                }
                Admission::BatchExhausted => {
                    self.abort_batch(inputs.tmux, inputs.pool, BatchStop::WorkBudget);
                    return Ok(PhaseOutcome::Stop);
                }
            }
            let pane = decision.pane;
            let started = Instant::now();
            let outcome = self.reconcile_one_ack(inputs, decision, &mut facts, work)?;
            work.charge(pane, started.elapsed());
            match outcome {
                PhaseOutcome::Continue(ack_changed) => changed |= ack_changed,
                PhaseOutcome::Stop => return Ok(PhaseOutcome::Stop),
            }
        }
        Ok(PhaseOutcome::Continue(changed))
    }

    fn refresh_ack_facts(
        &mut self,
        tmux: &Tmux,
        pool: &mut ControlPool,
        facts: &mut Option<AckFacts>,
        work: &WorkBudget,
    ) -> Result<bool, TmuxError> {
        if facts
            .as_ref()
            .is_some_and(|facts| facts.checked_at.elapsed() < PROCESS_CADENCE)
        {
            return Ok(true);
        }
        if !self.continue_run(tmux, pool, work) {
            return Ok(false);
        }
        let rows = tmux.list_panes()?;
        if !self.continue_run(tmux, pool, work) {
            return Ok(false);
        }
        self.counts.process_samples += 1;
        if facts.is_some() {
            self.counts.delayed_process_samples += 1;
        }
        let processes = match program_processes() {
            Ok(processes) => processes,
            Err(reason) => {
                self.abort_batch(tmux, pool, BatchStop::ProcessUnavailable(reason));
                return Ok(false);
            }
        };
        *facts = Some(AckFacts {
            rows,
            processes,
            checked_at: Instant::now(),
        });
        Ok(self.continue_run(tmux, pool, work))
    }

    fn reconcile_one_ack(
        &mut self,
        inputs: &mut RunInputs<'_>,
        decision: ProgramDecision,
        facts: &mut Option<AckFacts>,
        work: &WorkBudget,
    ) -> Result<PhaseOutcome, TmuxError> {
        let RunInputs {
            tmux,
            pool,
            manifests,
            notify,
            subscribers,
        } = inputs;
        let tmux = *tmux;
        let pool = &mut **pool;
        let pane = decision.pane;
        let Some(target) = self.targets.get(&pane).copied() else {
            return Ok(PhaseOutcome::Continue(false));
        };
        let Some(token) = self
            .lease
            .as_ref()
            .map(|lease| lease.persisted.token.clone())
        else {
            return Ok(PhaseOutcome::Continue(false));
        };
        let read = match store::read_pane(tmux, &target.address()) {
            Ok(read) => read,
            Err(error) => {
                self.error("acknowledgement read", error);
                return Ok(PhaseOutcome::Continue(false));
            }
        };
        if !self.continue_run(tmux, pool, work) {
            return Ok(PhaseOutcome::Stop);
        }
        // The durable read can be slow. Refresh the shared pane/process cache
        // after it, before changing reducer presentation or publishing an ACK.
        if !self.refresh_ack_facts(tmux, pool, facts, work)? {
            return Ok(PhaseOutcome::Stop);
        }
        let facts = facts.as_ref().expect("fresh ACK facts");
        let Some(row) = facts
            .rows
            .iter()
            .find(|row| PaneId::parse(&row.pane_id).ok() == Some(pane))
        else {
            return Ok(
                if self.invalidate_pane(tmux, pool, pane, RecoveryReason::PaneRemoved, work) {
                    PhaseOutcome::Continue(true)
                } else {
                    PhaseOutcome::Stop
                },
            );
        };
        let registration = registration(row);
        let Some(fact) = super::facts(row, registration.as_ref()) else {
            return Ok(PhaseOutcome::Continue(false));
        };
        let processes = &facts.processes;
        let current = match self.runtime.reconcile_acknowledgement(
            pool,
            &decision,
            &fact,
            manifests,
            processes,
            &read,
            &token,
            tma_runtime::now_ms(),
        ) {
            Ok(Some(current)) => current,
            Ok(None) | Err(ProgramRecheckError::StaleDecision) => {
                return Ok(PhaseOutcome::Continue(false))
            }
            Err(ProgramRecheckError::Revoked(release)) => {
                return Ok(if self.release(tmux, pool, &release, work) {
                    PhaseOutcome::Continue(true)
                } else {
                    PhaseOutcome::Stop
                });
            }
        };
        let needs_publication = current.selection.state() != decision.selection.state()
            || current.selection.detail() != decision.selection.detail()
            || current.selection.record_id() != decision.selection.record_id()
            || current
                .selection
                .result()
                .is_some_and(|result| result.pending);
        if !needs_publication {
            if let Some(committed) = self.committed.get_mut(&pane) {
                committed.decision = current;
                committed.acknowledged = read.program.acknowledged.clone();
            }
            return Ok(PhaseOutcome::Continue(false));
        }
        if !self.continue_run(tmux, pool, work) {
            return Ok(PhaseOutcome::Stop);
        }
        let Some(lease) = self.lease.as_mut() else {
            return Ok(PhaseOutcome::Stop);
        };
        let outcome = self.runtime.publish_checked_with_completion(
            tmux,
            pool,
            &current,
            &fact,
            manifests,
            processes,
            &token,
            target.session,
            tma_runtime::now_ms,
            || {
                !shutdown_requested()
                    && work.started.elapsed() < BATCH_IO_WINDOW
                    && lease.live(self.origin)
            },
        );
        if !self.continue_run(tmux, pool, work) {
            return Ok(PhaseOutcome::Stop);
        }
        match outcome {
            Ok(outcome) if outcome.won() => {
                let read = outcome.winner();
                if let (Some(decision), Some(owner), Some(commit)) = (
                    self.runtime.decision(pane),
                    read.owner(),
                    read.program.commit.clone(),
                ) {
                    self.committed.insert(
                        pane,
                        Committed {
                            decision,
                            owner,
                            commit,
                            acknowledged: read.program.acknowledged.clone(),
                        },
                    );
                }
                self.counts.publications += 1;
                self.counts.notify_passes += 1;
                self.notify_with_deadline(
                    tmux,
                    pool,
                    notify,
                    Some(work.started + BATCH_IO_WINDOW),
                )?;
                if !self.continue_run(tmux, pool, work) {
                    return Ok(PhaseOutcome::Stop);
                }
                let _ = push_subscribers(subscribers);
            }
            Ok(_) | Err(ProgramPublishError::Persistence(_)) => {
                if let Err(ProgramPublishError::Persistence(error)) = outcome {
                    self.error("acknowledgement publication", error);
                }
                if !self.invalidate_pane(tmux, pool, pane, RecoveryReason::PersistenceLost, work) {
                    return Ok(PhaseOutcome::Stop);
                }
            }
            Err(ProgramPublishError::ScheduleLost) => {
                self.abort_batch(tmux, pool, BatchStop::LeaseLost);
                return Ok(PhaseOutcome::Stop);
            }
            Err(ProgramPublishError::Recheck(ProgramRecheckError::Revoked(release))) => {
                if !self.release(tmux, pool, &release, work) {
                    return Ok(PhaseOutcome::Stop);
                }
            }
            Err(ProgramPublishError::Recheck(ProgramRecheckError::StaleDecision)) => {
                return Ok(PhaseOutcome::Continue(false))
            }
        }
        Ok(PhaseOutcome::Continue(true))
    }
}
