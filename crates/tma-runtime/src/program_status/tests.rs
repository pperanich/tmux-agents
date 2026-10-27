use super::*;
use tma_core::program_status::{
    AppIdRef, ConnectionEpoch, ProgramStatus, RecordIdRef, RecordRef, SingleLineRef,
};
use tma_core::snapshot::{ProcInfo, ProcessBirth, ProcessSample};
use tma_core::{AgentState, Manifest, RuleEngine};

#[derive(Default)]
struct Collector {
    epochs: BTreeMap<PaneId, (u64, u64)>,
    queued: Vec<ObservedEvent>,
    gaps: Vec<ObservedEvent>,
    boundaries: usize,
    budget: usize,
    global_gap: bool,
}
impl Collector {
    fn emit(&mut self, pane: u32, event: ProgramEvent) {
        let &(epoch, sequence) = self.epochs.get(&PaneId::new(pane)).unwrap();
        let sequence = sequence + 1;
        self.epochs.get_mut(&PaneId::new(pane)).unwrap().1 = sequence;
        self.queued.push(ObservedEvent {
            pane: PaneId::new(pane),
            connection: ConnectionEpoch::new(epoch),
            sequence: ObservationSequence::new(sequence),
            event,
        });
    }
    fn report(&mut self, pane: u32, id: &str, app: Option<&str>, status: ProgramStatus) {
        let id = if id.is_empty() {
            RecordIdRef::ROOT
        } else {
            RecordIdRef::parse(id).unwrap()
        };
        let mut record = RecordRef::new(id, status);
        record.app = app.map(|app| AppIdRef::parse(app).unwrap());
        self.emit(
            pane,
            ProgramEvent::Command(ProgramCommand::Replace(record.into_owned())),
        );
    }
    fn gap(&mut self, pane: u32, reason: ObservationGap) {
        self.invalidate(PaneId::new(pane), reason);
    }
}
impl ProgramCollector for Collector {
    fn boundary(&mut self, pane: PaneId) -> Option<ObservationBoundary> {
        self.boundaries += 1;
        self.invalidate(pane, ObservationGap::OwnerChanged);
        let (epoch, sequence) = self.epochs[&pane];
        Some(ObservationBoundary {
            connection: ConnectionEpoch::new(epoch),
            sequence_floor: ObservationSequence::new(sequence),
        })
    }
    fn stream(&self, pane: PaneId) -> Option<ProgramStreamState> {
        if self.global_gap {
            return None;
        }
        self.epochs.get(&pane).map(|(epoch, _)| ProgramStreamState {
            connection: ConnectionEpoch::new(*epoch),
            pending_gap: self.gaps.iter().find_map(|event| match event.event {
                ProgramEvent::Gap(reason) if event.pane == pane => Some(reason),
                _ => None,
            }),
        })
    }
    fn invalidate(&mut self, pane: PaneId, reason: ObservationGap) {
        let reason = self
            .gaps
            .iter()
            .find(|event| {
                event.pane == pane
                    && event.event == ProgramEvent::Gap(ObservationGap::BudgetExceeded)
            })
            .map_or(reason, |_| ObservationGap::BudgetExceeded);
        self.queued.retain(|event| event.pane != pane);
        self.gaps.retain(|event| event.pane != pane);
        let entry = self.epochs.entry(pane).or_default();
        entry.0 += 1;
        entry.1 += 1;
        self.gaps.push(ObservedEvent {
            pane,
            connection: ConnectionEpoch::new(entry.0),
            sequence: ObservationSequence::new(entry.1),
            event: ProgramEvent::Gap(reason),
        });
    }
    fn pending_bytes(&self) -> usize {
        self.queued
            .iter()
            .map(|event| payload_bytes(&event.event))
            .sum()
    }
    fn set_payload_budget(&mut self, available: usize) {
        self.budget = available;
    }
    fn drain(&mut self) -> ProgramDrain {
        ProgramDrain {
            all_invalidated: std::mem::take(&mut self.global_gap),
            invalidations: std::mem::take(&mut self.gaps),
            observations: std::mem::take(&mut self.queued),
        }
    }
}

