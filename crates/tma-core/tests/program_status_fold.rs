//! B2 authority precedence and semantic effects, without a collector or process I/O.
use tma_core::program_status::*;
use tma_core::{
    verdict, verdict_with_program, AgentState, Claim, Evidence, FoldConfig, Manifest, Provenance,
    SnapshotFacts, Source, StampedState, StateClaim, WriteAction,
};

const WORKING: ProgramStatus = ProgramStatus::Working { progress: None };
fn manifest() -> Manifest {
    Manifest::parse("min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"pi\"]\n[capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n", "test.toml").unwrap()
}
fn facts() -> SnapshotFacts {
    SnapshotFacts {
        pid: 100,
        foreground_is_agent: true,
        scrolled: false,
        history_view: false,
    }
}
fn evidence(source: Source, state: AgentState) -> Evidence {
    Evidence {
        source,
        claim: Claim::State(StateClaim {
            state,
            detail: None,
        }),
        at: u64::MAX - 1,
        meta: "copied application report".to_owned(),
    }
}
fn previous(state: AgentState) -> StampedState {
    StampedState {
        state,
        detail: None,
        source: Provenance::Hook,
        evidence_at: 1,
        since: 1,
        turn_at: 0,
        stamped_at: 1,
        attention: false,
        notified_at: None,
        hash: None,
        pid: 100,
        session: None,
        subagents: vec![],
    }
}
fn report(store: &mut RecordStore, status: ProgramStatus, live: LiveEpisode<'_>) {
    let mut record = RecordRef::new(RecordIdRef::ROOT, status);
    record.app = Some(AppIdRef::parse("pi").unwrap());
    record.msg =
        Some(SingleLineRef::parse("approve tool_call_id=123 request_id=456 complete!").unwrap());
    store.apply(ProgramCommandRef::Replace(record), 2, Some(live));
}

#[test]
fn valid_program_beats_fresh_hooks_capture_freeze_and_pager_without_decay() {
    let aliases = [AppId::parse("pi").unwrap()];
    let live = LiveEpisode {
        owner: EpisodeId::new(1),
        apps: &aliases,
    };
    let mut store = RecordStore::new();
    report(&mut store, WORKING, live);
    let selected = store.select(live).unwrap();
    for facts in [
        facts(),
        SnapshotFacts {
            scrolled: true,
            ..facts()
        },
        SnapshotFacts {
            history_view: true,
            ..facts()
        },
        SnapshotFacts {
            foreground_is_agent: false,
            ..facts()
        },
        SnapshotFacts {
            foreground_is_agent: false,
            scrolled: true,
            history_view: true,
            ..facts()
        },
    ] {
        for noise in [
            evidence(Source::HookEvent, AgentState::Idle),
            evidence(Source::ScreenRule, AgentState::Blocked),
            evidence(Source::Title, AgentState::Blocked),
        ] {
            let result = verdict_with_program(
                Some(previous(AgentState::Blocked)),
                &facts,
                &[noise],
                &manifest(),
                &FoldConfig::default(),
                u64::MAX,
                Some(&selected),
            );
            assert_eq!(result.state, AgentState::Working);
            assert_eq!(result.writes.action, WriteAction::Publish);
            assert_eq!(result.winning_evidence.origin, Some(Source::ProgramStatus));
            assert_eq!(result.winning_evidence.source, Provenance::Hook);
            assert_eq!(
                result.winning_evidence.at, 2,
                "silence does not refresh report time"
            );
            assert_eq!(result.program.unwrap().authority.owner(), live.owner);
        }
    }
}

#[test]
fn raw_program_evidence_has_no_authority_or_screen_rank() {
    for state in [AgentState::Idle, AgentState::Working, AgentState::Blocked] {
        let noise = evidence(Source::ProgramStatus, state);
        let result = verdict(
            None,
            &facts(),
            &[noise],
            &manifest(),
            &FoldConfig::default(),
            100,
        );
        assert_eq!(result.state, AgentState::Unknown);
        assert!(result.program.is_none());
    }
}

