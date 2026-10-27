//! Native A5/A9/A12 adoption evidence through production writers and consumers.
use tma_core::persistence::{self as model, MetadataEvidence, Token};
use tma_core::program_status::{
    AppIdRef, ConnectionEpoch, ObservationGap, ObservationSequence, ObservedEvent, PaneId,
    ProgramCommand, ProgramEvent, ProgramStatus, RecordIdRef, RecordRef, SessionId,
};
use tma_core::render::{Guard, Publish};
use tma_core::stamp::opt;
use tma_core::{AgentState, FoldConfig, Manifest, Provenance, RuleEngine};
use tma_runtime::event::{apply_event, NotifyPolicy};
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::program_status::{ProgramCollector, ProgramPane, ProgramRuntime};
use tma_test_support as common;
use tma_tmux::control::{ObservationBoundary, ProgramDrain, ProgramStreamState};
use tma_tmux::persistence::{self as store, AttentionObservation, OptionEdit};
use tma_tmux::stamp::{self, StampPlan};
use tma_tmux::tmux::{program_processes, Server, Tmux};

#[derive(Default)]
struct Collector {
    epoch: u64,
    sequence: u64,
    pane: Option<PaneId>,
    events: Vec<ObservedEvent>,
    gaps: Vec<ObservedEvent>,
}
impl Collector {
    fn report(&mut self, id: &str, status: ProgramStatus) {
        self.sequence += 1;
        let mut record = RecordRef::new(
            if id.is_empty() {
                RecordIdRef::ROOT
            } else {
                RecordIdRef::parse(id).unwrap()
            },
            status,
        );
        record.app = Some(AppIdRef::parse("pi").unwrap());
        if matches!(status, ProgramStatus::Blocked { .. }) {
            record = record
                .with_title(Some(
                    tma_core::program_status::SingleLineRef::parse("approve_from_protocol_text")
                        .unwrap(),
                ))
                .unwrap();
            record.msg = Some(
                tma_core::program_status::SingleLineRef::parse(
                    "request_id=approve_from_protocol_text tool_name=Bash allow once",
                )
                .unwrap(),
            );
        }
        self.events.push(ObservedEvent {
            pane: self.pane.unwrap(),
            connection: ConnectionEpoch::new(self.epoch),
            sequence: ObservationSequence::new(self.sequence),
            event: ProgramEvent::Command(ProgramCommand::Replace(record.into_owned())),
        });
    }
}
impl ProgramCollector for Collector {
    fn boundary(&mut self, pane: PaneId) -> Option<ObservationBoundary> {
        self.epoch += 1;
        self.pane = Some(pane);
        self.events.clear();
        Some(ObservationBoundary {
            connection: ConnectionEpoch::new(self.epoch),
            sequence_floor: ObservationSequence::new(self.sequence),
        })
    }
    fn stream(&self, pane: PaneId) -> Option<ProgramStreamState> {
        (self.pane == Some(pane)).then_some(ProgramStreamState {
            connection: ConnectionEpoch::new(self.epoch),
            pending_gap: None,
        })
    }
    fn invalidate(&mut self, pane: PaneId, reason: ObservationGap) {
        self.epoch += 1;
        self.sequence += 1;
        self.events.clear();
        self.gaps.push(ObservedEvent {
            pane,
            connection: ConnectionEpoch::new(self.epoch),
            sequence: ObservationSequence::new(self.sequence),
            event: ProgramEvent::Gap(reason),
        });
    }
    fn pending_bytes(&self) -> usize {
        0
    }
    fn set_payload_budget(&mut self, _available: usize) {}
    fn drain(&mut self) -> ProgramDrain {
        ProgramDrain {
            observations: std::mem::take(&mut self.events),
            invalidations: std::mem::take(&mut self.gaps),
            ..ProgramDrain::default()
        }
    }
}
static NATIVE_CASE: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    scratch: common::Scratch,
    tmux: Tmux,
    pane: String,
    pid: u32,
    session: SessionId,
    manifests: Vec<LoadedManifest>,
    runtime: ProgramRuntime,
    collector: Collector,
    token: Token,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Option<Self> {
        if !common::tmux_available() {
            return None;
        }
        let serial = NATIVE_CASE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let scratch = common::Scratch::new("b5b_ownership");
        assert!(scratch
            .tmux(&["new-session", "-d", "-s", "owned", "exec /bin/sleep 600"])
            .status
            .success());
        let pane = scratch.get("owned", "#{pane_id}");
        common::poll_until("sleep reporter", || {
            scratch.get(&pane, "#{pane_current_command}") == "sleep"
        });
        let pid = scratch.get(&pane, "#{pane_pid}").parse().unwrap();
        let session = SessionId::parse(&scratch.get(&pane, "#{session_id}")).unwrap();
        let manifest = Manifest::parse(
            r#"min_engine_version = "0.1"
[identity]
process_names = ["sleep"]
[capture]
visible = ["working"]
[program_status]
apps = ["pi"]
[hooks]
covers = ["working", "idle", "blocked", "lifecycle"]
[[hooks.map]]
event = "start"
claim = { lifecycle = "start" }
[[hooks.map]]
event = "end"
claim = { lifecycle = "end" }
[[hooks.map]]
event = "work"
claim = { state = "working" }
[[hooks.map]]
event = "done"
claim = { state = "idle" }
turn_end = true
[[hooks.map]]
event = "blocked"
claim = { state = "blocked", detail = "permission" }
"#,
            "reporter.toml",
        )
        .unwrap();
        let engine = RuleEngine::build(&manifest).unwrap();
        let tmux = Tmux::connect(&Server::named(Some(scratch.socket.clone())));
        let token = store::fresh_token().unwrap();
        let mut fixture = Self {
            scratch,
            tmux,
            pane,
            pid,
            session,
            manifests: vec![LoadedManifest {
                name: "reporter".into(),
                manifest,
                engine,
            }],
            runtime: ProgramRuntime::new(),
            collector: Collector::default(),
            token,
            _serial: serial,
        };
        let facts = ProgramPane {
            pane: fixture.id(),
            pane_pid: pid,
            current_command: "sleep",
            title: "",
            ignored: false,
            registration: None,
        };
        fixture.runtime.process_batch(
            &mut fixture.collector,
            &[facts],
            &fixture.manifests,
            &mut program_processes,
            tma_runtime::now_ms(),
        );
        assert!(matches!(
            store::acquire_collector(
                &fixture.tmux,
                &fixture.target(),
                &fixture.token,
                tma_runtime::now_ms
            )
            .unwrap(),
            store::LeaseOutcome::Won(_)
        ));
        Some(fixture)
    }
    fn id(&self) -> PaneId {
        PaneId::parse(&self.pane).unwrap()
    }
    fn command(&self) -> std::process::Command {
        let mut command = self.scratch.command();
        command.args(["--socket-name", &self.scratch.socket]);
        command
    }
    fn target(&self) -> String {
        format!("${}:.%{}", self.session.get(), self.id().get())
    }
    fn read(&self) -> store::PaneRead {
        store::read_pane(&self.tmux, &self.target()).unwrap()
    }
    fn reduce(&mut self, id: &str, status: ProgramStatus) {
        self.collector.report(id, status);
        let facts = ProgramPane {
            pane: self.id(),
            pane_pid: self.pid,
            current_command: "sleep",
            title: "",
            ignored: false,
            registration: None,
        };
        self.runtime.process_batch(
            &mut self.collector,
            &[facts],
            &self.manifests,
            &mut program_processes,
            tma_runtime::now_ms(),
        );
    }
    fn publish(&mut self) -> store::CommitOutcome {
        let decision = self.runtime.decision(self.id()).unwrap();
        let facts = ProgramPane {
            pane: self.id(),
            pane_pid: self.pid,
            current_command: "sleep",
            title: "",
            ignored: false,
            registration: None,
        };
        self.runtime
            .publish_checked(
                &self.tmux,
                &mut self.collector,
                &decision,
                &facts,
                &self.manifests,
                &program_processes().unwrap(),
                &self.token,
                self.session,
                tma_runtime::now_ms,
            )
            .unwrap()
    }
    fn restart(&mut self) {
        store::release_collector(&self.tmux, &self.target(), &self.token).unwrap();
        self.token = store::fresh_token().unwrap();
        self.runtime = ProgramRuntime::new();
        self.collector = Collector::default();
        let facts = ProgramPane {
            pane: self.id(),
            pane_pid: self.pid,
            current_command: "sleep",
            title: "",
            ignored: false,
            registration: None,
        };
        self.runtime.process_batch(
            &mut self.collector,
            &[facts],
            &self.manifests,
            &mut program_processes,
            tma_runtime::now_ms(),
        );
        assert!(matches!(
            store::acquire_collector(&self.tmux, &self.target(), &self.token, tma_runtime::now_ms)
                .unwrap(),
            store::LeaseOutcome::Won(_)
        ));
        assert!(
            self.runtime.decision(self.id()).is_none(),
            "history alone never publishes"
        );
    }
    fn state(&self) -> tma_core::StampedState {
        model::current_stamp(&self.read().options, tma_runtime::now_ms())
            .unwrap()
            .unwrap()
            .into_inner()
    }
    fn hook(&self, kind: &str, payload: &str, notify: bool) {
        let commands = tma_runtime::config::NotifyCommands::default();
        let sinks = tma_runtime::config::NotifySinks::default();
        let on = [
            tma_runtime::config::NotifyTrigger::Blocked,
            tma_runtime::config::NotifyTrigger::Done,
        ];
        apply_event(
            &self.tmux,
            &self.manifests[0],
            &self.target(),
            kind,
            payload,
            &NotifyPolicy {
                opt_in: notify,
                on: &on,
                commands: &commands,
                sinks: &sinks,
            },
            tma_runtime::now_ms(),
        );
    }
    fn plan(&self, state: AgentState, at: u64) -> StampPlan {
        StampPlan::Publish(Publish {
            pane_id: self.target(),
            state,
            detail: None,
            source: Provenance::Capture,
            evidence_at: at,
            since: at,
            stamped_at: at,
            hash: Some(900),
            pid: self.pid,
            name: "capture-loser".into(),
            set_attention: true,
            episode_reset: true,
            guard: Guard::Unconditional,
        })
    }
}