fn manifest(name: &str, aliases: Option<&str>, titled: bool) -> LoadedManifest {
    let source = format!("min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"node\"]\n{}\n[capture]\nvisible = [\"working\"]\n{}",
        if titled { "title_patterns = [\"^π \" ]" } else { "" },
        aliases.map_or(String::new(), |aliases| format!("[program_status]\napps = [{aliases}]\n")));
    let manifest = Manifest::parse(&source, "test.toml").unwrap();
    let engine = RuleEngine::build(&manifest).unwrap();
    LoadedManifest {
        name: name.into(),
        manifest,
        engine,
    }
}
fn pane(id: u32) -> ProgramPane<'static> {
    ProgramPane {
        pane: PaneId::new(id),
        pane_pid: id * 100,
        current_command: "node",
        title: "π project",
        ignored: false,
        registration: None,
    }
}
fn process(
    pid: u32,
    ppid: u32,
    comm: &str,
    birth: u64,
) -> (u32, Result<ProcessSample, ProcessUnavailable>) {
    (
        pid,
        Ok(ProcessSample {
            info: ProcInfo {
                pid,
                ppid,
                pgid: pid,
                tpgid: Some(pid),
                comm: comm.into(),
            },
            birth: ProcessBirth::LinuxStartTicks(birth),
        }),
    )
}
fn snapshot(ids: &[u32], birth: u64) -> ProcessSnapshot {
    ProcessSnapshot::from_samples(ids.iter().flat_map(|id| {
        [
            process(id * 100, 1, "zsh", 1),
            process(id * 100 + 1, id * 100, "node", birth),
        ]
    }))
}
fn run(
    runtime: &mut ProgramRuntime,
    collector: &mut Collector,
    panes: &[ProgramPane<'_>],
    manifests: &[LoadedManifest],
    snapshot: &ProcessSnapshot,
) -> ProgramBatch {
    let mut samples = 0;
    let batch = runtime.process_batch(
        collector,
        panes,
        manifests,
        &mut || {
            samples += 1;
            Ok(snapshot.clone())
        },
        20,
    );
    if !panes.is_empty() {
        assert_eq!(samples, 2);
    }
    batch
}
fn setup() -> (
    ProgramRuntime,
    Collector,
    Vec<LoadedManifest>,
    ProcessSnapshot,
) {
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    let manifests = vec![manifest("pi", Some("\"pi\""), true)];
    let snapshot = snapshot(&[1], 10);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    (runtime, collector, manifests, snapshot)
}
fn state(runtime: &ProgramRuntime, id: u32) -> Option<AgentState> {
    runtime
        .decision(PaneId::new(id))
        .map(|decision| decision.selection.state())
}

#[test]
fn cold_bind_drops_queued_reports_then_requires_matching_fresh_app() {
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    collector.epochs.insert(PaneId::new(1), (1, 0));
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    let manifests = vec![manifest("pi", Some("\"pi\""), true)];
    let snapshot = snapshot(&[1], 10);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    assert!(runtime
        .records(PaneId::new(1))
        .unwrap()
        .records()
        .is_empty());
    collector.report(1, "", None, ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
}

#[test]
fn same_pid_different_birth_and_replacement_cannot_inherit_authority() {
    let (mut runtime, mut collector, manifests, original) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &original,
    );
    let owner = runtime
        .decision(PaneId::new(1))
        .unwrap()
        .selection
        .authority()
        .owner();
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    let replaced = snapshot(&[1], 11);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &replaced,
    );
    assert_eq!(state(&runtime, 1), None);
    assert_ne!(
        runtime.diagnostics(PaneId::new(1)).unwrap().owner,
        Some(owner)
    );
    collector.report(1, "", None, ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &replaced,
    );
    assert_eq!(state(&runtime, 1), None);
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &replaced,
    );
    assert!(
        runtime
            .decision(PaneId::new(1))
            .unwrap()
            .selection
            .result()
            .unwrap()
            .pending
    );
}

#[test]
fn before_after_process_conflict_discards_batch_while_other_panes_survive() {
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    let manifests = vec![manifest("pi", Some("\"pi\""), false)];
    let before = snapshot(&[1, 2], 10);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1), pane(2)],
        &manifests,
        &before,
    );
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    collector.report(2, "", Some("pi"), ProgramStatus::Working { progress: None });
    let after = ProcessSnapshot::from_samples([
        process(100, 1, "zsh", 1),
        process(101, 100, "node", 99),
        process(200, 1, "zsh", 1),
        process(201, 200, "node", 10),
    ]);
    let mut calls = 0;
    let batch = runtime.process_batch(
        &mut collector,
        &[pane(1), pane(2)],
        &manifests,
        &mut || {
            calls += 1;
            Ok(if calls == 1 {
                before.clone()
            } else {
                after.clone()
            })
        },
        1,
    );
    assert_eq!(calls, 2);
    assert_eq!(state(&runtime, 1), None);
    assert!(runtime
        .records(PaneId::new(1))
        .unwrap()
        .records()
        .is_empty());
    assert_eq!(state(&runtime, 2), Some(AgentState::Working));
    assert!(batch
        .releases
        .iter()
        .any(|release| release.pane == PaneId::new(1)
            && release.reason == RecoveryReason::ProcessChangedDuringBatch));
}

#[test]
fn fresh_binding_never_uses_a_legacy_pid_only_title_anchor() {
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    let manifests = vec![manifest("pi", Some("\"pi\""), true)];
    let mut facts = pane(1);
    facts.title = "tool output";
    let snapshot = snapshot(&[1], 10);
    run(
        &mut runtime,
        &mut collector,
        &[facts],
        &manifests,
        &snapshot,
    );
    assert_eq!(collector.boundaries, 0);
    assert_eq!(
        runtime.diagnostics(PaneId::new(1)).unwrap().coverage,
        CoverageState::Recovering(RecoveryReason::Unrecognized)
    );
}

#[test]
fn validated_birth_bound_title_anchor_survives_flicker_and_temporary_pager() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let mut pager = pane(1);
    pager.current_command = "less";
    pager.title = "log";
    run(
        &mut runtime,
        &mut collector,
        &[pager],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
    assert_eq!(collector.boundaries, 1);
}