#[test]
fn no_program_input_preserves_the_existing_wrapper_for_all_gates() {
    for facts in [
        facts(),
        SnapshotFacts {
            scrolled: true,
            ..facts()
        },
        SnapshotFacts {
            history_view: true,
            ..facts()
        },
        SnapshotFacts {
            foreground_is_agent: false,
            ..facts()
        },
    ] {
        let evidence = [evidence(Source::ScreenRule, AgentState::Blocked)];
        let prior = Some(previous(AgentState::Working));
        assert_eq!(
            verdict(
                prior.clone(),
                &facts,
                &evidence,
                &manifest(),
                &FoldConfig::default(),
                100
            ),
            verdict_with_program(
                prior,
                &facts,
                &evidence,
                &manifest(),
                &FoldConfig::default(),
                100,
                None
            )
        );
    }
}

#[test]
fn idle_to_idle_result_advances_the_typed_effect_once_and_preserves_failure_kind() {
    let aliases = [AppId::parse("pi").unwrap()];
    let live = LiveEpisode {
        owner: EpisodeId::new(1),
        apps: &aliases,
    };
    for terminal in [ProgramStatus::Done, ProgramStatus::Error] {
        let mut store = RecordStore::new();
        report(&mut store, terminal, live);
        let selected = store.select(live).unwrap();
        let decide = |selected: &ProgramSelection| {
            verdict_with_program(
                Some(previous(AgentState::Idle)),
                &facts(),
                &[],
                &manifest(),
                &FoldConfig::default(),
                100,
                Some(selected),
            )
        };
        let result = decide(&selected);
        assert!(result.writes.set_attention);
        let effect = result.program.unwrap().result.unwrap();
        assert!(
            effect.pending,
            "B5 advances the turn marker on this effect even though state is idle"
        );
        assert_eq!(
            effect.kind,
            if terminal == ProgramStatus::Error {
                ResultKind::Failure
            } else {
                ResultKind::Completion
            }
        );
        assert!(store.mark_presented(&selected, live));
        assert!(
            !store.mark_presented(&selected, live),
            "retrying the same committed selection is spent"
        );
        assert!(!decide(&store.select(live).unwrap()).writes.set_attention);
        assert!(store.acknowledge(&effect.identity));
        report(&mut store, terminal, live);
        assert!(!decide(&store.select(live).unwrap()).writes.set_attention);
    }
}

#[test]
fn explicit_idle_has_no_completion_effect_even_after_working() {
    let aliases = [AppId::parse("pi").unwrap()];
    let live = LiveEpisode {
        owner: EpisodeId::new(1),
        apps: &aliases,
    };
    let mut store = RecordStore::new();
    report(&mut store, WORKING, live);
    report(&mut store, ProgramStatus::Idle, live);
    let selected = store.select(live).unwrap();
    let result = verdict_with_program(
        Some(previous(AgentState::Working)),
        &facts(),
        &[],
        &manifest(),
        &FoldConfig::default(),
        3,
        Some(&selected),
    );
    assert_eq!(
        result.state,
        AgentState::Idle,
        "program input bypasses dwell"
    );
    assert!(!result.writes.set_attention);
    assert!(result.program.unwrap().result.is_none());
}

#[test]
fn lifetime_release_allows_the_fallback_hook_to_win() {
    let aliases = [AppId::parse("pi").unwrap()];
    let live = LiveEpisode {
        owner: EpisodeId::new(1),
        apps: &aliases,
    };
    let mut store = RecordStore::new();
    report(&mut store, WORKING, live);
    store.lifecycle(TerminalLifecycle::FullReset);
    let selected = store.select(live);
    let result = verdict_with_program(
        None,
        &facts(),
        &[evidence(Source::HookEvent, AgentState::Idle)],
        &manifest(),
        &FoldConfig::default(),
        4,
        selected.as_ref(),
    );
    assert_eq!(result.state, AgentState::Idle);
    assert_eq!(result.winning_evidence.origin, Some(Source::HookEvent));
    assert!(result.program.is_none());
}