#[test]
fn program_winner_owns_whole_tuple_rows_holds_removal_and_summaries() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.hook("work", r#"{"session_id":"same"}"#, false);
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    let winner = f.read();
    f.hook("blocked", r#"{"session_id":"same","request_id":"req_real","tool_name":"Bash","tool_use_id":"call_real","tool_input":{"command":"echo hello"}}"#, false);
    let metadata = f.read();
    assert_eq!(
        metadata
            .options
            .get(opt::PERMISSION_REQUEST)
            .map(String::as_str),
        Some("req_real")
    );
    assert_eq!(
        metadata.options.get(opt::PENDING_CALL).map(String::as_str),
        Some("call_real")
    );
    let panes = f.tmux.list_panes().unwrap();
    for plan in [
        f.plan(AgentState::Idle, tma_runtime::now_ms()),
        StampPlan::Hold {
            stamped_at: 1,
            hash: Some(999),
        },
        StampPlan::Remove,
    ] {
        let applied = stamp::apply(&f.tmux, &panes, &f.target(), &plan, true).unwrap();
        assert!(!applied.committed);
        for key in [
            opt::STATE,
            opt::SOURCE,
            opt::PID,
            opt::NAME,
            opt::HASH,
            opt::SINCE,
            opt::EVIDENCE_AT,
            opt::STAMPED_AT,
            opt::ATTENTION,
            opt::TURN_AT,
        ] {
            assert_eq!(
                applied.read.options.get(key),
                winner.options.get(key),
                "losing {key}"
            );
        }
    }
    assert_eq!(f.scratch.get(&f.target(), "#{@agent_summary}"), "working:1");
    assert_eq!(
        f.scratch.get(&f.target(), "#{@agent_session_summary}"),
        "working:1"
    );
    let rows = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap();
    assert_eq!(rows[0].state, AgentState::Working);
    assert_eq!(rows[0].agent, "reporter");
    let old = tma_runtime::now_ms() - 60_000;
    f.tmux
        .set_pane_option(&f.target(), opt::STAMPED_AT, &old.to_string())
        .unwrap();
    f.tmux
        .set_pane_option(&f.target(), opt::EVIDENCE_AT, &old.to_string())
        .unwrap();
    let debug = tma_runtime::debug::observe(&f.tmux, &f.pane, &f.manifests, &FoldConfig::default())
        .unwrap();
    assert_eq!(
        debug.state(),
        AgentState::Working,
        "live program reports do not age out in debug"
    );
    let report = tma_runtime::cycle::run_cycle_with(
        &f.tmux,
        &f.manifests,
        &FoldConfig::default(),
        tma_runtime::cycle::SeenClear::Deferred,
    )
    .unwrap();
    assert_eq!(report.captures, 0);
    assert_eq!(report.rows[0].state, AgentState::Working);
    let end = stamp::apply(
        &f.tmux,
        &f.tmux.list_panes().unwrap(),
        &f.target(),
        &StampPlan::EndSession,
        true,
    )
    .unwrap();
    assert!(end.committed);
    assert_eq!(f.state().state, AgentState::Working);
    f.hook("start", r#"{"session_id":"same"}"#, false);
    f.hook("end", r#"{"session_id":"same"}"#, false);
    assert_eq!(f.state().state, AgentState::Working);
    assert!(!f.read().options.contains_key(opt::SESSION));
    assert!(!f.read().options.contains_key(opt::PERMISSION_REQUEST));
    f.hook("start", r#"{"session_id":"replacement","model":"new-model","transcript_path":"/tmp/new-transcript"}"#, false);
    let bound = f.read();
    assert_eq!(
        bound.options.get(opt::SESSION).map(String::as_str),
        Some("replacement")
    );
    assert_eq!(
        bound.options.get(opt::MODEL).map(String::as_str),
        Some("new-model")
    );
    f.hook("end", r#"{"session_id":"same"}"#, false);
    assert_eq!(
        f.read().options.get(opt::SESSION),
        bound.options.get(opt::SESSION)
    );
}

#[test]
fn first_hook_metadata_survives_only_a_same_binding_cycle_receipt_refresh() {
    use std::os::unix::fs::PermissionsExt;
    #[derive(Clone, Copy, Debug)]
    enum Interference {
        Receipt,
        Session,
        Metadata,
        Episode,
        Process,
    }
    for case in [
        Interference::Receipt,
        Interference::Session,
        Interference::Metadata,
        Interference::Episode,
        Interference::Process,
    ] {
        let Some(f) = Fixture::new() else { return };
        assert!(
            stamp::apply(
                &f.tmux,
                &[],
                &f.target(),
                &f.plan(AgentState::Idle, tma_runtime::now_ms()),
                true
            )
            .unwrap()
            .committed
        );
        let original = f.read();
        assert!(store::metadata_owner(&original).is_none());
        let ready = f.scratch.workdir.join("first-metadata-ready");
        let release = f.scratch.workdir.join("first-metadata-release");
        let completed = f.scratch.workdir.join("first-metadata-status");
        let wrapper = f.scratch.workdir.join("first-metadata-gate");
        std::fs::write(&wrapper, format!("#!/bin/sh\nis_write=0\ngated=0\nfor arg do\n[ \"$arg\" != if-shell ] || is_write=1\ndone\nif [ $is_write -eq 1 ] && [ ! -e '{ready}' ]; then\nfor arg do\ncase \"$arg\" in\n*'@tma_metadata_owner'*'@tma_metadata_update'*) gated=1; touch '{ready}'; i=0; while [ ! -e '{release}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done; [ -e '{release}' ] || exit 125 ;;\nesac\ndone\nfi\nif [ $gated -eq 1 ]; then\ntmux \"$@\"\nstatus=$?\nprintf '%s' \"$status\" > '{completed}'\nexit \"$status\"\nfi\nexec tmux \"$@\"\n", ready=ready.display(), release=release.display(), completed=completed.display())).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&Server {
            socket_name: Some(f.scratch.socket.clone()),
            bin: Some(wrapper.display().to_string()),
            ..Default::default()
        });
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                apply_event(
                    &delayed,
                    &f.manifests[0],
                    &f.target(),
                    "blocked",
                    r#"{"session_id":"first-hook"}"#,
                    &NotifyPolicy {
                        opt_in: false,
                        on: &[],
                        commands: &Default::default(),
                        sinks: &Default::default(),
                    },
                    tma_runtime::now_ms(),
                )
            });
            common::poll_until("first hook reached its actual metadata write", || {
                ready.exists()
            });
            if matches!(case, Interference::Session) {
                f.hook("start", r#"{"session_id":"replacement"}"#, false);
            } else {
                let mut expected_options = original.options.clone();
                let changed = match case {
                    Interference::Metadata => Some((opt::MODEL, "competing-model".to_string())),
                    Interference::Episode => Some((
                        model::opt::ATTENTION_EPISODE,
                        store::fresh_token().unwrap().as_str().to_owned(),
                    )),
                    _ => None,
                };
                if let Some((key, value)) = changed {
                    f.tmux.set_pane_option(&f.target(), key, &value).unwrap();
                    expected_options.insert(key.into(), value);
                }
                let before_refresh = f.read();
                let same_stamp = original.options[opt::STAMPED_AT].clone();
                assert!(store::write_fallback(
                    &f.tmux,
                    &before_refresh,
                    Guard::Unconditional,
                    tma_runtime::now_ms,
                    &[OptionEdit::set(opt::STAMPED_AT, same_stamp)]
                )
                .unwrap()
                .won());
                if matches!(case, Interference::Process) {
                    assert!(f
                        .scratch
                        .tmux(&["respawn-pane", "-k", "-t", &f.pane, "exec /bin/sleep 600"])
                        .status
                        .success());
                    let fresh_pid: u32 = f.scratch.get(&f.pane, "#{pane_pid}").parse().unwrap();
                    assert_ne!(fresh_pid, f.pid, "actual pane root was replaced");
                }
                let refreshed = f.read();
                let mut refreshed_options = refreshed.options.clone();
                assert_ne!(
                    expected_options.remove(model::opt::RECEIPT),
                    refreshed_options.remove(model::opt::RECEIPT)
                );
                assert_eq!(
                    expected_options, refreshed_options,
                    "only the named interference and receipt changed: {case:?}"
                );
            }
            std::fs::write(&release, "release").unwrap();
            let outcome = pending.join().unwrap();
            assert_eq!(
                std::fs::read_to_string(&completed).unwrap(),
                "0",
                "gated native write completed without timeout: {case:?}"
            );
            let current = f.read();
            eprintln!("first-hook metadata race: interference={case:?}, native_status=0, event={outcome:?}, state={:?}, session={:?}, metadata_owner={:?}", current.options.get(opt::STATE), current.options.get(opt::SESSION), store::metadata_owner(&current));
            if matches!(case, Interference::Session) {
                assert_eq!(current.options[opt::SESSION], "replacement");
                assert_eq!(current.options[opt::STATE], "idle");
            } else if matches!(case, Interference::Receipt) {
                assert_eq!(
                    current.options.get(opt::SESSION).map(String::as_str),
                    Some("first-hook")
                );
                assert_eq!(current.options[opt::STATE], "blocked");
                assert_eq!(store::metadata_owner(&current).unwrap().process.pid, f.pid);
            } else {
                assert_eq!(current.options[opt::STATE], "idle");
                assert!(store::metadata_owner(&current).is_none());
                assert!(!current.options.contains_key(opt::SESSION));
            }
        });
    }
}

#[test]
fn first_sessionless_state_hook_binds_without_inventing_action_metadata() {
    for (kind, state) in [("work", "working"), ("blocked", "blocked")] {
        let Some(f) = Fixture::new() else { return };
        assert!(
            stamp::apply(
                &f.tmux,
                &[],
                &f.target(),
                &f.plan(AgentState::Idle, tma_runtime::now_ms()),
                true
            )
            .unwrap()
            .committed
        );
        f.hook(kind, "{}", false);
        let current = f.read();
        eprintln!(
            "first sessionless hook: kind={kind}, state={:?}, metadata_owner={:?}",
            current.options.get(opt::STATE),
            store::metadata_owner(&current)
        );
        assert_eq!(current.options[opt::STATE], state);
        let owner = store::metadata_owner(&current).unwrap();
        assert_eq!(owner.process.pid, f.pid);
        assert_eq!(owner.session, "<unattributed>");
        for key in [
            opt::SESSION,
            opt::PERMISSION_REQUEST,
            opt::QUESTION_REQUEST,
            opt::PENDING_TOOL,
            opt::PENDING_CALL,
        ] {
            assert!(
                !current.options.contains_key(key),
                "sessionless state created {key}"
            );
        }
    }
}

#[test]
fn applied_metadata_with_a_lost_receipt_is_not_readmitted() {
    use std::os::unix::fs::PermissionsExt;
    let Some(f) = Fixture::new() else { return };
    f.hook("start", r#"{"session_id":"same"}"#, false);
    let original = f.read();
    let ready = f.scratch.workdir.join("applied-metadata-ready");
    let release = f.scratch.workdir.join("applied-metadata-release");
    let writes = f.scratch.workdir.join("metadata-write-count");
    std::fs::write(&writes, "").unwrap();
    let wrapper = f.scratch.workdir.join("applied-metadata-gate");
    std::fs::write(&wrapper, format!("#!/bin/sh\nis_write=0\nfor arg do\n[ \"$arg\" != if-shell ] || is_write=1\ndone\nfor arg do\nif [ $is_write -eq 1 ]; then\ncase \"$arg\" in\n*\"'@tma_metadata_owner'\"*\"'@tma_metadata_update'\"*) printf x >> '{writes}';;\nesac\nfi\ncase \"$arg\" in\n*'@tma_program_origin'*'end')\nif [ ! -e '{ready}' ]; then touch '{ready}'; i=0; while [ ! -e '{release}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done; [ -e '{release}' ] || exit 125; fi;;\nesac\ndone\nexec tmux \"$@\"\n", ready=ready.display(), release=release.display(), writes=writes.display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let delayed = Tmux::connect(&Server {
        socket_name: Some(f.scratch.socket.clone()),
        bin: Some(wrapper.display().to_string()),
        ..Default::default()
    });
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            apply_event(
                &delayed,
                &f.manifests[0],
                &f.target(),
                "work",
                r#"{"session_id":"same"}"#,
                &NotifyPolicy {
                    opt_in: false,
                    on: &[],
                    commands: &Default::default(),
                    sinks: &Default::default(),
                },
                tma_runtime::now_ms(),
            )
        });
        common::poll_until("metadata applied before its delayed readback", || {
            ready.exists()
        });
        let applied = f.read();
        assert_eq!(
            store::metadata_owner(&applied),
            store::metadata_owner(&original)
        );
        for key in model::METADATA_KEYS {
            assert_eq!(
                applied.options.get(*key),
                original.options.get(*key),
                "unchanged metadata {key}"
            );
        }
        assert_ne!(
            applied.options.get(model::opt::METADATA_UPDATE),
            original.options.get(model::opt::METADATA_UPDATE)
        );
        let at = applied.options[opt::STAMPED_AT].clone();
        assert!(store::write_fallback(
            &f.tmux,
            &applied,
            Guard::Unconditional,
            tma_runtime::now_ms,
            &[OptionEdit::set(opt::STAMPED_AT, at)]
        )
        .unwrap()
        .won());
        std::fs::write(&release, "release").unwrap();
        assert_eq!(
            pending.join().unwrap(),
            tma_runtime::event::EventOutcome::Applied
        );
        // Only quoted mutation bodies count, not marker names inside the predicate.
        let count = std::fs::metadata(&writes).unwrap().len();
        assert_eq!(
            count, 1,
            "an already-applied metadata update must not be admitted again"
        );
        let current = f.read();
        assert_eq!(current.options[opt::STATE], "working");
        assert_eq!(
            current.options.get(model::opt::METADATA_UPDATE),
            applied.options.get(model::opt::METADATA_UPDATE)
        );
        eprintln!("applied metadata race: metadata_writes={count}, update_fence_preserved=true, hook_state=working");
    });
}

#[test]
fn exact_old_ack_cannot_clear_new_idle_result_and_spent_dialog_restore_stays_clear() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let first = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    let old_turn = f.state().turn_at;
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let second = f.read().result().unwrap();
    assert_ne!(first.read.result(), Some(second.clone()));
    assert!(
        f.state().turn_at > old_turn,
        "exclusive wait --since floor advances"
    );
    assert!(!tma_runtime::seen::acknowledge(&f.tmux, &first).unwrap());
    assert!(f.state().attention);
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(tma_runtime::seen::acknowledge(&f.tmux, &current).unwrap());
    assert_eq!(f.read().program.acknowledged, Some(second));
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(f.state().attention);
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(
        !f.state().attention,
        "spent done cannot keep blocked attention"
    );
}

#[test]
fn deferred_child_result_is_not_acknowledged_until_aggregate_settles() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("child", ProgramStatus::Done);
    assert!(f.publish().won());
    assert_eq!(f.state().state, AgentState::Working);
    assert!(AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).is_none());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    assert!(f.read().program.acknowledged.is_none());
    f.reduce("", ProgramStatus::Idle);
    assert!(f.publish().won());
    assert!(
        f.state().attention,
        "unseen selected child is now presented"
    );
    assert!(f.read().result().unwrap().identity.record.as_ref().as_str() == "child");
}