#[test]
fn ignore_remote_nested_pidless_and_unknown_agents_cannot_bind_via_aliases() {
    let registration = Registration {
        agent_name: "pi".into(),
        session: Some("session".into()),
    };
    let manifests = vec![manifest("pi", Some("\"pi\""), false)];
    for command in ["ssh", "mosh", "docker", "tmux", "zellij", "screen"] {
        let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
        let mut facts = pane(1);
        facts.current_command = command;
        facts.registration = Some(&registration);
        run(
            &mut runtime,
            &mut collector,
            &[facts],
            &manifests,
            &snapshot(&[1], 10),
        );
        assert_eq!(collector.boundaries, 0, "{command}");
    }
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    let mut ignored = pane(1);
    ignored.ignored = true;
    ignored.registration = Some(&registration);
    run(
        &mut runtime,
        &mut collector,
        &[ignored],
        &manifests,
        &snapshot(&[1], 10),
    );
    assert_eq!(collector.boundaries, 0);
    let only_shell = ProcessSnapshot::from_samples([process(100, 1, "zsh", 1)]);
    let mut pidless = pane(1);
    pidless.registration = Some(&registration);
    run(
        &mut runtime,
        &mut collector,
        &[pidless],
        &manifests,
        &only_shell,
    );
    assert_eq!(collector.boundaries, 0);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &[],
        &snapshot(&[1], 10),
    );
    assert_eq!(collector.boundaries, 0);
}

#[test]
fn missing_birth_and_enumeration_failure_remain_typed_fallback() {
    let manifests = vec![manifest("pi", Some("\"pi\""), false)];
    for reason in [
        ProcessUnavailable::NotFound,
        ProcessUnavailable::PermissionDenied,
        ProcessUnavailable::Unsupported,
        ProcessUnavailable::Malformed,
        ProcessUnavailable::ReadFailed,
    ] {
        let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
        let snapshot = ProcessSnapshot::from_samples([(100, Err(reason))]);
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        assert_eq!(
            runtime.diagnostics(PaneId::new(1)).unwrap().coverage,
            CoverageState::Recovering(RecoveryReason::ProcessUnavailable(reason))
        );
        assert_eq!(collector.boundaries, 0);
        runtime.process_batch(
            &mut collector,
            &[pane(1)],
            &manifests,
            &mut || Err(reason),
            0,
        );
        assert_eq!(
            runtime.diagnostics(PaneId::new(1)).unwrap().coverage,
            CoverageState::Recovering(RecoveryReason::ProcessUnavailable(reason))
        );
    }
}

#[test]
fn aliases_are_optional_and_alias_change_discards_queued_old_interval() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    collector.report(1, "", Some("pi2"), ProgramStatus::Done);
    let changed = vec![manifest("pi", Some("\"pi2\""), true)];
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &changed,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    assert_eq!(collector.boundaries, 2);
    let absent = vec![manifest("pi", None, true)];
    run(&mut runtime, &mut collector, &[pane(1)], &absent, &snapshot);
    assert_eq!(
        runtime.diagnostics(PaneId::new(1)).unwrap().coverage,
        CoverageState::Recovering(RecoveryReason::NoAliases)
    );
}

#[test]
fn anonymous_lineage_is_adopted_live_but_foreign_replacement_revokes_inheritance() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(
        1,
        "child",
        None,
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    collector.report(1, "", Some("pi"), ProgramStatus::Idle);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Blocked));
    collector.report(
        1,
        "explicit",
        Some("pi"),
        ProgramStatus::Working { progress: None },
    );
    collector.report(1, "", Some("tool"), ProgramStatus::Idle);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
    assert_eq!(
        runtime
            .decision(PaneId::new(1))
            .unwrap()
            .selection
            .record_id()
            .as_ref()
            .as_str(),
        "explicit"
    );
    collector.emit(
        1,
        ProgramEvent::Command(ProgramCommand::Clear(
            tma_core::program_status::RecordId::parse("explicit").unwrap(),
        )),
    );
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
}

#[test]
fn every_gap_requires_fresh_matching_report_and_cannot_adopt_pre_gap_children() {
    for reason in [
        ObservationGap::Disconnected,
        ObservationGap::OwnerChanged,
        ObservationGap::Paused,
        ObservationGap::MalformedTransport,
        ObservationGap::FramingLoss,
        ObservationGap::QueueOverflow,
        ObservationGap::BudgetExceeded,
    ] {
        let (mut runtime, mut collector, manifests, snapshot) = setup();
        collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
        collector.report(
            1,
            "child",
            None,
            ProgramStatus::Blocked {
                kind: None,
                progress: None,
            },
        );
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        collector.gap(1, reason);
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        assert_eq!(state(&runtime, 1), None, "{reason:?}");
        collector.report(1, "", Some("pi"), ProgramStatus::Idle);
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        assert_eq!(state(&runtime, 1), Some(AgentState::Idle), "{reason:?}");
    }
}

