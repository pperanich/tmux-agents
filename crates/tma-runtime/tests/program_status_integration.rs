//! Production collector/runtime boundary evidence with native process birth.
//! `cat` is an independently recognized synthetic reporter, not a live agent.
use std::time::Instant;
use tma_core::program_status::{ObservationGap, PaneId, ProgramStatus};
use tma_core::{Manifest, RuleEngine};
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::program_status::{
    ProgramPane, ProgramRecheckError, ProgramRuntime, RecoveryReason,
};
use tma_test_support as common;
use tma_tmux::control::{self, ControlPool};
use tma_tmux::tmux::{program_processes, Server, Tmux};

fn pump(runtime: &ProgramRuntime, pool: &mut ControlPool, tmux: &Tmux) {
    runtime.prepare_collection(pool);
    let ready = control::pollfds(pool)
        .into_iter()
        .map(|(session, _)| session)
        .collect();
    if control::dispatch_ready(pool, &ready, Instant::now()).need_reconcile {
        pool.reconcile(tmux).unwrap();
    }
    control::seed_attached(pool, tmux, Instant::now());
}
fn send(scratch: &common::Scratch, pane: &str, text: &str) {
    assert!(scratch
        .tmux(&["send-keys", "-t", pane, "-l", text])
        .status
        .success());
    assert!(scratch
        .tmux(&["send-keys", "-t", pane, "Enter"])
        .status
        .success());
}

#[test]
fn native_runtime_cold_bind_discards_queued_and_split_candidates_then_recovers() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        return;
    }
    let scratch = common::Scratch::new("program_runtime");
    assert!(scratch
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "reporter",
            "stty -echo; exec /bin/cat"
        ])
        .status
        .success());
    let pane = scratch.get("reporter", "#{pane_id}");
    let pane_pid = scratch.get(&pane, "#{pane_pid}").parse().unwrap();
    let pane_id = PaneId::parse(&pane).unwrap();
    let tmux = Tmux::connect(&Server::named(Some(scratch.socket.clone())));
    let mut pool = ControlPool::default();
    pool.set_program_collection(true);
    pool.reconcile(&tmux).unwrap();
    let mut runtime = ProgramRuntime::new();
    common::poll_until("native cat and collector attach", || {
        pump(&runtime, &mut pool, &tmux);
        !pool.pending_seed() && scratch.get(&pane, "#{pane_current_command}") == "cat"
    });
    let manifest = Manifest::parse("min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"cat\"]\n[capture]\nvisible = [\"working\"]\n[program_status]\napps = [\"pi\"]\n", "synthetic-cat.toml").unwrap();
    let engine = RuleEngine::build(&manifest).unwrap();
    let manifests = [LoadedManifest {
        name: "reporter".into(),
        manifest,
        engine,
    }];
    let facts = [ProgramPane {
        pane: pane_id,
        pane_pid,
        current_command: "cat",
        title: "",
        ignored: false,
        registration: None,
    }];
    // Synchronization is synthetic framing setup. A later complete report is queued
    // BEFORE independent process binding and must never establish authority.
    send(
        &scratch,
        &pane,
        "framing\x1b\\\x1b]7501;state=done:app=pi\x1b\\",
    );
    common::poll_until("prebind complete report queues", || {
        pump(&runtime, &mut pool, &tmux);
        pool.program_stats().unwrap().queued_events > 0
    });
    runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 1);
    assert!(runtime.decision(pane_id).is_none());
    assert!(runtime.records(pane_id).unwrap().records().is_empty());
    // Start a report, change the runtime interval while it is partial, then
    // finish it. The production program_boundary quarantines that candidate.
    send(&scratch, &pane, "\x1b]7501;state=working:app=");
    common::poll_until("partial report is held", || {
        pump(&runtime, &mut pool, &tmux);
        pool.pending_program_bytes() > 0
    });
    runtime.reset(&mut pool);
    runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 2);
    send(&scratch, &pane, "pi\x1b\\");
    for _ in 0..5 {
        pump(&runtime, &mut pool, &tmux);
    }
    runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 3);
    assert!(runtime.decision(pane_id).is_none());
    send(&scratch, &pane, "\x1b]7501;state=working:app=pi\x1b\\");
    common::poll_until("fresh complete report establishes native episode", || {
        pump(&runtime, &mut pool, &tmux);
        runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 4);
        runtime.decision(pane_id).is_some()
    });
    let decision = runtime.decision(pane_id).unwrap();
    assert!(matches!(
        decision.selection.protocol_state(),
        ProgramStatus::Working { .. }
    ));
    runtime
        .recheck(
            &mut pool,
            &decision,
            &facts[0],
            &manifests,
            &program_processes().unwrap(),
        )
        .unwrap();
    // A known collector loss can arrive after reduction and before delayed
    // publication. Fresh process facts alone must not authorize the old stream.
    pool.invalidate_program(pane_id, ObservationGap::Disconnected);
    assert!(matches!(
        runtime.recheck(
            &mut pool,
            &decision,
            &facts[0],
            &manifests,
            &program_processes().unwrap(),
        ),
        Err(ProgramRecheckError::Revoked(release))
            if release.reason == RecoveryReason::Observation(ObservationGap::Disconnected)
    ));
    assert!(runtime.decision(pane_id).is_none());
    runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 5);
    assert!(runtime.decision(pane_id).is_none());
    send(&scratch, &pane, "\x1b]7501;state=working:app=pi\x1b\\");
    common::poll_until("fresh report recovers the native stream loss", || {
        pump(&runtime, &mut pool, &tmux);
        runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 6);
        runtime.decision(pane_id).is_some()
    });
    // Real local process departure invalidates the runtime even while tmux has
    // not yet reconciled the dead stream. No persisted row is written here.
    assert!(scratch.tmux(&["kill-pane", "-t", &pane]).status.success());
    runtime.process_batch(&mut pool, &facts, &manifests, &mut program_processes, 7);
    assert!(runtime.decision(pane_id).is_none());
}