#[test]
fn duplicate_blocked_claim_survives_clear_metadata_and_duplicate_publication() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let observation = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    let stale = observation.clone();
    assert!(
        store::claim_observation(&f.tmux, &observation, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    assert!(
        !store::claim_observation(&f.tmux, &stale, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    f.hook("start", r#"{"session_id":"metadata","model":"m"}"#, false);
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(
        !f.state().attention,
        "duplicate blocked does not rearm after clear"
    );
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_eq!(current.identity, observation.identity);
    assert!(
        !store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn stale_projection_never_becomes_immortal_hook_and_fallback_repairs_it() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    assert!(matches!(
        store::release_collector(&f.tmux, &f.target(), &f.token).unwrap(),
        store::LeaseOutcome::Won(_)
    ));
    assert!(tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().is_empty());
    assert!(AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).is_none());
    for plan in [
        StampPlan::Hold {
            stamped_at: tma_runtime::now_ms(),
            hash: Some(123),
        },
        StampPlan::EndSession,
    ] {
        stamp::apply(&f.tmux, &[], &f.target(), &plan, true).unwrap();
        assert!(
            tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().is_empty(),
            "hold cannot turn stale program origin into a legacy hook"
        );
    }
    let plan = f.plan(AgentState::Idle, tma_runtime::now_ms());
    let applied = stamp::apply(
        &f.tmux,
        &f.tmux.list_panes().unwrap(),
        &f.target(),
        &plan,
        true,
    )
    .unwrap();
    assert!(applied.committed);
    assert_eq!(f.state().state, AgentState::Idle);
    assert!(!f.read().program.program_origin);
}

#[test]
fn delayed_summary_cas_cannot_overwrite_newer_winner() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.hook("work", r#"{"session_id":"legacy"}"#, false);
    f.scratch
        .tmux(&["set-option", "-wu", "-t", &f.target(), opt::SUMMARY]);
    let obsolete = stamp::summary_commands(&f.tmux).unwrap();
    assert!(!obsolete.is_empty());
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    f.tmux.apply(&obsolete).unwrap();
    assert_eq!(f.scratch.get(&f.target(), "#{@agent_summary}"), "blocked:1");
    assert_eq!(
        f.scratch.get(&f.target(), "#{@agent_session_summary}"),
        "blocked:1"
    );

    f.scratch
        .tmux(&["set-option", "-wu", "-t", &f.target(), opt::SUMMARY]);
    let before_membership_change = stamp::summary_commands(&f.tmux).unwrap();
    assert!(!before_membership_change.is_empty());
    let split = f
        .scratch
        .tmux(&["split-window", "-d", "-t", &f.target(), "exec /bin/cat"]);
    assert!(split.status.success());
    f.scratch.tmux(&[
        "set-option",
        "-w",
        "-t",
        &f.target(),
        opt::SUMMARY,
        "membership-sentinel",
    ]);
    f.tmux.apply(&before_membership_change).unwrap();
    assert_eq!(
        f.scratch.get(&f.target(), "#{@agent_summary}"),
        "membership-sentinel",
        "an added agentless pane still changes the scope snapshot"
    );
    stamp::reconcile_summaries(&f.tmux).unwrap();
    assert_eq!(f.scratch.get(&f.target(), "#{@agent_summary}"), "blocked:1");

    // Several bounded fields exceed the scope limit without exceeding tmux's command limit.
    for key in [
        opt::STATE,
        opt::SOURCE,
        opt::PID,
        opt::EVIDENCE_AT,
        opt::SINCE,
        opt::STAMPED_AT,
        opt::DETAIL,
    ] {
        f.tmux
            .set_pane_option(&f.target(), key, &"d".repeat(10 * 1024))
            .unwrap();
    }
    assert!(
        stamp::summary_commands(&f.tmux).unwrap().is_empty(),
        "an oversized snapshot must be skipped, never truncated"
    );
}

#[test]
fn legacy_migration_spent_claim_survives_source_change_and_equal_clock_new_episode() {
    let Some(f) = Fixture::new() else {
        return;
    };
    let at = tma_runtime::now_ms();
    let mut plan = f.plan(AgentState::Blocked, at);
    if let StampPlan::Publish(publish) = &mut plan {
        publish.episode_reset = false;
        publish.source = Provenance::Hook;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &plan, true)
            .unwrap()
            .committed
    );
    let observation = AttentionObservation::from_read(f.read(), at).unwrap();
    assert!(store::claim_observation(&f.tmux, &observation, || at)
        .unwrap()
        .claimed());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    if let StampPlan::Publish(publish) = &mut plan {
        publish.source = Provenance::Capture;
    }
    stamp::apply(&f.tmux, &[], &f.target(), &plan, true).unwrap();
    let refreshed = AttentionObservation::from_read(f.read(), at).unwrap();
    assert_eq!(refreshed.identity.encode(), observation.identity.encode());
    assert!(!store::claim_observation(&f.tmux, &refreshed, || at)
        .unwrap()
        .claimed());
    // Simulate a previously shipped legacy tuple, before the stable episode token existed.
    f.tmux
        .unset_pane_option(&f.target(), model::opt::ATTENTION_EPISODE)
        .unwrap();
    f.tmux
        .unset_pane_option(&f.target(), model::opt::EPISODE_NOTIFIED)
        .unwrap();
    stamp::apply(
        &f.tmux,
        &[],
        &f.target(),
        &StampPlan::Hold {
            stamped_at: at,
            hash: None,
        },
        true,
    )
    .unwrap();
    let migrated = AttentionObservation::from_read(f.read(), at).unwrap();
    assert!(!store::claim_observation(&f.tmux, &migrated, || at)
        .unwrap()
        .claimed());
    let mut working = f.plan(AgentState::Working, at);
    if let StampPlan::Publish(publish) = &mut working {
        publish.episode_reset = false;
    }
    stamp::apply(&f.tmux, &[], &f.target(), &working, true).unwrap();
    stamp::apply(&f.tmux, &[], &f.target(), &plan, true).unwrap();
    let next = AttentionObservation::from_read(f.read(), at).unwrap();
    assert_ne!(next.identity.encode(), migrated.identity.encode());
    assert!(store::claim_observation(&f.tmux, &next, || at)
        .unwrap()
        .claimed());
}

#[test]
fn fresh_runtime_rebinds_ack_history_repeatedly_then_observed_work_is_new() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Error);
    assert!(f.publish().won());
    let old_turn = f.state().turn_at;
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    for _ in 0..2 {
        f.restart();
        f.reduce("", ProgramStatus::Error);
        assert!(f.publish().won());
        assert!(!f.state().attention);
        assert_eq!(f.state().turn_at, old_turn);
        assert_eq!(f.read().program.acknowledged, f.read().result());
    }
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Error);
    assert!(f.publish().won());
    assert!(!f.state().attention);
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Error);
    assert!(f.publish().won());
    assert!(f.state().attention);
    assert!(f.state().turn_at > old_turn);
    assert_ne!(f.read().program.acknowledged, f.read().result());
}

#[test]
fn acknowledged_work_result_history_survives_repeated_publication_and_restart() {
    let Some(mut f) = Fixture::new() else { return };
    for reset in [
        ProgramStatus::Working { progress: None },
        ProgramStatus::Idle,
    ] {
        f.reduce("", reset);
        assert!(f.publish().won());
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
        let acknowledged = f.read().result().unwrap();
        let turn = f.state().turn_at;
        for _ in 0..3 {
            f.reduce("", ProgramStatus::Done);
            assert!(f.publish().won());
            assert_eq!(f.read().result(), Some(acknowledged.clone()));
            assert_eq!(f.read().program.acknowledged, Some(acknowledged.clone()));
            assert!(!f.state().attention);
        }
        f.restart();
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        assert!(!f.state().attention);
        assert_eq!(f.state().turn_at, turn);
        assert_eq!(f.read().result(), f.read().program.acknowledged);
    }
}

#[test]
fn delayed_program_publication_cannot_erase_a_later_result_invalidation() {
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let original = f.read();
    let owner = original.owner().unwrap();
    let result = original.result().unwrap();
    store::invalidate_history(&f.tmux, &original, &owner, &model::HistoryInvalidation::All)
        .unwrap();
    let StampPlan::Publish(mut publish) = f.plan(AgentState::Idle, tma_runtime::now_ms()) else {
        unreachable!()
    };
    publish.episode_reset = false;
    let outcome = store::publish_program(
        &f.tmux,
        &original,
        &owner,
        tma_runtime::now_ms,
        &publish,
        Some(&result),
        &[],
    )
    .unwrap();
    assert!(
        !outcome.won(),
        "the delayed publication observed no reset fence"
    );
    assert_eq!(outcome.winner().program.invalidated, Some(result));
}

#[test]
fn notified_unacknowledged_result_survives_fresh_runtime_without_duplicate_claim() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let old_turn = f.state().turn_at;
    let old = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(store::claim_observation(&f.tmux, &old, tma_runtime::now_ms)
        .unwrap()
        .claimed());
    for _ in 0..2 {
        f.restart();
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        assert!(
            f.state().attention,
            "unacknowledged presentation remains visible"
        );
        assert_eq!(f.state().turn_at, old_turn);
        let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
        assert!(
            !store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
                .unwrap()
                .claimed()
        );
    }
}

#[test]
fn unrelated_parent_idle_preserves_child_history_but_child_idle_resets_it() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("child", ProgramStatus::Done);
    assert!(f.publish().won());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    let turn = f.state().turn_at;
    f.restart();
    f.reduce("", ProgramStatus::Idle);
    assert!(f.publish().won());
    f.reduce("child", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(
        !f.state().attention,
        "parent idle is not a child result reset"
    );
    assert_eq!(f.state().turn_at, turn);
    f.reduce("child", ProgramStatus::Idle);
    assert!(f.publish().won());
    f.reduce("child", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(
        f.state().attention,
        "the child's own explicit idle starts a new result"
    );
    assert!(f.state().turn_at > turn);
}

#[test]
fn equal_clock_concurrent_legacy_claims_have_only_one_winner() {
    let Some(f) = Fixture::new() else {
        return;
    };
    f.hook("blocked", r#"{"session_id":"same"}"#, false);
    let observation = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    let now = tma_runtime::now_ms();
    let wins = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            store::claim_observation(&f.tmux, &observation, || now)
                .unwrap()
                .claimed()
        });
        let b = scope.spawn(|| {
            store::claim_observation(&f.tmux, &observation, || now)
                .unwrap()
                .claimed()
        });
        usize::from(a.join().unwrap()) + usize::from(b.join().unwrap())
    });
    assert_eq!(wins, 1);
}