#[test]
fn history_and_live_authority_have_distinct_lifetimes() {
    for event in [
        TerminalLifecycle::ShellPrompt,
        TerminalLifecycle::AttachedProcessExit,
        TerminalLifecycle::FullReset,
    ] {
        let (mut runtime, mut collector, manifests, snapshot) = setup();
        collector.report(1, "done", Some("pi"), ProgramStatus::Done);
        collector.report(1, "error", Some("pi"), ProgramStatus::Error);
        collector.report(
            1,
            "working",
            Some("pi"),
            ProgramStatus::Working { progress: None },
        );
        collector.report(1, "idle", Some("pi"), ProgramStatus::Idle);
        collector.emit(1, ProgramEvent::Lifecycle(event));
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        assert_eq!(state(&runtime, 1), None);
        let history = runtime.records(PaneId::new(1)).unwrap().records();
        assert_eq!(
            history.len(),
            if event == TerminalLifecycle::FullReset {
                0
            } else {
                2
            }
        );
        assert!(history.iter().all(|entry| entry.is_historical()));
    }
}

#[test]
fn agent_departure_invalidates_row_even_while_shell_remains() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    collector.report(
        1,
        "working",
        Some("pi"),
        ProgramStatus::Working { progress: None },
    );
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let shell = ProcessSnapshot::from_samples([process(100, 1, "zsh", 1)]);
    run(&mut runtime, &mut collector, &[pane(1)], &manifests, &shell);
    assert_eq!(state(&runtime, 1), None);
    assert_eq!(runtime.records(PaneId::new(1)).unwrap().records().len(), 1);
}

#[test]
fn silence_has_no_report_expiry_and_scope_loss_requires_new_boundary() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    runtime.process_batch(
        &mut collector,
        &[pane(1)],
        &manifests,
        &mut || Ok(snapshot.clone()),
        u64::MAX,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
    assert_eq!(
        runtime.diagnostics(PaneId::new(1)).unwrap().last_report,
        Some(20)
    );
    collector.global_gap = true;
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), None);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
}

#[test]
fn publication_recheck_detects_reused_intermediate_pid_and_fresh_exclusions() {
    let (mut runtime, mut collector) = (ProgramRuntime::new(), Collector::default());
    let manifests = vec![manifest("pi", Some("\"pi\""), true)];
    let snapshot = ProcessSnapshot::from_samples([
        process(100, 1, "zsh", 1),
        process(105, 100, "sh", 5),
        process(101, 105, "node", 10),
    ]);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let decision = runtime.decision(PaneId::new(1)).unwrap();
    assert_eq!(
        decision
            .process_chain
            .iter()
            .map(|identity| identity.pid)
            .collect::<Vec<_>>(),
        [100, 105, 101]
    );
    assert!(runtime
        .recheck(&mut collector, &decision, &pane(1), &manifests, &snapshot)
        .is_ok());
    let changed_wrapper = ProcessSnapshot::from_samples([
        process(100, 1, "zsh", 1),
        process(105, 100, "sh", 99),
        process(101, 105, "node", 10),
    ]);
    assert_eq!(snapshot.identity(100), changed_wrapper.identity(100));
    assert_eq!(snapshot.identity(101), changed_wrapper.identity(101));
    assert_ne!(snapshot.identity(105), changed_wrapper.identity(105));
    assert!(runtime
        .recheck(
            &mut collector,
            &decision,
            &pane(1),
            &manifests,
            &changed_wrapper
        )
        .is_err());
    assert_eq!(state(&runtime, 1), None);
    for excluded in ["ignore", "ssh", "tmux"] {
        let (mut runtime, mut collector, manifests, snapshot) = setup();
        collector.report(1, "", Some("pi"), ProgramStatus::Done);
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        let decision = runtime.decision(PaneId::new(1)).unwrap();
        let mut facts = pane(1);
        facts.ignored = excluded == "ignore";
        if excluded != "ignore" {
            facts.current_command = excluded;
        }
        assert!(runtime
            .recheck(&mut collector, &decision, &facts, &manifests, &snapshot)
            .is_err());
        assert_eq!(state(&runtime, 1), None);
    }
}

#[test]
fn only_published_exact_results_can_be_acknowledged_and_recovery_is_explicit() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "child", Some("pi"), ProgramStatus::Done);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert!(runtime
        .decision(PaneId::new(1))
        .unwrap()
        .selection
        .result()
        .is_none());
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let decision = runtime.decision(PaneId::new(1)).unwrap();
    let result = decision.selection.result().unwrap().identity.clone();
    assert!(!runtime.acknowledge(PaneId::new(1), &result));
    assert!(runtime.mark_presented(&decision));
    assert!(runtime.acknowledge(PaneId::new(1), &result));
    collector.gap(1, ObservationGap::Disconnected);
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let recovered = runtime.decision(PaneId::new(1)).unwrap();
    assert!(recovered.selection.result().unwrap().pending);
    assert!(
        recovered
            .previous_presentation
            .as_ref()
            .unwrap()
            .acknowledged
    );
    assert!(runtime.reconcile_result(&recovered, ResultDisposition::Acknowledged));
    assert!(runtime
        .decision(PaneId::new(1))
        .unwrap()
        .selection
        .result()
        .is_none());
}

