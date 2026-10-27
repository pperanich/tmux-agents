use super::*;

pub(super) struct BatchEffects<'a, 'facts> {
    pub(super) tmux: &'a Tmux,
    pub(super) manifests: &'a [LoadedManifest],
    pub(super) notify: &'a mut NotifyState,
    pub(super) subscribers: &'a mut Vec<UnixStream>,
    pub(super) panes: &'a [ProgramPane<'facts>],
    pub(super) lease: &'a mut Option<Lease>,
    pub(super) committed: &'a mut BTreeMap<PaneId, Committed>,
    pub(super) targets: &'a BTreeMap<PaneId, Target>,
    pub(super) counts: &'a mut Counters,
    pub(super) origin: Instant,
    pub(super) mode: ProgramStatusMode,
    pub(super) last_loss: &'a mut Option<RecoveryReason>,
    pub(super) truth: &'a mut BTreeMap<PaneId, store::PaneRead>,
    pub(super) work: &'a mut WorkBudget,
    pub(super) batch_stop: Option<BatchStop>,
    pub(super) refreshed_processes: Option<Result<ProcessSnapshot, ProcessUnavailable>>,
    pub(super) checked_at: Option<Instant>,
    pub(super) releases_due: Vec<String>,
    pub(super) changed: bool,
}

pub(super) struct EffectOutcome {
    pub(super) stopped: Option<BatchStop>,
    pub(super) changed: bool,
    pub(super) releases_due: Vec<String>,
}
impl BatchEffects<'_, '_> {
    pub(super) fn step(
        &mut self,
        runtime: &mut ProgramRuntime,
        pool: &mut ControlPool,
        step: ProgramStep<'_>,
    ) {
        let Self {
            tmux,
            lease,
            committed,
            targets,
            counts,
            origin,
            mode,
            last_loss,
            truth,
            work,
            batch_stop,
            refreshed_processes,
            checked_at,
            releases_due,
            changed,
            ..
        } = self;
        let tmux = *tmux;
        let lease = &mut **lease;
        let committed = &mut **committed;
        let targets = *targets;
        let counts = &mut **counts;
        let last_loss = &mut **last_loss;
        let truth = &mut **truth;
        let work = &mut **work;
        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
            return;
        }
        match step.kind {
            ProgramStepKind::Replace => counts.reports += 1,
            ProgramStepKind::Query => counts.queries += 1,
            _ => {}
        }
        if *mode != ProgramStatusMode::Prefer || lease.is_none() {
            return;
        }