#[test]
fn yielding_hooks_keep_legacy_notify_and_state_fallback_without_protocol_effect_permission() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-t",
            &format!("${}", f.session.get()),
            "after-set-option",
            "run-shell 'sleep 0.005'"
        ])
        .status
        .success());
    f.hook("blocked", r#"{"session_id":"legacy"}"#, true);
    assert_eq!(f.state().state, AgentState::Blocked);
    let current = f.read();
    assert!(
        current.options.contains_key(model::opt::EPISODE_NOTIFIED),
        "legacy direct fire spent its unique claim"
    );
    assert!(current.options.contains_key(opt::NOTIFIED_AT));
    let observation = AttentionObservation::from_read(current, tma_runtime::now_ms()).unwrap();
    assert!(
        !store::claim_observation(&f.tmux, &observation, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    f.reduce("", ProgramStatus::Done);
    let decision = f.runtime.decision(f.id()).unwrap();
    let facts = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    assert!(matches!(
        f.runtime.publish_checked(
            &f.tmux,
            &mut f.collector,
            &decision,
            &facts,
            &f.manifests,
            &program_processes().unwrap(),
            &f.token,
            f.session,
            tma_runtime::now_ms
        ),
        Err(
            tma_runtime::program_status::publication::ProgramPublishError::Persistence(
                store::PersistenceError::Unsupported
            )
        )
    ));
    assert!(f.state().state == AgentState::Blocked);
}

#[test]
fn metadata_evidence_order_and_old_session_binding_are_independent_of_program_state() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.hook("start", r#"{"session_id":"first"}"#, false);
    let original = f.read();
    let owner = store::metadata_owner(&original).unwrap();
    let newer = [
        OptionEdit::set(opt::CONTEXT_PCT, "90"),
        OptionEdit::set(opt::CONTEXT_AT, "200"),
    ];
    assert!(store::write_metadata_observation(
        &f.tmux,
        &original,
        Some(&owner),
        Some(&owner),
        &newer,
        Some(MetadataEvidence::Context(200))
    )
    .unwrap()
    .won());
    let old = [
        OptionEdit::set(opt::CONTEXT_PCT, "10"),
        OptionEdit::set(opt::CONTEXT_AT, "100"),
    ];
    let current = f.read();
    assert!(!store::write_metadata_observation(
        &f.tmux,
        &current,
        Some(&owner),
        Some(&owner),
        &old,
        Some(MetadataEvidence::Context(100))
    )
    .unwrap()
    .won());
    f.hook("end", r#"{"session_id":"first"}"#, false);
    f.hook("start", r#"{"session_id":"second"}"#, false);
    let read = f.read();
    let second = store::metadata_owner(&read).unwrap();
    assert_ne!(owner, second);
    assert!(!store::write_metadata(
        &f.tmux,
        &read,
        Some(&owner),
        Some(&owner),
        &[OptionEdit::set(opt::MODEL, "old")]
    )
    .unwrap()
    .won());
    assert_eq!(f.state().state, AgentState::Working);
    assert_eq!(
        f.read().options.get(opt::SESSION).map(String::as_str),
        Some("second")
    );
}

#[test]
fn gap_after_real_work_recovers_unacknowledged_history_and_clear_fences_it() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let old_turn = f.state().turn_at;
    let old = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(store::claim_observation(&f.tmux, &old, tma_runtime::now_ms)
        .unwrap()
        .claimed());
    f.collector.invalidate(f.id(), ObservationGap::Disconnected);
    let facts = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let batch = f.runtime.process_batch(
        &mut f.collector,
        &[facts],
        &f.manifests,
        &mut program_processes,
        tma_runtime::now_ms(),
    );
    for release in batch.unstreamed_releases() {
        tma_runtime::program_status::publication::release_persisted(
            &f.tmux, f.session, &f.token, release,
        )
        .unwrap();
    }
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(f.state().attention);
    assert_eq!(f.state().turn_at, old_turn);
    let recovered = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(
        !store::claim_observation(&f.tmux, &recovered, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    f.collector.sequence += 1;
    f.collector.events.push(ObservedEvent {
        pane: f.id(),
        connection: ConnectionEpoch::new(f.collector.epoch),
        sequence: ObservationSequence::new(f.collector.sequence),
        event: ProgramEvent::Command(ProgramCommand::Clear(
            tma_core::program_status::RecordId::root(),
        )),
    });
    let facts = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let batch = f.runtime.process_batch(
        &mut f.collector,
        &[facts],
        &f.manifests,
        &mut program_processes,
        tma_runtime::now_ms(),
    );
    assert!(!batch.unstreamed_releases().is_empty());
    for release in batch.unstreamed_releases() {
        assert!(release.history_invalidation.is_some());
        tma_runtime::program_status::publication::release_persisted(
            &f.tmux, f.session, &f.token, release,
        )
        .unwrap();
    }
    assert!(f.read().program.invalidated.is_some());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(f.state().attention);
    assert!(f.state().turn_at > old_turn);
}

#[test]
fn protocol_approval_text_cannot_satisfy_binder_but_same_process_hook_request_can() {
    use tma_runtime::broker::{self, BrokerIo, FireArgs};
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: Some(tma_core::program_status::BlockedKind::Permission),
            progress: None,
        },
    );
    assert!(f.publish().won());
    let action = tma_core::ActionManifest::parse("min_engine_version = \"0.1\"\nname = \"approve\"\nlabel = \"Approve\"\nkind = \"keys\"\nwhen = { state = [\"blocked\"], detail = [\"permission\"] }\n[keys]\nreporter = [\"1\"]\n", "approve", "test.toml").unwrap();
    let cfg = FoldConfig::default();
    let api = tma_runtime::config::ApiSection::default();
    let io = broker::TmuxBroker {
        tmux: &f.tmux,
        manifests: &f.manifests,
        cfg: &cfg,
        api_bases: &api,
        server: Server::named(Some(f.scratch.socket.clone())),
        notify_command: None,
    };
    let facts = io.read_pane(&f.pane).unwrap().unwrap();
    assert!(facts.permission_request.is_none());
    assert!(facts.pending_tool.is_none());
    let refused = broker::act(
        &io,
        &action,
        &f.pane,
        FireArgs {
            force: true,
            expect_permission_request: Some("approve_from_protocol_text"),
            ..FireArgs::default()
        },
    );
    assert_eq!(refused.reason(), Some("request-gone"));
    f.hook("blocked", r#"{"session_id":"same","request_id":"genuine_request","tool_name":"Bash","tool_use_id":"genuine_call","tool_input":{"command":"echo harmless"}}"#, false);
    assert_eq!(f.state().state, AgentState::Blocked);
    let sent = broker::act(
        &io,
        &action,
        &f.pane,
        FireArgs {
            expect_permission_request: Some("genuine_request"),
            ..FireArgs::default()
        },
    );
    assert_eq!(sent.outcome, broker::Outcome::Sent);
    let now = tma_runtime::now_ms();
    let held = io.acquire(&f.pane, now, now + 60_000, "test").unwrap();
    let tma_tmux::lock::Acquire::Acquired(held) = held else {
        panic!("expected owned lock");
    };
    let busy = broker::act(&io, &action, &f.pane, FireArgs::default());
    assert_eq!(busy.exit_code(), 5);
    io.clear(&f.pane, &held.nonce).unwrap();
}

#[test]
fn newer_same_owner_telemetry_supersedes_old_yielding_metadata_suffix() {
    let Some(f) = Fixture::new() else {
        return;
    };
    f.hook("start", r#"{"session_id":"same"}"#, false);
    let gate = f.scratch.workdir.join("telemetry-release");
    let hook = format!("if-shell -F '#{{==:#{{@agent_context_pct}},10}}' \"run-shell 'i=0; while [ ! -e {} ] && [ \\$i -lt 200 ]; do sleep 0.01; i=\\$((i+1)); done'\"", gate.display());
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-set-option", &hook])
        .status
        .success());
    std::thread::scope(|scope| {
        let old = scope.spawn(|| {
            stamp::apply_context(&f.tmux, &[], &f.target(), Some(10), Some(10), 100, true).unwrap()
        });
        common::poll_until("older context field reaches yielding hook", || {
            f.scratch.pane_option(&f.pane, opt::CONTEXT_PCT) == "10"
        });
        assert!(f
            .scratch
            .tmux(&[
                "set-hook",
                "-g",
                "after-set-option",
                "run-shell 'sleep 0.001'"
            ])
            .status
            .success());
        stamp::apply_context(&f.tmux, &[], &f.target(), Some(90), Some(90), 200, true).unwrap();
        std::fs::write(&gate, "resume").unwrap();
        old.join().unwrap();
    });
    let current = f.read();
    assert_eq!(
        current.options.get(opt::CONTEXT_PCT).map(String::as_str),
        Some("90")
    );
    assert_eq!(
        current.options.get(opt::CONTEXT_AT).map(String::as_str),
        Some("200")
    );
    assert_eq!(
        current.options.get(opt::TOKENS).map(String::as_str),
        Some("90")
    );
}

#[test]
fn yielding_initial_metadata_bind_cannot_write_old_session_after_replacement() {
    let Some(f) = Fixture::new() else {
        return;
    };
    let gate = f.scratch.workdir.join("binding-release");
    let hook = format!("if-shell -F '#{{&&:#{{!=:#{{@tma_metadata_owner}},}},#{{==:#{{@agent_session}},}}}}' \"run-shell 'i=0; while [ ! -e {} ] && [ \\$i -lt 200 ]; do sleep 0.01; i=\\$((i+1)); done'\"", gate.display());
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-set-option", &hook])
        .status
        .success());
    std::thread::scope(|scope| {
        let old = scope.spawn(|| {
            f.hook(
                "start",
                r#"{"session_id":"old","model":"old-model","transcript_path":"/tmp/old"}"#,
                false,
            )
        });
        common::poll_until("initial metadata binding reaches yielding hook", || {
            !f.scratch
                .pane_option(&f.pane, model::opt::METADATA_OWNER)
                .is_empty()
        });
        assert!(f
            .scratch
            .tmux(&[
                "set-hook",
                "-g",
                "after-set-option",
                "run-shell 'sleep 0.001'"
            ])
            .status
            .success());
        f.hook("end", r#"{"session_id":"old"}"#, false);
        f.hook(
            "start",
            r#"{"session_id":"new","model":"new-model","transcript_path":"/tmp/new"}"#,
            false,
        );
        std::fs::write(&gate, "resume").unwrap();
        old.join().unwrap();
    });
    let read = f.read();
    assert_eq!(
        read.options.get(opt::SESSION).map(String::as_str),
        Some("new")
    );
    assert_eq!(
        read.options.get(opt::MODEL).map(String::as_str),
        Some("new-model")
    );
    assert_eq!(
        read.options.get(opt::TRANSCRIPT).map(String::as_str),
        Some("/tmp/new")
    );
}

#[test]
fn production_fallback_suffix_refuses_reactivated_protocol_owner() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.hook("work", r#"{"session_id":"same"}"#, false);
    let gate = f.scratch.workdir.join("fallback-release");
    let hook = format!("if-shell -F '#{{==:#{{@agent_state}},idle}}' \"run-shell 'i=0; while [ ! -e {} ] && [ \\$i -lt 200 ]; do sleep 0.01; i=\\$((i+1)); done'\"", gate.display());
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-set-option", &hook])
        .status
        .success());
    let plan = f.plan(AgentState::Idle, tma_runtime::now_ms());
    let panes = f.tmux.list_panes().unwrap();
    let tmux = Tmux::connect(&Server::named(Some(f.scratch.socket.clone())));
    let target = f.target();
    std::thread::scope(|scope| {
        let old = scope.spawn(|| stamp::apply(&tmux, &panes, &target, &plan, true).unwrap());
        common::poll_until("fallback state reaches yielding hook", || {
            f.scratch.pane_option(&f.pane, opt::STATE) == "idle"
        });
        assert!(f
            .scratch
            .tmux(&["set-hook", "-gu", "after-set-option"])
            .status
            .success());
        f.reduce(
            "",
            ProgramStatus::Blocked {
                kind: None,
                progress: None,
            },
        );
        assert!(f.publish().won());
        let winner = f.read();
        std::fs::write(&gate, "resume").unwrap();
        let lost = old.join().unwrap();
        assert!(!lost.committed);
        for key in [
            opt::STATE,
            opt::SOURCE,
            opt::PID,
            opt::NAME,
            opt::HASH,
            opt::EVIDENCE_AT,
            opt::SINCE,
            opt::STAMPED_AT,
            opt::ATTENTION,
        ] {
            assert_eq!(
                lost.read.options.get(key),
                winner.options.get(key),
                "late fallback {key}"
            );
        }
    });
}

#[test]
fn legacy_old_snapshot_cleanup_cannot_remove_replacement_or_its_metadata() {
    let Some(f) = Fixture::new() else {
        return;
    };
    f.hook("work", r#"{"session_id":"same"}"#, false);
    let stale = f.tmux.list_panes().unwrap();
    f.hook("done", r#"{"session_id":"same"}"#, false);
    let winner = f.read();
    assert!(
        !stamp::apply(&f.tmux, &stale, &f.target(), &StampPlan::Remove, true)
            .unwrap()
            .committed
    );
    for key in [
        opt::STATE,
        opt::SOURCE,
        opt::SINCE,
        opt::TURN_AT,
        opt::SESSION,
    ] {
        assert_eq!(
            f.read().options.get(key),
            winner.options.get(key),
            "late cleanup {key}"
        );
    }
}

#[test]
fn linked_session_publication_uses_the_verified_hook_context() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    assert!(f
        .scratch
        .tmux(&["new-session", "-d", "-s", "linked", "-t", "owned"])
        .status
        .success());
    let linked = SessionId::parse(&f.scratch.get("linked", "#{session_id}")).unwrap();
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-t",
            &format!("${}", linked.get()),
            "after-set-option",
            "run-shell 'sleep 0.001'"
        ])
        .status
        .success());
    f.reduce("", ProgramStatus::Done);
    let decision = f.runtime.decision(f.id()).unwrap();
    let facts = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let unsupported = f.runtime.publish_checked(
        &f.tmux,
        &mut f.collector,
        &decision,
        &facts,
        &f.manifests,
        &program_processes().unwrap(),
        &f.token,
        linked,
        tma_runtime::now_ms,
    );
    assert!(matches!(
        unsupported,
        Err(
            tma_runtime::program_status::publication::ProgramPublishError::Persistence(
                store::PersistenceError::Unsupported
            )
        )
    ));
    assert!(f.read().owner().is_none());
    assert!(f.publish().won());
    assert!(f.state().attention);
}

#[test]
fn continuing_blocked_episode_survives_fresh_collector_without_rearming_claim_or_attention() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let old = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(store::claim_observation(&f.tmux, &old, tma_runtime::now_ms)
        .unwrap()
        .claimed());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    f.restart();
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(!f.state().attention);
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_eq!(old.identity.encode(), current.identity.encode());
    assert!(
        !store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn released_blocker_keeps_exact_history_through_fallback_and_reacquisition() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let original = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(
        store::claim_observation(&f.tmux, &original, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    let read = f.read();
    let owner = read.owner().unwrap();
    assert!(store::release_program(&f.tmux, &read, &owner, &[])
        .unwrap()
        .won());
    let mut fallback = f.plan(AgentState::Blocked, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut fallback {
        publish.episode_reset = false;
        publish.set_attention = false;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &fallback, true)
            .unwrap()
            .committed
    );
    assert!(f.read().owner().is_none());
    let legacy = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_eq!(legacy.identity.encode(), original.identity.encode());
    assert!(
        !store::claim_observation(&f.tmux, &legacy, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    f.restart();
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(!f.state().attention);
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_eq!(current.identity.encode(), original.identity.encode());
    assert!(
        !store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn actual_hook_fallback_preserves_the_released_blocked_episode() {
    let Some(mut f) = Fixture::new() else { return };
    f.hook("start", r#"{"session_id":"same"}"#, false);
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let original = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(
        store::claim_observation(&f.tmux, &original, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    let read = f.read();
    assert!(
        store::release_program(&f.tmux, &read, &read.owner().unwrap(), &[])
            .unwrap()
            .won()
    );
    f.hook("blocked", r#"{"session_id":"same"}"#, false);
    let fallback = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_eq!(fallback.identity.encode(), original.identity.encode());
    assert!(!f.state().attention);
    assert!(
        !store::claim_observation(&f.tmux, &fallback, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn fallback_rejects_blocked_history_with_a_reused_pid_and_different_birth() {
    let Some(mut f) = Fixture::new() else { return };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    let read = f.read();
    let mut history =
        model::BlockedHistory::parse(&read.options[model::opt::BLOCKED_HISTORY]).unwrap();
    let mut chain = history.processes.as_slice().to_vec();
    let agent = chain.last_mut().unwrap();
    agent.birth = match agent.birth {
        tma_core::snapshot::ProcessBirth::MacOsStart {
            seconds,
            microseconds,
        } => tma_core::snapshot::ProcessBirth::MacOsStart {
            seconds: seconds.saturating_sub(1),
            microseconds,
        },
        tma_core::snapshot::ProcessBirth::LinuxStartTicks(ticks) => {
            tma_core::snapshot::ProcessBirth::LinuxStartTicks(ticks.saturating_sub(1))
        }
    };
    history.processes = model::ProcessChain::new(chain).unwrap();
    assert!(
        store::release_program(&f.tmux, &read, &read.owner().unwrap(), &[])
            .unwrap()
            .won()
    );
    f.tmux
        .set_pane_option(&f.target(), model::opt::BLOCKED_HISTORY, &history.encode())
        .unwrap();
    let mut fallback = f.plan(AgentState::Blocked, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut fallback {
        publish.episode_reset = false;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &fallback, true)
            .unwrap()
            .committed
    );
    let current = f.read();
    assert_ne!(
        current
            .options
            .get(model::opt::ATTENTION_EPISODE)
            .map(String::as_str),
        Some(history.episode.as_str())
    );
    assert!(!current.options.contains_key(model::opt::BLOCKED_HISTORY));
    assert!(f.state().attention);
}

#[test]
fn replaced_native_process_receives_a_new_blocked_episode() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let old = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(store::claim_observation(&f.tmux, &old, tma_runtime::now_ms)
        .unwrap()
        .claimed());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    assert!(f
        .scratch
        .tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &f.target(),
            "exec /bin/sleep 600"
        ])
        .status
        .success());
    common::poll_until("replacement sleep process", || {
        f.scratch.get(&f.pane, "#{pane_pid}") != f.pid.to_string()
            && f.scratch.get(&f.pane, "#{pane_current_command}") == "sleep"
    });
    f.pid = f.scratch.get(&f.pane, "#{pane_pid}").parse().unwrap();
    f.restart();
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(f.state().attention);
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_ne!(old.identity.encode(), current.identity.encode());
    assert!(
        store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn full_reset_invalidates_a_spent_blocker_even_when_state_and_process_stay_equal() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    let old = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(store::claim_observation(&f.tmux, &old, tma_runtime::now_ms)
        .unwrap()
        .claimed());
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    f.collector.sequence += 1;
    f.collector.events.push(ObservedEvent {
        pane: f.id(),
        connection: ConnectionEpoch::new(f.collector.epoch),
        sequence: ObservationSequence::new(f.collector.sequence),
        event: ProgramEvent::Lifecycle(tma_core::program_status::TerminalLifecycle::FullReset),
    });
    let facts = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let batch = f.runtime.process_batch(
        &mut f.collector,
        &[facts],
        &f.manifests,
        &mut program_processes,
        tma_runtime::now_ms(),
    );
    for release in batch.unstreamed_releases() {
        tma_runtime::program_status::publication::release_persisted(
            &f.tmux, f.session, &f.token, release,
        )
        .unwrap();
    }
    assert!(!f.read().options.contains_key(model::opt::BLOCKED_HISTORY));
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(f.state().attention);
    let current = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert_ne!(old.identity.encode(), current.identity.encode());
    assert!(
        store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
}

#[test]
fn legacy_context_notifications_keep_unique_claims_and_rearming_under_yielding_hooks() {
    let Some(f) = Fixture::new() else {
        return;
    };
    f.hook("start", r#"{"session_id":"same"}"#, false);
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-g",
            "after-set-option",
            "run-shell 'sleep 0.001'"
        ])
        .status
        .success());
    let now = tma_runtime::now_ms();
    let log = f.scratch.workdir.join("context-notifications.jsonl");
    let sinks = tma_runtime::config::NotifySinks {
        log: Some(log.clone()),
        ..Default::default()
    };
    for (at, pct, count) in [(now, 95, 1), (now + 1, 50, 1), (now + 2, 95, 2)] {
        stamp::apply_context(&f.tmux, &[], &f.target(), Some(pct), None, at, true).unwrap();
        let rec = f.tmux.list_panes().unwrap().remove(0);
        tma_runtime::notify::evaluate_context_high(&f.tmux, true, &rec, 90, None, &sinks, at);
        tma_runtime::notify::evaluate_context_high(&f.tmux, true, &rec, 90, None, &sinks, at);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().lines().count(),
            count
        );
    }
}

#[test]
fn removed_pane_revision_tombstone_fences_a_stale_empty_snapshot() {
    let Some(f) = Fixture::new() else {
        return;
    };
    let empty = f.tmux.list_panes().unwrap();
    assert!(!empty[0].options.contains_key(model::opt::RECEIPT));
    f.hook("work", r#"{"session_id":"same"}"#, false);
    let current = f.tmux.list_panes().unwrap();
    assert!(
        stamp::apply(&f.tmux, &current, &f.target(), &StampPlan::Remove, true)
            .unwrap()
            .committed
    );
    let removed = f.read();
    assert!(!removed.options.contains_key(opt::STATE));
    assert!(!removed.options.contains_key(opt::SESSION));
    assert!(!removed.options.contains_key(model::opt::METADATA_UPDATE));
    assert!(Token::parse(&removed.options[model::opt::RECEIPT]).is_some());
    let delayed = f.plan(AgentState::Working, tma_runtime::now_ms());
    assert!(
        !stamp::apply(&f.tmux, &empty, &f.target(), &delayed, true)
            .unwrap()
            .committed
    );
    assert!(!f.read().options.contains_key(opt::STATE));
}

#[test]
fn actual_wait_since_observes_a_new_idle_to_idle_program_result() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let floor = f.state().episode_at();
    let same = f.scratch.tma(&[
        "wait",
        "--pane",
        &f.pane,
        "--until",
        "done",
        "--since",
        &floor.to_string(),
        "--timeout",
        "0",
        "--json",
    ]);
    assert_eq!(
        same.status.code(),
        Some(124),
        "the old result cannot satisfy its own exclusive floor: {}",
        String::from_utf8_lossy(&same.stderr)
    );
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    let since = f.state().since;
    f.reduce("", ProgramStatus::Idle);
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert_eq!(f.state().since, since);
    let episode = f.state().episode_at();
    assert!(episode > floor);
    let next = f.scratch.tma(&[
        "wait",
        "--pane",
        &f.pane,
        "--until",
        "done",
        "--since",
        &floor.to_string(),
        "--timeout",
        "0",
        "--json",
    ]);
    assert!(
        next.status.success(),
        "new idle result is visible to the production wait consumer: {}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert!(String::from_utf8_lossy(&next.stdout).contains(&format!("\"episode_ms\":{episode}")));
}

#[test]
fn stale_displayed_row_cannot_acknowledge_a_newer_result_during_ui_focus() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let shown = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    assert!(shown.acknowledgement.is_some());
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let new_result = f.read().result().unwrap();
    tma_runtime::ui::clear_observed_attention(
        &f.tmux,
        &shown.pane_id,
        shown.acknowledgement.as_ref(),
    )
    .unwrap();
    assert!(f.state().attention);
    assert_ne!(f.read().program.acknowledged, Some(new_result.clone()));
    let current = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    tma_runtime::ui::clear_observed_attention(
        &f.tmux,
        &current.pane_id,
        current.acknowledgement.as_ref(),
    )
    .unwrap();
    assert!(!f.state().attention);
    assert_eq!(f.read().program.acknowledged, Some(new_result));
}

#[test]
fn arrival_hook_acknowledges_a_result_already_visible_before_programmatic_focus() {
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let shown = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    let split = f.scratch.tmux(&[
        "split-window",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        &f.target(),
        "exec /bin/sleep 600",
    ]);
    assert!(split.status.success());
    assert_eq!(
        f.scratch.attach_client("owned"),
        common::AttachOutcome::Attached
    );
    let binary = common::tma_bin();
    let handler = f.scratch.workdir.join("focus-handler");
    std::fs::write(
        &handler,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\" --socket-name '{}'\n",
            binary, f.scratch.socket
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let hook = tma_runtime::tmux_hooks::clear_attention_command(&handler, "after-select-pane");
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-select-pane", &hook])
        .status
        .success());
    f.reduce("", ProgramStatus::Working { progress: None });
    assert!(f.publish().won());
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let new_result = f.read().result().unwrap();
    tma_runtime::ui::focus_pane(
        &f.tmux,
        None,
        &shown.session,
        &format!("{}:{}", shown.session, shown.window_index),
        &shown.pane_id,
    )
    .unwrap();
    tma_runtime::ui::clear_observed_attention(
        &f.tmux,
        &shown.pane_id,
        shown.acknowledgement.as_ref(),
    )
    .unwrap();
    assert!(
        !f.state().attention,
        "the actual arrival presents the already-visible result"
    );
    assert_eq!(f.read().program.acknowledged, Some(new_result));
}

#[test]
fn delayed_focus_hook_cannot_acknowledge_result_published_after_navigation() {
    use std::os::unix::fs::PermissionsExt;
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let shown = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    assert!(f
        .scratch
        .tmux(&["split-window", "-t", &f.target(), "exec /bin/sleep 600"])
        .status
        .success());
    assert_eq!(
        f.scratch.attach_client("owned"),
        common::AttachOutcome::Attached
    );
    let handler = f.scratch.workdir.join("paused-focus");
    let ready = f.scratch.workdir.join("focus-ready");
    let gate = f.scratch.workdir.join("focus-release");
    let done = f.scratch.workdir.join("focus-done");
    std::fs::write(&handler, format!(
        "#!/bin/sh\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nstatus=125\nif [ -e '{}' ]; then\n'{}' \"$@\" --socket-name '{}'\nstatus=$?\nfi\nprintf '%s\\n' \"$status\" > '{}'\nexit \"$status\"\n",
        ready.display(), gate.display(), gate.display(), common::tma_bin(), f.scratch.socket, done.display(),
    )).unwrap();
    std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o700)).unwrap();
    let hook = tma_runtime::tmux_hooks::clear_attention_command(&handler, "after-select-pane");
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-select-pane", &hook])
        .status
        .success());
    let focus_tmux = Tmux::connect(&Server::named(Some(f.scratch.socket.clone())));
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            tma_runtime::ui::focus_pane(
                &focus_tmux,
                None,
                &shown.session,
                &format!("{}:{}", shown.session, shown.window_index),
                &shown.pane_id,
            )
            .unwrap()
        });
        common::poll_until("focus occurred before its delayed handler read", || {
            ready.exists() && f.scratch.displayed_pane() == f.pane
        });
        f.reduce("", ProgramStatus::Working { progress: None });
        assert!(f.publish().won());
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        let later = f.read().result().unwrap();
        std::fs::write(&gate, "resume").unwrap();
        pending.join().unwrap();
        common::poll_until("delayed focus handler completed after release", || {
            std::fs::read_to_string(&done).is_ok_and(|status| !status.is_empty())
        });
        assert_eq!(std::fs::read_to_string(&done).unwrap().trim(), "0");
        assert!(
            f.state().attention,
            "navigation did not present the later result"
        );
        assert_ne!(f.read().program.acknowledged, Some(later));
    });
}