#[test]
fn exact_replacement_eviction_clear_history_removal_and_disable_account_payload() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    for index in 0..300 {
        collector.report(1, &format!("task{index}"), Some("pi"), ProgramStatus::Done);
    }
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(
        runtime.records(PaneId::new(1)).unwrap().records().len(),
        256
    );
    assert_eq!(
        runtime.retained_bytes(),
        runtime.records(PaneId::new(1)).unwrap().retained_bytes()
    );
    let old = runtime.retained_bytes();
    collector.report(1, "task299", Some("pi"), ProgramStatus::Error);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(runtime.retained_bytes(), old);
    collector.emit(1, ProgramEvent::Lifecycle(TerminalLifecycle::ShellPrompt));
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(runtime.retained_bytes(), old);
    collector.report(1, "", Some("pi"), ProgramStatus::Idle);
    collector.emit(
        1,
        ProgramEvent::Command(ProgramCommand::Clear(
            tma_core::program_status::RecordId::root(),
        )),
    );
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(runtime.retained_bytes(), 0);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    runtime.reset(&mut collector);
    assert_eq!(runtime.retained_bytes(), 0);
    assert!(runtime.diagnostics(PaneId::new(1)).is_none());
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    run(&mut runtime, &mut collector, &[], &manifests, &snapshot);
    assert_eq!(runtime.retained_bytes(), 0);
}

#[test]
fn budget_failure_revokes_whole_affected_pane_without_dropping_healthy_authority() {
    let mut runtime = ProgramRuntime::new();
    let mut collector = Collector::default();
    let manifests = vec![manifest("pi", Some("\"pi\""), false)];
    let ids: Vec<_> = (1..=70).collect();
    let panes: Vec<_> = ids.iter().map(|id| pane(*id)).collect();
    let snapshot = snapshot(&ids, 10);
    run(&mut runtime, &mut collector, &panes, &manifests, &snapshot);
    let message = "m".repeat(2048);
    for id in &ids {
        for index in 0..256 {
            let record_id = format!("task{index}");
            let mut record = RecordRef::new(
                RecordIdRef::parse(&record_id).unwrap(),
                ProgramStatus::Working { progress: None },
            );
            record.app = Some(AppIdRef::parse("pi").unwrap());
            record.msg = Some(SingleLineRef::parse(&message).unwrap());
            collector.emit(
                *id,
                ProgramEvent::Command(ProgramCommand::Replace(record.into_owned())),
            );
        }
        let result = run(&mut runtime, &mut collector, &panes, &manifests, &snapshot);
        assert!(runtime.retained_bytes() <= MAX_PROGRAM_BYTES);
        if result.releases.iter().any(|release| {
            release.reason == RecoveryReason::Observation(ObservationGap::BudgetExceeded)
        }) {
            assert_eq!(state(&runtime, *id), None);
            assert_eq!(
                runtime.records(PaneId::new(*id)).unwrap().retained_bytes(),
                0
            );
            assert_eq!(state(&runtime, 1), Some(AgentState::Working));
            assert_eq!(
                collector.budget,
                MAX_PROGRAM_BYTES - runtime.retained_bytes()
            );
            return;
        }
    }
    panic!("the fleet must reach its shared payload budget");
}

#[test]
fn ordered_consumer_preserves_done_work_clear_and_lifecycle_before_coalescing() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    collector.report(
        1,
        "",
        Some("pi"),
        ProgramStatus::Blocked {
            kind: Some(tma_core::program_status::BlockedKind::Permission),
            progress: None,
        },
    );
    collector.emit(
        1,
        ProgramEvent::Command(ProgramCommand::Clear(
            tma_core::program_status::RecordId::root(),
        )),
    );
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    collector.emit(1, ProgramEvent::Lifecycle(TerminalLifecycle::ShellPrompt));
    let mut steps = Vec::new();
    let mut samples = 0;
    runtime.process_batch_with(
        &mut collector,
        &[pane(1)],
        &manifests,
        &mut || {
            samples += 1;
            Ok(snapshot.clone())
        },
        30,
        |runtime, collector, step| {
            steps.push((
                step.kind,
                step.decision
                    .as_ref()
                    .map(|decision| decision.selection.state()),
            ));
            if let Some(decision) = step.decision.as_ref().filter(|decision| {
                decision
                    .selection
                    .result()
                    .is_some_and(|result| result.pending)
            }) {
                assert!(runtime
                    .recheck(
                        collector,
                        decision,
                        &pane(1),
                        &manifests,
                        step.checked_processes
                    )
                    .is_ok());
                assert!(runtime.mark_presented(decision));
            }
        },
    );
    assert_eq!(samples, 2);
    assert_eq!(
        steps,
        [
            (ProgramStepKind::Replace, Some(AgentState::Idle)),
            (ProgramStepKind::Replace, Some(AgentState::Working)),
            (ProgramStepKind::Replace, Some(AgentState::Blocked)),
            (ProgramStepKind::Clear, None),
            (ProgramStepKind::Replace, Some(AgentState::Idle)),
            (
                ProgramStepKind::Lifecycle(TerminalLifecycle::ShellPrompt),
                None
            ),
        ]
    );
    assert_eq!(state(&runtime, 1), None);
}