        if work.exhausted.contains(&step.pane) {
            return;
        }
        if let Err(reason) = refresh_processes(refreshed_processes, checked_at, counts) {
            stop_batch(
                tmux,
                lease,
                counts,
                batch_stop,
                BatchStop::ProcessUnavailable(reason),
            );
            return;
        }
        let decision = runtime.decision(step.pane);
        let is_duplicate = decision.as_ref().is_some_and(|decision| {
            duplicate(
                committed.get(&step.pane),
                truth.get(&step.pane),
                decision,
                step.history_invalidation.is_some(),
            )
        });
        let releases = step
            .releases
            .iter()
            .filter(|release| release.owner.is_some())
            .count();
        let operations = releases
            + usize::from(decision.is_some() && !is_duplicate)
            + usize::from(decision.is_some() && step.history_invalidation.is_some());
        match work.admit(step.pane, operations, work.started.elapsed()) {
            Admission::BatchExhausted => {
                stop_batch(tmux, lease, counts, batch_stop, BatchStop::WorkBudget);
                return;
            }
            Admission::PaneExhausted => {
                let started = Instant::now();
                counts.work_pane_losses += 1;
                let Some(target) = targets.get(&step.pane).copied() else {
                    stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                    return;
                };
                let owner = if let Some(committed) = committed.get(&step.pane) {
                    Some(committed.owner.clone())
                } else {
                    match store::read_pane(tmux, &target.address()) {
                        Ok(read) => read.owner().filter(|owner| {
                            lease
                                .as_ref()
                                .is_some_and(|current| owner.collector == current.persisted.token)
                        }),
                        Err(error) => {
                            counts.errors += 1;
                            eprintln!(
                                "tma: program work-budget owner read: {}",
                                persistence_failure(&error)
                            );
                            stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                            return;
                        }
                    }
                };
                if let Some(owner) = owner {
                    if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                        return;
                    }
                    if let Err(error) = revoke_exact(tmux, &target.address(), &owner) {
                        counts.errors += 1;
                        eprintln!(
                            "tma: program work-budget revoke: {}",
                            persistence_failure(&error)
                        );
                        stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                        return;
                    }
                }
                committed.remove(&step.pane);
                runtime.invalidate(pool, step.pane, RecoveryReason::WorkBudgetExceeded);
                *last_loss = Some(RecoveryReason::WorkBudgetExceeded);
                releases_due.push(format!("%{}", step.pane.get()));
                *changed = true;
                work.charge(step.pane, started.elapsed());
                return;
            }
            Admission::Allow => {}
        }
        let started = Instant::now();
        let pane = step.pane;
        self.apply_step(runtime, pool, step, decision, is_duplicate);
        if operations != 0 {
            self.work.charge(pane, started.elapsed());
        }
    }
    fn apply_step(
        &mut self,
        runtime: &mut ProgramRuntime,
        pool: &mut ControlPool,
        step: ProgramStep<'_>,
        decision: Option<ProgramDecision>,
        is_duplicate: bool,
    ) {
        let Self {
            tmux,
            manifests,
            notify,
            subscribers,
            panes,
            lease,
            committed,
            targets,
            counts,
            origin,
            last_loss,
            truth,
            work,
            batch_stop,
            refreshed_processes,
            checked_at,
            releases_due,
            changed,
            ..
        } = self;
        let tmux = *tmux;
        let manifests = *manifests;
        let notify = &mut **notify;
        let subscribers = &mut **subscribers;
        let panes = *panes;
        let lease = &mut **lease;
        let committed = &mut **committed;
        let targets = *targets;
        let counts = &mut **counts;
        let last_loss = &mut **last_loss;
        let truth = &mut **truth;
        let work = &mut **work;

        for release in step.releases {
            if release.owner.is_none() {
                continue;
            }
            if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                return;
            }
            if !release_or_stop(
                tmux,
                lease,
                targets,
                committed,
                counts,
                release,
                batch_stop,
                *origin,
                work.started,
            ) {
                return;
            }
            *last_loss = Some(release.reason);
            releases_due.push(format!("%{}", release.pane.get()));
            *changed = true;
        }
        let Some(decision) = decision else {
            return;
        };
        let Some(target) = targets.get(&step.pane).copied() else {
            return;
        };
        let Some(pane) = panes.iter().find(|pane| pane.pane == step.pane) else {
            return;
        };
        let checked = match refreshed_processes.as_ref() {
            Some(Ok(processes)) => processes,
            _ => step.checked_processes,
        };
        if let Err(error) = runtime.recheck(pool, &decision, pane, manifests, checked) {
            if let ProgramRecheckError::Revoked(release) = error {
                if !release_or_stop(
                    tmux,
                    lease,
                    targets,
                    committed,
                    counts,
                    &release,
                    batch_stop,
                    *origin,
                    work.started,
                ) {
                    return;
                }
                releases_due.push(format!("%{}", release.pane.get()));
                *changed = true;
            }
            return;
        }
        if is_duplicate {
            counts.duplicates += 1;
            return;
        }
        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
            return;
        }
        if let Some(invalidation) = &step.history_invalidation {
            match store::read_pane(tmux, &target.address()) {
                Ok(read) => {
                    if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                        return;
                    }
                    if let Some(owner) = read.owner().filter(|owner| {
                        lease
                            .as_ref()
                            .is_some_and(|current| owner.collector == current.persisted.token)
                            && owner.episode == decision.selection.authority().owner()
                    }) {
                        if let Err(error) =
                            store::invalidate_history(tmux, &read, &owner, invalidation)
                        {
                            counts.errors += 1;
                            eprintln!(
                                "tma: program history invalidation: {}",
                                persistence_failure(&error)
                            );
                            stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                            return;
                        }
                    }
                }
                Err(error) => {
                    counts.errors += 1;
                    eprintln!("tma: program history read: {}", persistence_failure(&error));
                    stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                    return;
                }
            }
        }
        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
            return;
        }
        let fresh = match tmux.list_panes() {
            Ok(rows) => rows,
            Err(error) => {
                counts.errors += 1;
                eprintln!("tma: program fresh pane read: {}", tmux_failure(&error));
                stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                return;
            }
        };
        let Some(row) = fresh
            .iter()
            .find(|row| PaneId::parse(&row.pane_id).ok() == Some(step.pane))
        else {
            return;
        };
        let registration = registration(row);
        let Some(pane) = facts(row, registration.as_ref()) else {
            return;
        };
        if let Err(reason) = refresh_processes(refreshed_processes, checked_at, counts) {
            stop_batch(
                tmux,
                lease,
                counts,
                batch_stop,
                BatchStop::ProcessUnavailable(reason),
            );
            return;
        }
        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
            return;
        }
        let checked = match refreshed_processes.as_ref() {
            Some(Ok(processes)) => processes,
            _ => step.checked_processes,
        };
        let Some(current_lease) = lease.as_mut() else {
            return;
        };
        let token = current_lease.persisted.token.clone();
        let publication_started = Instant::now();
        let outcome = runtime.publish_checked_with_completion(
            tmux,
            pool,
            &decision,
            &pane,
            manifests,
            checked,
            &token,
            target.session,
            tma_runtime::now_ms,
            || {
                !shutdown_requested()
                    && work.started.elapsed() < BATCH_IO_WINDOW
                    && current_lease.live(*origin)
            },
        );
        counts.max_publication_ms = counts
            .max_publication_ms
            .max(publication_started.elapsed().as_millis());
        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
            return;
        }
        match outcome {
            Ok(outcome) if outcome.won() => {
                counts.publications += 1;
                let read = outcome.winner();
                if let (Some(decision), Some(owner), Some(commit)) = (
                    runtime.decision(step.pane),
                    read.owner(),
                    read.program.commit.clone(),
                ) {
                    committed.insert(
                        step.pane,
                        Committed {
                            decision,
                            owner,
                            commit,
                            acknowledged: read.program.acknowledged.clone(),
                        },
                    );
                }
                truth.insert(step.pane, read.clone());
                counts.notify_passes += 1;
                let Some(notification_lease) = lease.as_mut() else {
                    return;
                };
                let notification_started = Instant::now();
                if let Err(error) = notify.reconcile_checked(tmux, |read| {
                    !read.program.program_origin
                        || (!shutdown_requested()
                            && work.started.elapsed() < BATCH_IO_WINDOW
                            && read.program.owner.as_ref().is_some_and(|owner| {
                                owner.collector == notification_lease.persisted.token
                            })
                            && notification_lease.live(*origin))
                }) {
                    counts.errors += 1;
                    eprintln!("tma: program notification: {}", tmux_failure(&error));
                }
                counts.max_notification_ms = counts
                    .max_notification_ms
                    .max(notification_started.elapsed().as_millis());
                if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                    return;
                }

                let _ = push_subscribers(subscribers);
                *changed = true;
            }
            Ok(_) => {
                let batch = runtime.invalidate(pool, step.pane, RecoveryReason::PersistenceLost);
                for release in batch.unstreamed_releases() {
                    if !release_or_stop(
                        tmux,
                        lease,
                        targets,
                        committed,
                        counts,
                        release,
                        batch_stop,
                        *origin,
                        work.started,
                    ) {
                        return;
                    }
                    releases_due.push(format!("%{}", release.pane.get()));
                }
                *changed = true;
            }
            Err(ProgramPublishError::Recheck(ProgramRecheckError::Revoked(release))) => {
                if !release_or_stop(
                    tmux,
                    lease,
                    targets,
                    committed,
                    counts,
                    &release,
                    batch_stop,
                    *origin,
                    work.started,
                ) {
                    return;
                }
                releases_due.push(format!("%{}", release.pane.get()));
                *changed = true;
            }
            Err(ProgramPublishError::Recheck(ProgramRecheckError::StaleDecision)) => {}
            Err(ProgramPublishError::ScheduleLost) => {
                stop_batch(tmux, lease, counts, batch_stop, BatchStop::LeaseLost);
            }
            Err(ProgramPublishError::Persistence(error)) => {
                let reason = if matches!(error, store::PersistenceError::Unsupported) {
                    counts.capability_losses += 1;
                    RecoveryReason::CapabilityUnavailable
                } else {
                    counts.errors += 1;
                    eprintln!("tma: program publication: {}", persistence_failure(&error));
                    RecoveryReason::PersistenceLost
                };
                let batch = runtime.invalidate(pool, step.pane, reason);
                for release in batch.unstreamed_releases() {
                    if !release_or_stop(
                        tmux,
                        lease,
                        targets,
                        committed,
                        counts,
                        release,
                        batch_stop,
                        *origin,
                        work.started,
                    ) {
                        return;
                    }
                    releases_due.push(format!("%{}", release.pane.get()));
                }
                *last_loss = Some(reason);
                *changed = true;
            }
        }
    }
    pub(super) fn tail(
        &mut self,
        runtime: &mut ProgramRuntime,
        pool: &mut ControlPool,
        releases: &[ProgramRelease],
    ) {
        let Self {
            tmux,
            lease,
            committed,
            targets,
            counts,
            origin,
            last_loss,
            work,
            batch_stop,
            releases_due,
            changed,
            ..
        } = self;
        let tmux = *tmux;
        let lease = &mut **lease;
        let committed = &mut **committed;
        let targets = *targets;
        let counts = &mut **counts;
        let last_loss = &mut **last_loss;
        let work = &mut **work;
        for release in releases {
            if release.owner.is_none() || lease.is_none() || work.exhausted.contains(&release.pane)
            {
                continue;
            }
            if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                break;
            }
            let started = Instant::now();
            match work.admit(release.pane, 1, work.started.elapsed()) {
                Admission::Allow => {
                    if !release_or_stop(
                        tmux,
                        lease,
                        targets,
                        committed,
                        counts,
                        release,
                        batch_stop,
                        *origin,
                        work.started,
                    ) {
                        break;
                    }
                    *last_loss = Some(release.reason);
                    releases_due.push(format!("%{}", release.pane.get()));
                    *changed = true;
                }
                Admission::PaneExhausted => {
                    let outcome = if let (Some(current), Some(target)) =
                        (lease.as_ref(), targets.get(&release.pane))
                    {
                        let token = current.persisted.token.clone();
                        let read = store::read_pane(tmux, &target.address());
                        if !check_io(tmux, lease, counts, batch_stop, *origin, work.started) {
                            break;
                        }
                        read.and_then(|read| {
                            if let Some(owner) = read.owner().filter(|owner| {
                                owner.collector == token && Some(owner.episode) == release.owner
                            }) {
                                revoke_exact(tmux, &target.address(), &owner)
                            } else {
                                Ok(())
                            }
                        })
                    } else {
                        Err(store::PersistenceError::Invalid(
                            "missing budget release target",
                        ))
                    };
                    if let Err(error) = outcome {
                        counts.errors += 1;
                        eprintln!("tma: program tail revoke: {}", persistence_failure(&error));
                        stop_batch(tmux, lease, counts, batch_stop, BatchStop::PersistenceLost);
                        break;
                    }
                    committed.remove(&release.pane);
                    runtime.invalidate(pool, release.pane, RecoveryReason::WorkBudgetExceeded);
                    counts.work_pane_losses += 1;
                    *last_loss = Some(RecoveryReason::WorkBudgetExceeded);
                    releases_due.push(format!("%{}", release.pane.get()));
                    *changed = true;
                }
                Admission::BatchExhausted => {
                    stop_batch(tmux, lease, counts, batch_stop, BatchStop::WorkBudget);
                    break;
                }
            }
            work.charge(release.pane, started.elapsed());
        }
    }
    pub(super) fn finish(self) -> EffectOutcome {
        self.counts.work_max_operations = self.counts.work_max_operations.max(self.work.operations);
        self.counts.work_max_batch_ms = self
            .counts
            .work_max_batch_ms
            .max(self.work.started.elapsed().as_millis());
        EffectOutcome {
            stopped: self.batch_stop,
            changed: self.changed,
            releases_due: self.releases_due,
        }
    }
}