#[test]
fn delayed_departure_hooks_keep_the_captured_pane_and_result_for_both_navigation_kinds() {
    use std::os::unix::fs::PermissionsExt;
    for hook_kind in ["after-select-pane", "session-window-changed"] {
        let Some(mut f) = Fixture::new() else { return };
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        let command = if hook_kind == "after-select-pane" {
            "split-window"
        } else {
            "new-window"
        };
        let creation_target = if command == "new-window" {
            "owned".to_owned()
        } else {
            f.target()
        };
        let added = f.scratch.tmux(&[
            command,
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &creation_target,
            "exec /bin/sleep 600",
        ]);
        assert!(added.status.success());
        let other = String::from_utf8(added.stdout).unwrap().trim().to_owned();
        let window = f.scratch.get(&other, "#{session_name}:#{window_index}");
        assert_eq!(
            f.scratch.attach_client("owned"),
            common::AttachOutcome::Attached
        );
        let handler = f.scratch.workdir.join("paused-departure");
        let ready = f.scratch.workdir.join("departure-ready");
        let gate = f.scratch.workdir.join("departure-release");
        let done = f.scratch.workdir.join("departure-done");
        std::fs::write(&handler, format!(
            "#!/bin/sh\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nstatus=125\nif [ -e '{}' ]; then\n'{}' \"$@\" --socket-name '{}'\nstatus=$?\nfi\nprintf '%s\\n' \"$status\" > '{}'\nexit \"$status\"\n",
            ready.display(), gate.display(), gate.display(), common::tma_bin(), f.scratch.socket, done.display(),
        )).unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o700)).unwrap();
        let hook = tma_runtime::tmux_hooks::clear_attention_command(&handler, hook_kind);
        assert!(f
            .scratch
            .tmux(&["set-hook", "-g", hook_kind, &hook])
            .status
            .success());
        let focus_tmux = Tmux::connect(&Server::named(Some(f.scratch.socket.clone())));
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                tma_runtime::ui::focus_pane(&focus_tmux, None, "owned", &window, &other).unwrap()
            });
            common::poll_until("departure dispatch captured its old presentation", || {
                ready.exists() && f.scratch.displayed_pane() == other
            });
            f.reduce("", ProgramStatus::Working { progress: None });
            assert!(f.publish().won());
            f.reduce("", ProgramStatus::Done);
            assert!(f.publish().won());
            let later = f.read().result().unwrap();
            std::fs::write(&gate, "resume").unwrap();
            pending.join().unwrap();
            common::poll_until(
                "delayed departure handler has checked the later result",
                || std::fs::read_to_string(&done).is_ok_and(|status| !status.is_empty()),
            );
            assert_eq!(std::fs::read_to_string(&done).unwrap().trim(), "0");
            assert!(
                f.state().attention,
                "{hook_kind} consumed a post-dispatch result"
            );
            assert_ne!(f.read().program.acknowledged, Some(later));
        });
    }
}