#[test]
fn stale_publication_rejects_without_revoking_newer_owner_or_selection() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let old = runtime.decision(PaneId::new(1)).unwrap();
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(
        runtime.recheck(&mut collector, &old, &pane(1), &manifests, &snapshot),
        Err(ProgramRecheckError::StaleDecision)
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
    collector.gap(1, ObservationGap::OwnerChanged);
    collector.report(1, "", Some("pi"), ProgramStatus::Idle);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    assert_eq!(
        runtime.recheck(&mut collector, &old, &pane(1), &manifests, &snapshot),
        Err(ProgramRecheckError::StaleDecision)
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Idle));
}

#[test]
fn delayed_publication_rejects_a_collector_gap_before_the_next_batch() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let decision = runtime.decision(PaneId::new(1)).unwrap();
    collector.gap(1, ObservationGap::Disconnected);
    assert!(matches!(
        runtime.recheck(&mut collector, &decision, &pane(1), &manifests, &snapshot),
        Err(ProgramRecheckError::Revoked(_))
    ));
    assert_eq!(state(&runtime, 1), None);
}

#[test]
fn publication_recheck_allows_queued_updates_but_rejects_missing_coverage() {
    for loss in [false, true] {
        let (mut runtime, mut collector, manifests, snapshot) = setup();
        collector.report(1, "", Some("pi"), ProgramStatus::Done);
        run(
            &mut runtime,
            &mut collector,
            &[pane(1)],
            &manifests,
            &snapshot,
        );
        let decision = runtime.decision(PaneId::new(1)).unwrap();
        collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
        assert!(runtime
            .recheck(&mut collector, &decision, &pane(1), &manifests, &snapshot)
            .is_ok());
        if loss {
            collector.global_gap = true;
        } else {
            collector.epochs.remove(&PaneId::new(1));
        }
        assert!(matches!(
            runtime.recheck(&mut collector, &decision, &pane(1), &manifests, &snapshot),
            Err(ProgramRecheckError::Revoked(_))
        ));
        assert_eq!(state(&runtime, 1), None);
    }
}

#[test]
fn callback_invalidation_discards_later_events_and_delivers_its_release_once() {
    for invalidate_runtime in [false, true] {
        let (mut runtime, mut collector, manifests, snapshot) = setup();
        collector.report(1, "", Some("pi"), ProgramStatus::Done);
        collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
        let mut seen = Vec::new();
        let mut releases = Vec::new();
        let batch = runtime.process_batch_with(
            &mut collector,
            &[pane(1)],
            &manifests,
            &mut || Ok(snapshot.clone()),
            30,
            |runtime, collector, step| {
                seen.push(step.kind);
                releases.extend_from_slice(step.releases);
                if let Some(decision) = step.decision {
                    assert_eq!(decision.selection.protocol_state(), ProgramStatus::Done);
                    if invalidate_runtime {
                        let mut facts = pane(1);
                        facts.ignored = true;
                        let Err(ProgramRecheckError::Revoked(release)) = runtime.recheck(
                            collector,
                            &decision,
                            &facts,
                            &manifests,
                            step.checked_processes,
                        ) else {
                            panic!("fresh exclusion must revoke this episode");
                        };
                        releases.push(release);
                    } else {
                        collector.invalidate(PaneId::new(1), ObservationGap::Disconnected);
                    }
                }
            },
        );
        assert_eq!(batch.applied_events, 1);
        assert_eq!(
            seen,
            if invalidate_runtime {
                vec![ProgramStepKind::Replace]
            } else {
                vec![ProgramStepKind::Replace, ProgramStepKind::Release]
            }
        );
        assert_eq!(releases.len(), 1);
        assert!(batch.unstreamed_releases().is_empty());
        assert_eq!(state(&runtime, 1), None);
    }
}

#[test]
fn a_binding_boundary_cannot_hide_a_budget_eviction() {
    let (mut runtime, mut collector, manifests, original) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &original,
    );
    assert!(runtime.retained_bytes() > 0);
    collector.gap(1, ObservationGap::BudgetExceeded);
    let replacement = snapshot(&[1], 11);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &replacement,
    );
    assert_eq!(runtime.retained_bytes(), 0);
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &replacement,
    );
    assert_eq!(state(&runtime, 1), Some(AgentState::Working));
}

#[test]
fn budget_gap_evicts_history_even_without_a_live_binding() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let mut ignored = pane(1);
    ignored.ignored = true;
    run(
        &mut runtime,
        &mut collector,
        &[ignored],
        &manifests,
        &snapshot,
    );
    assert!(runtime.retained_bytes() > 0);
    collector.gap(1, ObservationGap::BudgetExceeded);
    let mut ignored = pane(1);
    ignored.ignored = true;
    run(
        &mut runtime,
        &mut collector,
        &[ignored],
        &manifests,
        &snapshot,
    );
    assert_eq!(runtime.retained_bytes(), 0);
}

#[test]
fn failed_process_sample_does_not_discard_a_required_budget_eviction() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Done);
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    collector.gap(1, ObservationGap::BudgetExceeded);
    let batch = runtime.process_batch_with(
        &mut collector,
        &[pane(1)],
        &manifests,
        &mut || Err(ProcessUnavailable::ReadFailed),
        30,
        |_, _, _| panic!("failed process sample cannot certify publication"),
    );
    assert_eq!(runtime.retained_bytes(), 0);
    assert_eq!(batch.unstreamed_releases().len(), 1);
    assert_eq!(state(&runtime, 1), None);
}

