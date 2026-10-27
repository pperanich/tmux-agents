use super::*;

impl ProgramService {
    pub(super) fn maintain_truth(
        &mut self,
        inputs: &mut RunInputs<'_>,
        truth: &mut BTreeMap<PaneId, store::PaneRead>,
        work: &WorkBudget,
        periodic: bool,
    ) -> PhaseOutcome {
        let RunInputs { tmux, pool, .. } = inputs;
        let tmux = *tmux;
        let pool = &mut **pool;
        // Detect known target-local capability loss even when the application is silent.
        let mut changed = false;
        if periodic && self.mode == ProgramStatusMode::Prefer {
            let owners: Vec<_> = self
                .committed
                .iter()
                .filter_map(|(&pane, committed)| {
                    Some((
                        pane,
                        self.targets.get(&pane).copied()?,
                        committed.owner.clone(),
                    ))
                })
                .collect();
            for (pane, target, owner) in owners {
                if !self.continue_run(tmux, pool, work) {
                    return PhaseOutcome::Stop;
                }
                let read = store::read_pane(tmux, &target.address());
                if !self.continue_run(tmux, pool, work) {
                    return PhaseOutcome::Stop;
                }
                match read {
                    Ok(read) if !read.option_hooks => {
                        truth.insert(pane, read);
                    }
                    Ok(_) => {
                        if let Err(error) = revoke_exact(tmux, &target.address(), &owner) {
                            self.error("capability revocation", error);
                            self.abort_batch(tmux, pool, BatchStop::PersistenceLost);
                            return PhaseOutcome::Stop;
                        }
                        if !self.continue_run(tmux, pool, work) {
                            return PhaseOutcome::Stop;
                        }
                        let batch = self.runtime.invalidate(
                            pool,
                            pane,
                            RecoveryReason::CapabilityUnavailable,
                        );
                        for release in batch.unstreamed_releases() {
                            if !self.release(tmux, pool, release, work) {
                                return PhaseOutcome::Stop;
                            }
                        }
                        self.counts.capability_losses += 1;
                        control::mark_recheck(pool, &[format!("%{}", pane.get())], Instant::now());
                        changed = true;
                    }
                    Err(error) => {
                        self.error("capability read", error);
                        self.abort_batch(tmux, pool, BatchStop::PersistenceLost);
                        return PhaseOutcome::Stop;
                    }
                }
            }
        }
        if self.mode == ProgramStatusMode::Prefer {
            let lost: Vec<_> = self
                .committed
                .iter()
                .filter_map(|(&pane, committed)| {
                    let valid = truth.get(&pane).is_some_and(|read| {
                        read.owner().as_ref() == Some(&committed.owner)
                            && read.program.commit.as_ref() == Some(&committed.commit)
                            && matches!(
                                read.projection(tma_runtime::now_ms()),
                                Ok(ProjectionRead::Live { .. })
                            )
                    });
                    (!valid).then_some(pane)
                })
                .collect();
            for pane in lost {
                if !self.continue_run(tmux, pool, work) {
                    return PhaseOutcome::Stop;
                }
                let batch = self
                    .runtime
                    .invalidate(pool, pane, RecoveryReason::PersistenceLost);
                for release in batch.unstreamed_releases() {
                    if !self.release(tmux, pool, release, work) {
                        return PhaseOutcome::Stop;
                    }
                }
                control::mark_recheck(pool, &[format!("%{}", pane.get())], Instant::now());
                changed = true;
            }
        }
        if self.cleanup_pending {
            return PhaseOutcome::Stop;
        }
        if changed
            && self.committed.is_empty()
            && self.last_loss == Some(RecoveryReason::CapabilityUnavailable)
            && self.lease.is_some()
        {
            self.lose(tmux, pool, RecoveryReason::CapabilityUnavailable);
            return PhaseOutcome::Stop;
        }
        PhaseOutcome::Continue(changed)
    }
}