#[test]
fn generated_departure_hooks_acknowledge_the_original_linked_target_after_later_navigation() {
    use std::os::unix::fs::PermissionsExt;
    use tma_tmux::tmux::DepartureKind;

    for hook_kind in ["after-select-pane", "session-window-changed"] {
        let Some(mut f) = Fixture::new() else { return };
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        let result = f.read().result().unwrap();
        let creation = if hook_kind == "after-select-pane" {
            "split-window"
        } else {
            "new-window"
        };
        let creation_target = if creation == "new-window" {
            "owned".to_owned()
        } else {
            f.target()
        };
        let mut others = Vec::new();
        for _ in 0..2 {
            let added = f.scratch.tmux(&[
                creation,
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &creation_target,
                "exec /bin/sleep 600",
            ]);
            assert!(added.status.success());
            others.push(String::from_utf8(added.stdout).unwrap().trim().to_owned());
        }
        assert!(f
            .scratch
            .tmux(&["new-session", "-d", "-s", "linked", "-t", "owned"])
            .status
            .success());
        let linked = SessionId::parse(&f.scratch.get("linked", "#{session_id}")).unwrap();
        let original = format!("${}:.{}", linked.get(), f.pane);
        let other = format!("${}:.{}", linked.get(), others[0]);
        let third = format!("${}:.{}", linked.get(), others[1]);
        assert_eq!(
            f.scratch.attach_client("linked"),
            common::AttachOutcome::Attached
        );
        assert_eq!(f.scratch.displayed_pane(), f.pane);

        let handler = f.scratch.workdir.join("captured-departure");
        let ready = f.scratch.workdir.join("departure-ready");
        let gate = f.scratch.workdir.join("departure-release");
        let capture = f.scratch.workdir.join("departure-capture");
        let done = f.scratch.workdir.join("departure-done");
        std::fs::write(&handler, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$TMA_DEPARTURE_REVISION\" > '{}'\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 1000 ]; do sleep 0.01; i=$((i+1)); done\nstatus=125\nif [ -e '{}' ]; then\n'{}' \"$@\" --socket-name '{}'\nstatus=$?\nfi\nprintf '%s\\n' \"$status\" > '{}'\nexit \"$status\"\n",
            capture.display(), ready.display(), gate.display(), gate.display(), common::tma_bin(), f.scratch.socket, done.display(),
        )).unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o700)).unwrap();
        let hook = tma_runtime::tmux_hooks::clear_attention_command(&handler, hook_kind);
        assert!(f
            .scratch
            .tmux(&["set-hook", "-g", hook_kind, &hook])
            .status
            .success());
        let context_hook = format!("if-shell -F '#{{&&:#{{!=:#{{@tma_program_ack}},}},#{{==:#{{@focus_context}},}}}}' 'set-option -p -t {original} @focus_context linked'");
        assert!(f
            .scratch
            .tmux(&[
                "set-hook",
                "-t",
                &format!("${}", linked.get()),
                "after-set-option",
                &context_hook,
            ])
            .status
            .success());

        let navigation = if hook_kind == "after-select-pane" {
            "select-pane"
        } else {
            "select-window"
        };
        let socket = f.scratch.socket.clone();
        std::thread::scope(|scope| {
            let pending =
                scope.spawn(|| common::scratch_tmux(&socket, &[navigation, "-t", &other]));
            common::poll_until(
                "generated departure has captured the original linked pane",
                || ready.exists() && f.scratch.displayed_pane() == others[0],
            );
            let captured = std::fs::read_to_string(&capture).unwrap();
            let revision = model::AttentionRevision::parse(captured.trim_end())
                .unwrap_or_else(|| panic!("{hook_kind} capture: {captured:?}\nhook: {hook}"));
            assert_eq!(revision.session(), linked);
            assert_eq!(revision.pane(), f.id());
            assert!(revision.matches(&f.read().options));

            assert!(f
                .scratch
                .tmux(&["set-hook", "-gu", hook_kind])
                .status
                .success());
            f.tmux.set_pane_option(&other, opt::STATE, "idle").unwrap();
            f.tmux.set_pane_option(&other, opt::ATTENTION, "1").unwrap();
            assert!(f.scratch.tmux(&[navigation, "-t", &third]).status.success());
            assert_eq!(
                f.tmux
                    .departed_pane(&third, DepartureKind::from_hook_name(hook_kind).unwrap())
                    .unwrap(),
                Some(others[0].clone()),
                "the current departure target has moved while the first handler waits",
            );
            std::fs::write(&gate, "resume").unwrap();
            let completed = pending.join().unwrap();
            assert!(completed.status.success());
            common::poll_until("asynchronous departure handler completed its ACK", || {
                std::fs::read_to_string(&done).is_ok_and(|status| !status.is_empty())
            });
            assert_eq!(std::fs::read_to_string(&done).unwrap().trim(), "0");
            assert!(
                !f.state().attention,
                "{hook_kind} must ACK its valid original capture"
            );
            assert_eq!(f.read().program.acknowledged, Some(result));
            assert_eq!(f.scratch.get(&other, "#{@agent_attention}"), "1");
            assert_eq!(f.scratch.get(&original, "#{@focus_context}"), "linked");
        });
    }
}

#[test]
fn captured_focus_revision_false_misses_on_notify_or_metadata_refresh_and_preserves_session_context(
) {
    use model::AttentionRevision;
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let capture = || {
        AttentionRevision::parse(
            &f.tmux
                .pane_format(&f.target(), &AttentionRevision::capture_format())
                .unwrap(),
        )
        .unwrap()
    };
    let before_notify = capture();
    let observation = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
    assert!(
        store::claim_observation(&f.tmux, &observation, tma_runtime::now_ms)
            .unwrap()
            .claimed()
    );
    tma_runtime::seen::acknowledge_revision(&f.tmux, &before_notify).unwrap();
    assert!(f.state().attention);
    let before_metadata = capture();
    f.hook("start", r#"{"session_id":"same","model":"m"}"#, false);
    tma_runtime::seen::acknowledge_revision(&f.tmux, &before_metadata).unwrap();
    assert!(f.state().attention);
    assert!(f
        .scratch
        .tmux(&["new-session", "-d", "-s", "linked", "-t", "owned"])
        .status
        .success());
    let linked = SessionId::parse(&f.scratch.get("linked", "#{session_id}")).unwrap();
    let target = format!("${}:.{}", linked.get(), f.pane);
    let revision = AttentionRevision::parse(
        &f.tmux
            .pane_format(&target, &AttentionRevision::capture_format())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(revision.session(), linked);
    let hook = format!("if-shell -F '#{{&&:#{{!=:#{{@tma_program_ack}},}},#{{==:#{{@focus_context}},}}}}' 'set-option -p -t {target} @focus_context linked'");
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-t",
            &format!("${}", linked.get()),
            "after-set-option",
            &hook
        ])
        .status
        .success());
    tma_runtime::seen::acknowledge_revision(&f.tmux, &revision).unwrap();
    assert!(!f.state().attention);
    assert_eq!(f.scratch.get(&target, "#{@focus_context}"), "linked");
    assert_eq!(f.read().program.acknowledged, f.read().result());
}

#[test]
fn captured_focus_tuple_stops_a_yielding_fallback_suffix_even_when_its_receipt_is_unchanged() {
    let Some(f) = Fixture::new() else { return };
    f.hook("work", r#"{"session_id":"same"}"#, false);
    let gate = f.scratch.workdir.join("focus-fallback-release");
    let hook = format!("if-shell -F '#{{&&:#{{==:#{{@agent_state}},idle}},#{{==:#{{@agent_attention}},1}}}}' \"run-shell 'i=0; while [ ! -e {} ] && [ \\$i -lt 200 ]; do sleep 0.01; i=\\$((i+1)); done'\"", gate.display());
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-set-option", &hook])
        .status
        .success());
    let mut plan = f.plan(AgentState::Idle, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut plan {
        publish.episode_reset = false;
    }
    let panes = f.tmux.list_panes().unwrap();
    std::thread::scope(|scope| {
        let pending =
            scope.spawn(|| stamp::apply(&f.tmux, &panes, &f.target(), &plan, true).unwrap());
        common::poll_until(
            "advisory attention precedes the new stable episode token",
            || {
                f.scratch.pane_option(&f.pane, opt::STATE) == "idle"
                    && f.scratch.pane_option(&f.pane, opt::ATTENTION) == "1"
            },
        );
        let during = f.read();
        let revision = model::AttentionRevision::parse(
            &f.tmux
                .pane_format(&f.target(), &model::AttentionRevision::capture_format())
                .unwrap(),
        )
        .unwrap();
        std::fs::write(&gate, "resume").unwrap();
        pending.join().unwrap();
        let current = f.read();
        assert_eq!(
            during.options.get(model::opt::RECEIPT),
            current.options.get(model::opt::RECEIPT)
        );
        assert!(!revision.matches(&current.options));
        tma_runtime::seen::acknowledge_revision(&f.tmux, &revision).unwrap();
        assert!(f.state().attention);
    });
}

#[test]
fn invalid_hook_capture_is_one_bounded_argument_and_cannot_fall_back_to_a_later_result() {
    use std::os::unix::fs::PermissionsExt;
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(f
        .scratch
        .tmux(&["split-window", "-t", &f.target(), "exec /bin/sleep 600"])
        .status
        .success());
    assert_eq!(
        f.scratch.attach_client("owned"),
        common::AttachOutcome::Attached
    );
    let handler = f.scratch.workdir.join("invalid-focus");
    let ready = f.scratch.workdir.join("invalid-focus-ready");
    let gate = f.scratch.workdir.join("invalid-focus-release");
    let capture = f.scratch.workdir.join("invalid-capture");
    let done = f.scratch.workdir.join("invalid-focus-done");
    std::fs::write(&handler, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$TMA_ATTENTION_REVISION\" > '{}'\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nstatus=125\nif [ -e '{}' ]; then\n'{}' \"$@\" --socket-name '{}'\nstatus=$?\nfi\nprintf '%s\\n' \"$status\" > '{}'\nexit \"$status\"\n",
        capture.display(), ready.display(), gate.display(), gate.display(), common::tma_bin(), f.scratch.socket, done.display(),
    )).unwrap();
    std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o700)).unwrap();
    let hook = tma_runtime::tmux_hooks::clear_attention_command(&handler, "after-select-pane");
    assert!(f
        .scratch
        .tmux(&["set-hook", "-g", "after-select-pane", &hook])
        .status
        .success());
    f.tmux
        .set_pane_option(&f.target(), opt::SINCE, "'\"|$IFS\nquoted")
        .unwrap();
    let focus_tmux = Tmux::connect(&Server::named(Some(f.scratch.socket.clone())));
    let target_pane = f.pane.clone();
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            tma_runtime::ui::focus_pane(&focus_tmux, None, "owned", "owned:0", &target_pane)
                .unwrap()
        });
        common::poll_until(
            "invalid dispatch capture reaches its handler safely",
            || ready.exists(),
        );
        let captured = std::fs::read_to_string(&capture).unwrap();
        assert_eq!(captured.lines().count(), 1);
        assert!(captured.len() <= 257);
        assert!(captured.starts_with(&format!("${}|{}|", f.session.get(), f.pane)));
        assert!(captured.contains('|') && captured.contains('!'));
        assert!(model::AttentionRevision::parse(captured.trim_end()).is_none());
        f.reduce("", ProgramStatus::Working { progress: None });
        assert!(f.publish().won());
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        std::fs::write(&gate, "resume").unwrap();
        pending.join().unwrap();
        common::poll_until("invalid capture handler completed after release", || {
            std::fs::read_to_string(&done).is_ok_and(|status| !status.is_empty())
        });
        assert_eq!(std::fs::read_to_string(&done).unwrap().trim(), "0");
        assert!(f.state().attention);
        assert_ne!(f.read().program.acknowledged, f.read().result());
    });
}

#[test]
fn known_old_hook_without_witness_skips_program_ack_while_explicit_clear_still_works() {
    let Some(mut f) = Fixture::new() else { return };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    for captured in [None, Some("invalid")] {
        let mut command = f.command();
        command
            .args(["clear-attention", &f.pane])
            .env(tma_runtime::tmux_hooks::HOOK_KIND_ENV, "after-select-pane")
            .env_remove(tma_runtime::tmux_hooks::ATTENTION_REVISION_ENV)
            .env_remove(tma_runtime::tmux_hooks::DEPARTURE_REVISION_ENV);
        if let Some(captured) = captured {
            command.env(tma_runtime::tmux_hooks::ATTENTION_REVISION_ENV, captured);
        }
        assert!(command.output().unwrap().status.success());
        assert!(f.state().attention);
    }
    assert!(f
        .command()
        .args(["clear-attention", &f.pane])
        .env_remove(tma_runtime::tmux_hooks::HOOK_KIND_ENV)
        .env_remove(tma_runtime::tmux_hooks::ATTENTION_REVISION_ENV)
        .env_remove(tma_runtime::tmux_hooks::DEPARTURE_REVISION_ENV)
        .output()
        .unwrap()
        .status
        .success());
    assert!(!f.state().attention);
    assert_eq!(f.read().program.acknowledged, f.read().result());
}

#[test]
fn stale_program_attention_does_not_become_a_fallback_completion() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: None,
            progress: None,
        },
    );
    assert!(f.publish().won());
    assert!(f.state().attention);
    store::release_collector(&f.tmux, &f.target(), &f.token).unwrap();
    let mut fallback = f.plan(AgentState::Idle, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut fallback {
        publish.episode_reset = false;
        publish.set_attention = false;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &fallback, true)
            .unwrap()
            .committed
    );
    let row = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    assert_eq!(row.state, AgentState::Idle);
    assert!(
        !row.attention,
        "an expired blocker is not a new fallback completion"
    );
}