#[test]
fn native_collector_budget_gap_evicts_retained_payload_and_recovers_with_healthy_peer() {
    use std::time::Instant;
    use tma_test_support as common;
    use tma_tmux::{control, tmux};
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        return;
    }
    let scratch = common::Scratch::new("program_budget");
    for name in ["reporter", "healthy"] {
        assert!(scratch
            .tmux(&["new-session", "-d", "-s", name, "stty -echo; exec /bin/cat"])
            .status
            .success());
    }
    let reporter = scratch.get("reporter", "#{pane_id}");
    let healthy = scratch.get("healthy", "#{pane_id}");
    let reporter_id = PaneId::parse(&reporter).unwrap();
    let healthy_id = PaneId::parse(&healthy).unwrap();
    let reporter_pid = scratch.get(&reporter, "#{pane_pid}").parse().unwrap();
    let healthy_pid = scratch.get(&healthy, "#{pane_pid}").parse().unwrap();
    let tmux = tmux::Tmux::connect(&tmux::Server::named(Some(scratch.socket.clone())));
    let mut pool = ControlPool::default();
    pool.set_program_collection(true);
    pool.reconcile(&tmux).unwrap();
    let mut runtime = ProgramRuntime::new();
    let pump = |runtime: &ProgramRuntime, pool: &mut ControlPool| {
        runtime.prepare_collection(pool);
        let ready = control::pollfds(pool)
            .into_iter()
            .map(|(session, _)| session)
            .collect();
        if control::dispatch_ready(pool, &ready, Instant::now()).need_reconcile {
            pool.reconcile(&tmux).unwrap();
        }
        control::seed_attached(pool, &tmux, Instant::now());
    };
    common::poll_until("both native cat reporters attach", || {
        pump(&runtime, &mut pool);
        !pool.pending_seed()
            && scratch.get(&reporter, "#{pane_current_command}") == "cat"
            && scratch.get(&healthy, "#{pane_current_command}") == "cat"
    });
    let manifest = Manifest::parse("min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"cat\"]\n[capture]\nvisible = [\"working\"]\n[program_status]\napps = [\"pi\"]\n", "synthetic-cat.toml").unwrap();
    let engine = RuleEngine::build(&manifest).unwrap();
    let manifests = [LoadedManifest {
        name: "cat".into(),
        manifest,
        engine,
    }];
    let mut facts = vec![
        ProgramPane {
            pane: reporter_id,
            pane_pid: reporter_pid,
            current_command: "cat",
            title: "",
            ignored: false,
            registration: None,
        },
        ProgramPane {
            pane: healthy_id,
            pane_pid: healthy_pid,
            current_command: "cat",
            title: "",
            ignored: false,
            registration: None,
        },
    ];
    runtime.process_batch(
        &mut pool,
        &facts,
        &manifests,
        &mut tmux::program_processes,
        1,
    );
    let send = |pane: &str, payload: &str| {
        tmux.send_text(pane, &[], payload, &["Enter".into()])
            .unwrap();
    };
    for pane in [&reporter, &healthy] {
        send(pane, "framing\x1b\\\x1b]7501;state=working:app=pi\x1b\\");
    }
    common::poll_until("both native episodes become authoritative", || {
        pump(&runtime, &mut pool);
        runtime.process_batch(
            &mut pool,
            &facts,
            &manifests,
            &mut tmux::program_processes,
            2,
        );
        runtime.decision(reporter_id).is_some() && runtime.decision(healthy_id).is_some()
    });
    // Fixture setup fills genuine bounded RecordStores with history. Cloning a
    // completed template avoids repeatedly exercising the reducer's lineage walk.
    // Filler panes have controlled facts and no stream; the two native streams
    // above exercise production admission, out-of-band loss, eviction and recovery.
    let message = "m".repeat(2048);
    let mut template = RecordStore::new();
    for index in 0..256 {
        let id = format!("r{index:03}");
        let mut record = RecordRef::new(RecordIdRef::parse(&id).unwrap(), ProgramStatus::Done);
        record.msg = Some(SingleLineRef::parse(&message).unwrap());
        template.apply(ProgramCommandRef::Replace(record), 1, None);
    }
    template.invalidate_episode();
    let target = MAX_PROGRAM_BYTES - 8;
    let mut filler = 1000;
    while runtime.retained_bytes + template.retained_bytes() <= target {
        runtime.panes.insert(
            PaneId::new(filler),
            PaneProgram {
                store: template.clone(),
                ..PaneProgram::default()
            },
        );
        runtime.retained_bytes += template.retained_bytes();
        facts.push(ProgramPane {
            pane: PaneId::new(filler),
            pane_pid: reporter_pid,
            current_command: "cat",
            title: "",
            ignored: false,
            registration: None,
        });
        filler += 1;
    }
    let mut tail = RecordStore::new();
    while runtime.retained_bytes + tail.retained_bytes() < target {
        let remaining = target - runtime.retained_bytes - tail.retained_bytes();
        let id = format!("r{:03}", tail.records().len());
        let bytes = (remaining - id.len()).min(2048);
        let mut record = RecordRef::new(RecordIdRef::parse(&id).unwrap(), ProgramStatus::Done);
        record.msg = Some(SingleLineRef::parse(&message[..bytes]).unwrap());
        tail.apply(ProgramCommandRef::Replace(record), 1, None);
    }
    tail.invalidate_episode();
    runtime.retained_bytes += tail.retained_bytes();
    runtime.panes.insert(
        PaneId::new(filler),
        PaneProgram {
            store: tail,
            ..PaneProgram::default()
        },
    );
    facts.push(ProgramPane {
        pane: PaneId::new(filler),
        pane_pid: reporter_pid,
        current_command: "cat",
        title: "",
        ignored: false,
        registration: None,
    });
    assert_eq!(runtime.retained_bytes, target);
    // Give the affected native pane enough retained payload to free frame-sized
    // headroom after its budget loss. Move bytes from historical fixture storage.
    let mut record = RecordRef::new(RecordIdRef::ROOT, ProgramStatus::Working { progress: None });
    record.app = Some(AppIdRef::parse("pi").unwrap());
    record.msg = Some(SingleLineRef::parse(&message).unwrap());
    let entry = runtime.panes.get_mut(&reporter_id).unwrap();
    let binding = entry.binding.as_ref().unwrap();
    entry.store.apply(
        ProgramCommandRef::Replace(record),
        2,
        Some(LiveEpisode {
            owner: binding.owner,
            apps: &binding.identity.apps,
        }),
    );
    // Remove precisely 2048 fixture bytes so the combined retained payload stays
    // at the same near-limit value, rather than faking the accounting counter.
    let entry = runtime.panes.get_mut(&PaneId::new(1000)).unwrap();
    let first = entry.store.records()[0].record().as_ref();
    let smaller = RecordRef::new(first.id, first.status).into_owned();
    entry
        .store
        .apply(ProgramCommandRef::Replace(smaller.as_ref()), 2, None);
    assert_eq!(
        runtime
            .panes
            .values()
            .map(|entry| entry.store.retained_bytes())
            .sum::<usize>(),
        target
    );
    send(&reporter, "\x1b]7501;state=working:app=pi\x1b\\");
    let mut budget_release = false;
    common::poll_until("native budget loss evicts whole affected pane", || {
        pump(&runtime, &mut pool);
        let batch = runtime.process_batch(
            &mut pool,
            &facts,
            &manifests,
            &mut tmux::program_processes,
            3,
        );
        budget_release |= batch.releases.iter().any(|release| {
            release.pane == reporter_id
                && release.reason == RecoveryReason::Observation(ObservationGap::BudgetExceeded)
        });
        budget_release
    });
    assert!(runtime.decision(reporter_id).is_none());
    assert_eq!(runtime.records(reporter_id).unwrap().retained_bytes(), 0);
    assert!(runtime.decision(healthy_id).is_some());
    assert!(MAX_PROGRAM_BYTES - runtime.retained_bytes() >= 2058);
    send(&reporter, "\x1b]7501;state=working:app=pi\x1b\\");
    common::poll_until(
        "fresh native report recovers with reclaimed headroom",
        || {
            pump(&runtime, &mut pool);
            runtime.process_batch(
                &mut pool,
                &facts,
                &manifests,
                &mut tmux::program_processes,
                4,
            );
            runtime.decision(reporter_id).is_some()
        },
    );
    assert!(runtime.decision(healthy_id).is_some());
    assert!(runtime.retained_bytes() + pool.pending_program_bytes() <= MAX_PROGRAM_BYTES);
}