#[test]
fn no_attention_fallback_preserves_unacknowledged_result_recovery_and_new_work() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    let turn = f.state().turn_at;
    let since = f.state().since;
    store::release_collector(&f.tmux, &f.target(), &f.token).unwrap();
    let mut fallback = f.plan(AgentState::Idle, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut fallback {
        publish.episode_reset = false;
        publish.set_attention = false;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &fallback, true)
            .unwrap()
            .committed
    );
    assert!(!f.state().attention);
    assert_eq!(f.state().since, since);
    assert_eq!(f.state().turn_at, turn);
    f.restart();
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(
        f.state().attention,
        "fallback absence is not durable acknowledgement"
    );
    assert_eq!(f.state().episode_at(), turn.max(since));
    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
    f.restart();
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(!f.state().attention, "exact ACK remains spent");
    store::release_collector(&f.tmux, &f.target(), &f.token).unwrap();
    let mut working = f.plan(AgentState::Working, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut working {
        publish.episode_reset = false;
        publish.set_attention = false;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &working, true)
            .unwrap()
            .committed
    );
    assert!(f.read().result().is_none());
    f.restart();
    f.reduce("", ProgramStatus::Done);
    assert!(f.publish().won());
    assert!(
        f.state().attention,
        "observed fallback work opens a new result"
    );
    assert!(f.state().turn_at > turn);
}

#[test]
fn replacement_cannot_reuse_the_previous_process_request_metadata() {
    use tma_runtime::broker::{self, BrokerIo};
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: Some(tma_core::program_status::BlockedKind::Permission),
            progress: None,
        },
    );
    assert!(f.publish().won());
    f.hook("blocked", r#"{"session_id":"old","request_id":"old_request","tool_name":"Bash","tool_use_id":"old_call"}"#, false);
    assert!(f.read().options.contains_key(opt::PERMISSION_REQUEST));
    assert!(f
        .scratch
        .tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &f.target(),
            "exec /bin/sleep 600"
        ])
        .status
        .success());
    common::poll_until("replacement process for metadata", || {
        f.scratch.get(&f.pane, "#{pane_pid}") != f.pid.to_string()
            && f.scratch.get(&f.pane, "#{pane_current_command}") == "sleep"
    });
    f.pid = f.scratch.get(&f.pane, "#{pane_pid}").parse().unwrap();
    f.restart();
    f.reduce(
        "",
        ProgramStatus::Blocked {
            kind: Some(tma_core::program_status::BlockedKind::Permission),
            progress: None,
        },
    );
    assert!(f.publish().won());
    let row = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    assert!(row.permission_request.is_none());
    assert!(row.pending.is_none());
    let cfg = FoldConfig::default();
    let api = tma_runtime::config::ApiSection::default();
    let io = broker::TmuxBroker {
        tmux: &f.tmux,
        manifests: &f.manifests,
        cfg: &cfg,
        api_bases: &api,
        server: Server::named(Some(f.scratch.socket.clone())),
        notify_command: None,
    };
    let facts = io.read_pane(&f.pane).unwrap().unwrap();
    assert!(facts.permission_request.is_none());
    assert!(facts.session.is_none());
    f.hook("start", r#"{"session_id":"new"}"#, false);
    f.hook(
        "blocked",
        r#"{"session_id":"new","request_id":"new_request"}"#,
        false,
    );
    assert_eq!(
        io.read_pane(&f.pane)
            .unwrap()
            .unwrap()
            .permission_request
            .as_deref(),
        Some("new_request")
    );
}

#[test]
fn delayed_reaper_marker_cannot_attach_to_a_replacement_legacy_episode() {
    let Some(f) = Fixture::new() else { return };
    for yielding in [false, true] {
        f.hook("start", r#"{"session_id":"old"}"#, false);
        let original = f.read();
        let marker = tma_core::render::set_pane_option(&f.target(), opt::REG_DEAD_SINCE, "1");
        f.hook("end", r#"{"session_id":"old"}"#, false);
        f.hook("start", r#"{"session_id":"new"}"#, false);
        if yielding {
            assert!(f
                .scratch
                .tmux(&[
                    "set-hook",
                    "-g",
                    "after-set-option",
                    "run-shell 'sleep 0.001'",
                ])
                .status
                .success());
        }
        let applied = stamp::apply_commands(&f.tmux, &original, &[marker]).unwrap();
        assert!(
            !applied.committed,
            "the original episode lost its marker write"
        );
        assert!(!applied.read.options.contains_key(opt::REG_DEAD_SINCE));
        assert_eq!(
            applied.read.options.get(opt::SESSION).map(String::as_str),
            Some("new")
        );
        assert_ne!(
            original.options.get(model::opt::RECEIPT),
            applied.read.options.get(model::opt::RECEIPT)
        );
        f.scratch.tmux(&["set-hook", "-gu", "after-set-option"]);
        f.hook("end", r#"{"session_id":"new"}"#, false);
    }
}

#[test]
fn delayed_telemetry_snapshot_cannot_update_a_replacement_metadata_session() {
    let Some(f) = Fixture::new() else { return };
    f.hook("start", r#"{"session_id":"old"}"#, false);
    let original = f.tmux.list_panes().unwrap();
    f.hook("end", r#"{"session_id":"old"}"#, false);
    f.hook(
        "start",
        r#"{"session_id":"new","model":"new-model"}"#,
        false,
    );
    for yielding in [false, true] {
        if yielding {
            assert!(f
                .scratch
                .tmux(&[
                    "set-hook",
                    "-g",
                    "after-set-option",
                    "run-shell 'sleep 0.001'",
                ])
                .status
                .success());
        }
        stamp::apply_context(
            &f.tmux,
            &original,
            &f.target(),
            Some(99),
            Some(123),
            tma_runtime::now_ms(),
            true,
        )
        .unwrap();
        stamp::apply_quota(
            &f.tmux,
            &original,
            &f.target(),
            &tma_core::QuotaStamp {
                pct: Some(90),
                window: Some("5h"),
                resets_at_ms: None,
                cost_usd: None,
            },
            tma_runtime::now_ms(),
            true,
        )
        .unwrap();
        assert!(!stamp::apply_metadata_edits(
            &f.tmux,
            &original,
            &f.target(),
            &[OptionEdit::set(opt::MODEL, "old-model")],
        )
        .unwrap());
        let current = f.read();
        assert!(!current.options.contains_key(opt::CONTEXT_PCT));
        assert!(!current.options.contains_key(opt::QUOTA_PCT));
        assert_eq!(
            current.options.get(opt::SESSION).map(String::as_str),
            Some("new")
        );
        assert_eq!(
            current.options.get(opt::MODEL).map(String::as_str),
            Some("new-model")
        );
    }
}

#[test]
fn one_metadata_snapshot_can_publish_context_quota_and_model_in_sequence() {
    let Some(f) = Fixture::new() else { return };
    f.hook("start", r#"{"session_id":"same"}"#, false);
    let original = f.tmux.list_panes().unwrap();
    stamp::apply_context(
        &f.tmux,
        &original,
        &f.target(),
        Some(50),
        Some(100),
        100,
        true,
    )
    .unwrap();
    stamp::apply_quota(
        &f.tmux,
        &original,
        &f.target(),
        &tma_core::QuotaStamp {
            pct: Some(80),
            window: Some("5h"),
            resets_at_ms: None,
            cost_usd: None,
        },
        100,
        true,
    )
    .unwrap();
    assert!(stamp::apply_metadata_edits(
        &f.tmux,
        &original,
        &f.target(),
        &[OptionEdit::set(opt::MODEL, "same-model"),]
    )
    .unwrap());
    let current = f.read();
    assert_eq!(
        current.options.get(opt::CONTEXT_PCT).map(String::as_str),
        Some("50")
    );
    assert_eq!(
        current.options.get(opt::QUOTA_PCT).map(String::as_str),
        Some("80")
    );
    assert_eq!(
        current.options.get(opt::MODEL).map(String::as_str),
        Some("same-model")
    );
}

#[test]
fn delayed_subagent_update_cannot_attach_to_a_replacement_parent_session() {
    use std::os::unix::fs::PermissionsExt;
    let Some(f) = Fixture::new() else { return };
    f.hook("start", r#"{"session_id":"old-parent"}"#, false);
    let wrapper = f.scratch.workdir.join("pause-subagent");
    let ready = f.scratch.workdir.join("subagent-ready");
    let gate = f.scratch.workdir.join("subagent-release");
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nfor arg do\nif [ \"$arg\" = '#{{pane_id}}' ] && [ ! -e '{}' ]; then\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nfi\ndone\nexec tmux \"$@\"\n", ready.display(), ready.display(), gate.display(),
    )).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let delayed = Tmux::connect(&Server {
        socket_name: Some(f.scratch.socket.clone()),
        bin: Some(wrapper.to_string_lossy().into_owned()),
        ..Default::default()
    });
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            let commands = tma_runtime::config::NotifyCommands::default();
            let sinks = tma_runtime::config::NotifySinks::default();
            apply_event(
                &delayed,
                &f.manifests[0],
                &f.target(),
                tma_runtime::manifests::SUBAGENT_START,
                r#"{"session_id":"old-child"}"#,
                &NotifyPolicy {
                    opt_in: false,
                    on: &[],
                    commands: &commands,
                    sinks: &sinks,
                },
                tma_runtime::now_ms(),
            )
        });
        common::poll_until("subagent update has read its original parent", || {
            ready.exists()
        });
        f.hook("end", r#"{"session_id":"old-parent"}"#, false);
        f.hook("start", r#"{"session_id":"new-parent"}"#, false);
        std::fs::write(&gate, "resume").unwrap();
        pending.join().unwrap();
    });
    let current = f.read();
    assert_eq!(
        current.options.get(opt::SESSION).map(String::as_str),
        Some("new-parent")
    );
    assert!(
        !current.options.contains_key(opt::SUBAGENTS),
        "the old child belongs only to the original parent observation"
    );
}

#[test]
fn telemetry_cannot_bind_an_unmarked_process_outside_the_target_pane() {
    let Some(f) = Fixture::new() else { return };
    f.tmux
        .set_pane_option(&f.target(), opt::PID, &std::process::id().to_string())
        .unwrap();
    f.tmux
        .set_pane_option(&f.target(), opt::SESSION, "foreign-process")
        .unwrap();
    let panes = f.tmux.list_panes().unwrap();
    stamp::apply_context(&f.tmux, &panes, &f.target(), Some(99), None, 100, true).unwrap();
    assert!(!f.read().options.contains_key(model::opt::METADATA_OWNER));
    assert!(!f.read().options.contains_key(opt::CONTEXT_PCT));
}

#[test]
fn written_legacy_claim_cannot_authorize_a_replacement_or_cleared_presentation() {
    use std::os::unix::fs::PermissionsExt;
    for clear in [false, true] {
        let Some(f) = Fixture::new() else { return };
        f.hook("blocked", r#"{"session_id":"current"}"#, false);
        let observation = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
        let wrapper = f.scratch.workdir.join("hold-written-claim");
        let ready = f.scratch.workdir.join("claim-written");
        let gate = f.scratch.workdir.join("claim-readback-release");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
set -eu
for arg do
    case "$arg" in
        *'@tma_episode_notified'*'|legacy|'*)
            if [ ! -e '{ready}' ]; then
                tmux "$@"
                touch '{ready}'
                i=0
                while [ ! -e '{gate}' ] && [ "$i" -lt 150 ]; do sleep 0.01; i=$((i+1)); done
                exit 0
            fi
            ;;
    esac
done
exec tmux "$@"
"#,
                ready = ready.display(),
                gate = gate.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&Server {
            socket_name: Some(f.scratch.socket.clone()),
            bin: Some(wrapper.to_string_lossy().into_owned()),
            ..Default::default()
        });
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                store::claim_observation(&delayed, &observation, tma_runtime::now_ms).unwrap()
            });
            common::poll_until("unique legacy claim was written before readback", || {
                ready.exists()
            });
            let claim = f.read().options[model::opt::EPISODE_NOTIFIED].clone();
            if clear {
                tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
            } else {
                f.hook("work", r#"{"session_id":"current"}"#, false);
            }
            assert_eq!(f.read().options[model::opt::EPISODE_NOTIFIED], claim);
            std::fs::write(&gate, "resume").unwrap();
            assert!(
                !pending.join().unwrap().claimed(),
                "an old unique claim must not grant an effect after replacement or clear"
            );
        });
    }
}

#[test]
fn written_program_result_claim_requires_its_current_unacknowledged_live_result() {
    use std::os::unix::fs::PermissionsExt;
    for change in ["metadata", "ack", "replace", "invalidate", "expire"] {
        let Some(mut f) = Fixture::new() else { return };
        f.hook("start", r#"{"session_id":"current"}"#, false);
        f.reduce("", ProgramStatus::Done);
        assert!(f.publish().won());
        let observation = AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
        let result = observation.read.result().unwrap();
        let wrapper = f.scratch.workdir.join("hold-written-result-claim");
        let ready = f.scratch.workdir.join("result-claim-written");
        let gate = f.scratch.workdir.join("result-claim-release");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
set -eu
for arg do
    case "$arg" in
        *"'set-option' "*'@tma_program_notified'*)
            if [ ! -e '{ready}' ]; then
                tmux "$@"
                touch '{ready}'
                i=0
                while [ ! -e '{gate}' ] && [ "$i" -lt 150 ]; do sleep 0.01; i=$((i+1)); done
                exit 0
            fi
            ;;
    esac
done
exec tmux "$@"
"#,
                ready = ready.display(),
                gate = gate.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&Server {
            socket_name: Some(f.scratch.socket.clone()),
            bin: Some(wrapper.to_string_lossy().into_owned()),
            ..Default::default()
        });
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                store::claim_observation(&delayed, &observation, tma_runtime::now_ms).unwrap()
            });
            common::poll_until("unique program claim was written before readback", || {
                ready.exists()
            });
            let claim = f.read().program.notified.unwrap();
            match change {
                "ack" => {
                    tma_runtime::seen::acknowledge_current(&f.tmux, &f.target()).unwrap();
                }
                "replace" => {
                    f.reduce("", ProgramStatus::Working { progress: None });
                    assert!(f.publish().won());
                    f.reduce("", ProgramStatus::Done);
                    assert!(f.publish().won());
                }
                "expire" => {
                    f.tmux
                        .set_server_option(
                            model::opt::COLLECTOR,
                            &model::CollectorLease {
                                token: f.token.clone(),
                                expires_at: tma_runtime::now_ms() - 1,
                            }
                            .encode(),
                        )
                        .unwrap();
                }
                _ => {
                    if change == "invalidate" {
                        f.tmux
                            .set_pane_option(&f.target(), model::opt::INVALIDATED, &result.encode())
                            .unwrap();
                    }
                    assert!(stamp::apply_metadata_edits(
                        &f.tmux,
                        &f.tmux.list_panes().unwrap(),
                        &f.target(),
                        &[OptionEdit::set(opt::MODEL, "same-owner-refresh")]
                    )
                    .unwrap());
                }
            }
            if change == "metadata" {
                assert_eq!(f.read().program.notified.as_ref(), Some(&claim));
            }
            std::fs::write(&gate, "resume").unwrap();
            let outcome = pending.join().unwrap();
            assert_eq!(outcome.claimed(), change == "metadata", "{change}");
            if change == "metadata" {
                assert!(matches!(
                    outcome,
                    store::NotificationOutcome::Claimed { grouped: false, .. }
                ));
                let current =
                    AttentionObservation::from_read(f.read(), tma_runtime::now_ms()).unwrap();
                assert!(
                    !store::claim_observation(&f.tmux, &current, tma_runtime::now_ms)
                        .unwrap()
                        .claimed(),
                    "a later caller does not inherit this invocation's unique claim"
                );
            }
        });
    }
}

#[test]
fn cold_hook_then_child_capture_and_next_hook_preserve_independent_metadata() {
    let Some(f) = Fixture::new() else { return };
    assert!(f
        .scratch
        .tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &f.target(),
            "exec sh -c '/bin/sleep 600 & wait'"
        ])
        .status
        .success());
    let mut child = None;
    common::poll_until("native child agent is alive under the pane shell", || {
        let root: u32 = f.scratch.get(&f.target(), "#{pane_pid}").parse().unwrap();
        child = program_processes()
            .unwrap()
            .processes()
            .iter()
            .find(|process| {
                process.pid != root
                    && process.comm == "sleep"
                    && tma_tmux::tmux::sample_process_chain(root, process.pid).is_ok()
            })
            .map(|process| process.pid);
        child.is_some()
    });
    let child = child.unwrap();
    // An earlier unattributed statusline can bind the shell before SessionStart.
    stamp::apply_context(
        &f.tmux,
        &f.tmux.list_panes().unwrap(),
        &f.target(),
        Some(10),
        None,
        10,
        true,
    )
    .unwrap();
    f.hook("start", r#"{"session_id":"child-session","model":"child-model","api_endpoint":"http://127.0.0.1:4321","transcript_path":"/tmp/child-transcript"}"#, false);
    assert_eq!(
        f.read().options.get(opt::MODEL).map(String::as_str),
        Some("child-model")
    );
    let original = f.tmux.list_panes().unwrap();
    stamp::apply_context(
        &f.tmux,
        &original,
        &f.target(),
        Some(50),
        Some(100),
        100,
        true,
    )
    .unwrap();
    stamp::apply_quota(
        &f.tmux,
        &original,
        &f.target(),
        &tma_core::QuotaStamp {
            pct: Some(80),
            window: Some("5h"),
            resets_at_ms: None,
            cost_usd: None,
        },
        100,
        true,
    )
    .unwrap();
    {
        use tma_runtime::broker::{BrokerIo, TmuxBroker};
        let cfg = FoldConfig::default();
        let api = tma_runtime::config::ApiSection::default();
        let broker = TmuxBroker {
            tmux: &f.tmux,
            manifests: &f.manifests,
            cfg: &cfg,
            api_bases: &api,
            server: Server::named(Some(f.scratch.socket.clone())),
            notify_command: None,
        };
        let facts = broker.read_pane(&f.pane).unwrap().unwrap();
        assert!(
            facts.pid.is_none(),
            "capture has not published the real PID yet"
        );
        broker.set_act_repeat(&f.pane, &facts, "same-episode");
        assert_eq!(
            f.read().options.get(opt::MODEL).map(String::as_str),
            Some("child-model")
        );
    }
    let mut capture = f.plan(AgentState::Working, tma_runtime::now_ms());
    if let StampPlan::Publish(publish) = &mut capture {
        publish.pid = child;
    }
    assert!(
        stamp::apply(&f.tmux, &[], &f.target(), &capture, true)
            .unwrap()
            .committed
    );
    f.hook("work", r#"{"session_id":"child-session"}"#, false);
    let row = tma_runtime::cycle::stamp_rows(&f.tmux).unwrap().remove(0);
    assert_eq!(row.agent_session.as_deref(), Some("child-session"));
    assert_eq!(row.model.as_deref(), Some("child-model"));
    assert_eq!(row.transcript.as_deref(), Some("/tmp/child-transcript"));
    assert_eq!(row.context_pct, Some(50));
    assert_eq!(row.quota.as_ref().map(|quota| quota.pct), Some(80));
    let current = f.read();
    assert_eq!(
        current.options.get(opt::API_ENDPOINT).map(String::as_str),
        Some("http://127.0.0.1:4321")
    );
    assert_eq!(store::metadata_owner(&current).unwrap().process.pid, child);
}

#[test]
fn delayed_cycle_housekeeping_returns_the_replacement_that_defeated_it() {
    use std::os::unix::fs::PermissionsExt;
    for registered in [true, false] {
        let Some(mut f) = Fixture::new() else { return };
        f.manifests[0].manifest.identity.process_names = vec!["no-such-process".into()];
        if registered {
            f.hook("start", r#"{"session_id":"old"}"#, false);
        } else {
            f.hook("work", "{}", false);
        }
        f.tmux
            .set_pane_option(&f.target(), opt::REG_DEAD_SINCE, "1")
            .unwrap();
        let wrapper = f.scratch.workdir.join("pause-poll");
        let ready = f.scratch.workdir.join("poll-ready");
        let gate = f.scratch.workdir.join("poll-release");
        std::fs::write(&wrapper, format!(
        "#!/bin/sh\nfor arg do\nif [ \"$arg\" = '@tma_last_poll' ] && [ ! -e '{}' ]; then\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nfi\ndone\nexec tmux \"$@\"\n", ready.display(), ready.display(), gate.display(),
    )).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&Server {
            socket_name: Some(f.scratch.socket.clone()),
            bin: Some(wrapper.to_string_lossy().into_owned()),
            ..Default::default()
        });
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                tma_runtime::cycle::run_cycle_with(
                    &delayed,
                    &f.manifests,
                    &FoldConfig {
                        freshness_secs: 0,
                        ..FoldConfig::default()
                    },
                    tma_runtime::cycle::SeenClear::Deferred,
                )
                .unwrap()
            });
            common::poll_until("cycle has captured its original pane snapshot", || {
                ready.exists()
            });
            // One real replacement hook fits inside the paused tmux command's 3s deadline.
            // A registered episode retains its session; an unregistered one binds it now.
            let session = if registered { "old" } else { "new" };
            f.hook(
                "blocked",
                &format!(r#"{{"session_id":"{session}"}}"#),
                false,
            );
            f.tmux
                .set_pane_option(&f.target(), opt::REG_DEAD_SINCE, "2")
                .unwrap();
            std::fs::write(&gate, "resume").unwrap();
            let report = pending.join().unwrap();
            let row = report
                .rows
                .iter()
                .find(|row| row.pane_id == f.pane)
                .unwrap();
            assert_eq!(row.state, AgentState::Blocked);
            assert_eq!(row.agent_session.as_deref(), Some(session));
            assert_eq!(f.scratch.pane_option(&f.pane, opt::REG_DEAD_SINCE), "2");
            assert!(report
                .deferred_seen
                .iter()
                .any(|observation| observation.identity == row.acknowledgement.clone().unwrap()));
        });
    }
}

#[test]
fn maximum_valid_owners_reconcile_each_window_and_degrade_the_oversized_session() {
    use tma_core::persistence::{PaneOwner, ProcessChain};
    use tma_core::snapshot::{ProcessBirth, ProcessIdentity};
    let Some(f) = Fixture::new() else { return };
    let owner = PaneOwner {
        collector: f.token.clone(),
        episode: tma_core::program_status::EpisodeId::new(1),
        stream_epoch: 1,
        processes: ProcessChain::new(
            (1..=256)
                .map(|index| ProcessIdentity {
                    pid: u32::MAX - 256 + index,
                    birth: ProcessBirth::MacOsStart {
                        seconds: u64::MAX - 1,
                        microseconds: 999_999,
                    },
                })
                .collect(),
        )
        .unwrap(),
    };
    let mut targets = vec![f.target()];
    for _ in 0..3 {
        let added = f.scratch.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{session_id}:.#{pane_id}",
            "-t",
            "owned",
            "exec /bin/sleep 600",
        ]);
        assert!(added.status.success());
        targets.push(String::from_utf8(added.stdout).unwrap().trim().into());
    }
    for (index, target) in targets.iter().enumerate() {
        let publish = Publish {
            pane_id: target.clone(),
            state: AgentState::Blocked,
            detail: None,
            source: Provenance::Hook,
            evidence_at: 1,
            since: 1,
            stamped_at: 1,
            hash: None,
            pid: owner.processes.agent().pid,
            name: "max-owner".into(),
            set_attention: true,
            episode_reset: false,
            guard: Guard::Unconditional,
        };
        assert!(store::publish_program(
            &f.tmux,
            &store::read_pane(&f.tmux, target).unwrap(),
            &owner,
            tma_runtime::now_ms,
            &publish,
            None,
            &[]
        )
        .unwrap()
        .won());
        if index == 0 {
            stamp::reconcile_summaries(&f.tmux).unwrap();
            assert_eq!(f.scratch.get(target, "#{@agent_summary}"), "blocked:1");
            assert_eq!(
                f.scratch.get(target, "#{@agent_session_summary}"),
                "blocked:1"
            );
        }
    }
    f.scratch.tmux(&[
        "set-option",
        "-t",
        "owned",
        opt::SESSION_SUMMARY,
        "bounded-sentinel",
    ]);
    let commands = stamp::summary_commands(&f.tmux).unwrap();
    assert_eq!(
        commands.len(),
        3,
        "each remaining window still converges independently"
    );
    stamp::reconcile_summaries(&f.tmux).unwrap();
    for target in targets {
        assert_eq!(f.scratch.get(&target, "#{@agent_summary}"), "blocked:1");
    }
    assert_eq!(
        f.scratch.get(&f.target(), "#{@agent_session_summary}"),
        "bounded-sentinel"
    );
    eprintln!("maximum valid owner: {} bytes; four such window snapshots exceed the 65536 byte session cap", owner.encode().len());
}

#[test]
fn scheduler_completion_failure_revokes_commit_before_marking_result_presented() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    let decision = f.runtime.decision(f.id()).unwrap();
    let pane = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let target = f.target();
    let mut observed_commit = None;
    assert!(f.read().program.commit.is_none());
    let result = f.runtime.publish_checked_with_completion(
        &f.tmux,
        &mut f.collector,
        &decision,
        &pane,
        &f.manifests,
        &program_processes().unwrap(),
        &f.token,
        f.session,
        tma_runtime::now_ms,
        || {
            let read = store::read_pane(&f.tmux, &target).unwrap();
            if matches!(
                read.projection(tma_runtime::now_ms()),
                Ok(model::ProjectionRead::Live { .. })
            ) {
                observed_commit = read.program.commit;
                false
            } else {
                true
            }
        },
    );
    assert!(matches!(
        result,
        Err(tma_runtime::program_status::publication::ProgramPublishError::ScheduleLost)
    ));
    let commit = observed_commit.expect("actual guarded publication observed before denial");
    assert_eq!(commit.owner.collector, f.token);
    assert_eq!(commit.owner.episode, decision.selection.authority().owner());
    assert!(f.read().program.commit.is_none());
    assert!(
        f.runtime
            .decision(f.id())
            .unwrap()
            .selection
            .result()
            .unwrap()
            .pending
    );
}

#[test]
fn scheduler_prepublication_denial_starts_no_program_write() {
    let Some(mut f) = Fixture::new() else {
        return;
    };
    f.reduce("", ProgramStatus::Done);
    let decision = f.runtime.decision(f.id()).unwrap();
    let pane = ProgramPane {
        pane: f.id(),
        pane_pid: f.pid,
        current_command: "sleep",
        title: "",
        ignored: false,
        registration: None,
    };
    let result = f.runtime.publish_checked_with_completion(
        &f.tmux,
        &mut f.collector,
        &decision,
        &pane,
        &f.manifests,
        &program_processes().unwrap(),
        &f.token,
        f.session,
        tma_runtime::now_ms,
        || false,
    );
    assert!(matches!(
        result,
        Err(tma_runtime::program_status::publication::ProgramPublishError::ScheduleLost)
    ));
    let read = f.read();
    assert!(!read.program.program_origin);
    assert!(read.owner().is_none());
    assert!(read.program.commit.is_none());
    assert_eq!(f.runtime.decision(f.id()), Some(decision));
}