#[test]
fn callback_release_tail_is_exact_for_validated_failed_and_empty_batches() {
    let (mut runtime, mut collector, manifests, snapshot) = setup();
    collector.report(1, "", Some("pi"), ProgramStatus::Working { progress: None });
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    collector.gap(1, ObservationGap::Disconnected);
    let mut seen = 0;
    let batch = runtime.process_batch_with(
        &mut collector,
        &[pane(1)],
        &manifests,
        &mut || Ok(snapshot.clone()),
        30,
        |_, _, step| seen += step.releases.len(),
    );
    assert_eq!(seen, batch.releases.len());
    assert!(batch.unstreamed_releases().is_empty());
    for failure_at in [1, 2] {
        let mut calls = 0;
        let batch = runtime.process_batch_with(
            &mut collector,
            &[pane(1)],
            &manifests,
            &mut || {
                calls += 1;
                if calls == failure_at {
                    Err(ProcessUnavailable::ReadFailed)
                } else {
                    Ok(snapshot.clone())
                }
            },
            40,
            |_, _, _| panic!("unavailable snapshot cannot certify a callback"),
        );
        assert!(!batch.unstreamed_releases().is_empty());
        assert_eq!(batch.unstreamed_releases(), batch.releases);
    }
    run(
        &mut runtime,
        &mut collector,
        &[pane(1)],
        &manifests,
        &snapshot,
    );
    let batch = runtime.process_batch_with(
        &mut collector,
        &[],
        &manifests,
        &mut || panic!("no pane needs a process sample"),
        50,
        |_, _, _| panic!("empty batch has no checked process snapshot"),
    );
    assert_eq!(batch.unstreamed_releases().len(), 1);
    assert_eq!(
        batch.unstreamed_releases()[0].reason,
        RecoveryReason::PaneRemoved
    );
}
