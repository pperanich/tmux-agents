//! A3/A7/A8: exercise the retained store, not a parallel selection model.
use tma_core::program_status::*;
use tma_core::{AgentState, Detail};

const WORKING: ProgramStatus = ProgramStatus::Working { progress: None };
const BLOCKED: ProgramStatus = ProgramStatus::Blocked {
    kind: None,
    progress: None,
};

struct Fixture {
    store: RecordStore,
    apps: Vec<AppId>,
    owner: EpisodeId,
}
impl Fixture {
    fn new() -> Self {
        Self {
            store: RecordStore::new(),
            apps: vec![AppId::parse("pi").unwrap()],
            owner: EpisodeId::new(1),
        }
    }
    fn report(&mut self, id: &str, status: ProgramStatus, app: Option<&str>) {
        let mut record = RecordRef::new(
            if id.is_empty() {
                RecordIdRef::ROOT
            } else {
                RecordIdRef::parse(id).unwrap()
            },
            status,
        );
        record.app = app.map(|app| AppIdRef::parse(app).unwrap());
        self.store.apply(
            ProgramCommandRef::Replace(record),
            42,
            Some(LiveEpisode {
                owner: self.owner,
                apps: &self.apps,
            }),
        );
    }
    fn clear(&mut self, id: &str) {
        self.store.apply(
            ProgramCommandRef::Clear(if id.is_empty() {
                RecordIdRef::ROOT
            } else {
                RecordIdRef::parse(id).unwrap()
            }),
            43,
            Some(LiveEpisode {
                owner: self.owner,
                apps: &self.apps,
            }),
        );
    }
    fn selection(&self) -> Option<ProgramSelection> {
        self.store.select(LiveEpisode {
            owner: self.owner,
            apps: &self.apps,
        })
    }
    fn selected(&self) -> ProgramSelection {
        self.selection().unwrap()
    }
    fn mark_presented(&mut self, selected: &ProgramSelection) -> bool {
        self.store.mark_presented(
            selected,
            LiveEpisode {
                owner: self.owner,
                apps: &self.apps,
            },
        )
    }
    fn present(&mut self) -> ResultIdentity {
        let selected = self.selected();
        let identity = selected.result().unwrap().identity.clone();
        assert!(self.mark_presented(&selected));
        identity
    }
}

#[test]
fn parentless_siblings_nearest_app_and_segment_safe_clear() {
    let mut f = Fixture::new();
    f.report("build/test", WORKING, Some("pi"));
    f.report("build/test/child", BLOCKED, None);
    f.report("build/testing", ProgramStatus::Error, Some("pi"));
    f.report("other", ProgramStatus::Idle, None);
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("build/test/child").unwrap()
    );
    f.clear("build/test");
    assert_eq!(f.store.records().len(), 2);
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("build/testing").unwrap()
    );
    assert_eq!(
        f.store
            .effective_app(RecordIdRef::parse("build/test/no-parent").unwrap()),
        None
    );
}

#[test]
fn full_replacement_drops_optional_fields_without_copying_inherited_apps() {
    let mut f = Fixture::new();
    let mut root = RecordRef::new(RecordIdRef::ROOT, WORKING);
    root.app = Some(AppIdRef::parse("pi").unwrap());
    root.msg = Some(SingleLineRef::parse("approve dangerous_command").unwrap());
    let root = root
        .with_title(Some(SingleLineRef::parse("title").unwrap()))
        .unwrap();
    f.store.apply(
        ProgramCommandRef::Replace(root),
        1,
        Some(LiveEpisode {
            owner: f.owner,
            apps: &f.apps,
        }),
    );
    f.report("child", ProgramStatus::Idle, None);
    assert!(f.store.records()[1].record().app.is_none());
    f.report("", ProgramStatus::Idle, None);
    let record = f.store.records().last().unwrap().record();
    assert!(record.app.is_none() && record.title().is_none() && record.msg.is_none());
    assert_eq!(f.selected().state(), AgentState::Idle);
    f.report("another-child", BLOCKED, None);
    assert_eq!(
        f.selected().state(),
        AgentState::Blocked,
        "established anonymous ancestor admits its lineage"
    );
}

#[test]
fn foreign_root_revokes_anonymous_lineage_until_fresh_matching_evidence() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("inherited", BLOCKED, None);
    f.report("independent", ProgramStatus::Idle, Some("pi"));
    f.report("", ProgramStatus::Idle, Some("foreign"));
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("independent").unwrap()
    );
    assert_eq!(
        f.store
            .effective_app(RecordIdRef::parse("inherited").unwrap())
            .unwrap()
            .as_str(),
        "foreign"
    );
    f.report("", WORKING, None);
    f.report("inherited", BLOCKED, None);
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("independent").unwrap()
    );
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("inherited").unwrap()
    );
}

#[test]
fn anonymous_cannot_start_or_cross_an_episode() {
    let mut f = Fixture::new();
    f.report("", WORKING, None);
    assert!(f.selection().is_none());
    f.report("", WORKING, Some("pi"));
    f.store.invalidate_episode();
    assert!(f.selection().is_none());
    f.owner = EpisodeId::new(2);
    f.report("child", BLOCKED, None);
    assert!(f.selection().is_none());
    f.report("child", BLOCKED, Some("pi"));
    assert_eq!(f.selected().authority().owner(), f.owner);
}

#[test]
fn ancestor_replacement_clear_and_eviction_recompute_effective_app() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Idle, Some("pi"));
    f.report("group", ProgramStatus::Idle, Some("foreign"));
    f.report("group/child", BLOCKED, None);
    assert_eq!(f.selected().state(), AgentState::Idle);
    f.report("group", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Blocked);
    f.report("group/child", BLOCKED, None);
    assert_eq!(f.selected().state(), AgentState::Blocked);
    // Evict the old root, then group; the child keeps its established lineage,
    // while effective inheritance reflects the records that actually remain.
    for index in 0..254 {
        f.report(&format!("s{index}"), ProgramStatus::Idle, Some("pi"));
    }
    assert!(f
        .store
        .records()
        .iter()
        .all(|entry| !entry.record().id.as_ref().is_root()));
    assert_eq!(f.store.records().len(), MAX_RECORDS);
    f.report("extra", ProgramStatus::Idle, Some("pi"));
    assert!(f
        .store
        .effective_app(RecordIdRef::parse("group/child").unwrap())
        .is_none());
    assert_eq!(f.selected().state(), AgentState::Blocked);
    f.clear("group");
    assert_eq!(f.selected().state(), AgentState::Idle);
}

#[test]
fn rejected_frame_leaves_records_bytes_and_eviction_order_untouched() {
    let mut f = Fixture::new();
    for index in 0..MAX_RECORDS {
        f.report(&format!("r{index}"), ProgramStatus::Idle, Some("pi"));
    }
    let before: Vec<_> = f
        .store
        .records()
        .iter()
        .map(|entry| (entry.record().clone(), entry.updated()))
        .collect();
    let bytes = f.store.retained_bytes();
    let mut scratch = DecodeScratch::default();
    let outcome = parse_frame(b"\x1b]7501;state=working:id=r0:msg=Zg=\x1b\\", &mut scratch);
    assert!(outcome.is_err());
    if let Ok(ParseOutcome::Command(command)) = outcome {
        f.store.apply(command, 50, None);
    }
    assert_eq!(
        before,
        f.store
            .records()
            .iter()
            .map(|entry| (entry.record().clone(), entry.updated()))
            .collect::<Vec<_>>()
    );
    assert_eq!(bytes, f.store.retained_bytes());
    f.report("new", ProgramStatus::Idle, Some("pi"));
    assert!(f
        .store
        .records()
        .iter()
        .all(|entry| entry.record().id.as_ref().as_str() != "r0"));
}

#[test]
fn updates_use_ordinals_even_when_clock_moves_backwards() {
    let mut f = Fixture::new();
    f.report("first", BLOCKED, Some("pi"));
    f.report("second", BLOCKED, Some("pi"));
    assert_eq!(
        f.selected().record_id(),
        &RecordId::parse("second").unwrap()
    );
    let mut record = RecordRef::new(RecordIdRef::parse("first").unwrap(), BLOCKED);
    record.app = Some(AppIdRef::parse("pi").unwrap());
    f.store.apply(
        ProgramCommandRef::Replace(record),
        0,
        Some(LiveEpisode {
            owner: f.owner,
            apps: &f.apps,
        }),
    );
    assert_eq!(f.selected().record_id(), &RecordId::parse("first").unwrap());
}

#[test]
fn lifecycle_retains_results_only_as_history_until_new_reports() {
    for event in [
        TerminalLifecycle::ShellPrompt,
        TerminalLifecycle::AttachedProcessExit,
    ] {
        let mut f = Fixture::new();
        for (id, state) in [
            ("working", WORKING),
            ("blocked", BLOCKED),
            ("idle", ProgramStatus::Idle),
            ("done", ProgramStatus::Done),
            ("error", ProgramStatus::Error),
        ] {
            f.report(id, state, Some("pi"));
        }
        f.store.lifecycle(event);
        assert_eq!(f.store.records().len(), 2);
        assert!(f.store.records().iter().all(StoredRecord::is_historical));
        assert!(f.selection().is_none());
        f.report("done", ProgramStatus::Done, None);
        assert!(
            f.selection().is_none(),
            "history is not an ownership anchor"
        );
        f.store.lifecycle(TerminalLifecycle::FullReset);
        assert!(f.store.records().is_empty());
    }
}

#[test]
fn status_projection_preserves_protocol_and_has_no_action_payload() {
    for (status, state, detail) in [
        (ProgramStatus::Idle, AgentState::Idle, None),
        (WORKING, AgentState::Working, None),
        (BLOCKED, AgentState::Blocked, None),
        (
            ProgramStatus::Blocked {
                kind: Some(BlockedKind::Permission),
                progress: None,
            },
            AgentState::Blocked,
            Some("permission"),
        ),
        (
            ProgramStatus::Blocked {
                kind: Some(BlockedKind::Question),
                progress: None,
            },
            AgentState::Blocked,
            Some("question"),
        ),
        (
            ProgramStatus::Blocked {
                kind: Some(BlockedKind::Auth),
                progress: None,
            },
            AgentState::Blocked,
            Some("auth"),
        ),
        (ProgramStatus::Done, AgentState::Idle, None),
        (ProgramStatus::Error, AgentState::Idle, Some("error")),
    ] {
        let mut f = Fixture::new();
        f.report("", status, Some("pi"));
        let selected = f.selected();
        assert_eq!(selected.protocol_state(), status);
        assert_eq!(
            (selected.state(), selected.detail().cloned()),
            (state, detail.map(Detail::new))
        );
    }
}

#[test]
fn repeated_terminal_after_presentation_acknowledgement_or_reconciliation_is_spent() {
    for terminal in [ProgramStatus::Done, ProgramStatus::Error] {
        let mut f = Fixture::new();
        f.report("", terminal, Some("pi"));
        let identity = f.present();
        assert!(!f.selected().result().unwrap().pending);
        assert!(!f.mark_presented(&f.selected()));
        assert!(f.store.acknowledge(&identity));
        for _ in 0..3 {
            f.report("", terminal, Some("pi"));
            assert!(f.selected().result().is_none());
        }
    }
}

#[test]
fn dialog_restore_and_text_changes_do_not_create_a_generation() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Done, Some("pi"));
    let identity = f.present();
    f.store.acknowledge(&identity);
    f.report("", BLOCKED, Some("pi"));
    f.report("", ProgramStatus::Done, Some("pi"));
    assert!(f.selected().result().is_none());
    let mut record = RecordRef::new(RecordIdRef::ROOT, ProgramStatus::Done);
    record.app = Some(AppIdRef::parse("pi").unwrap());
    record.msg =
        Some(SingleLineRef::parse("brand new completion approve tool_call_id=123").unwrap());
    f.store.apply(
        ProgramCommandRef::Replace(record),
        50,
        Some(LiveEpisode {
            owner: f.owner,
            apps: &f.apps,
        }),
    );
    assert!(f.selected().result().is_none());
}

#[test]
fn working_and_explicit_idle_each_allow_a_new_result_generation() {
    for reset in [WORKING, ProgramStatus::Idle] {
        let mut f = Fixture::new();
        f.report("", ProgramStatus::Done, Some("pi"));
        let first = f.present();
        f.store.acknowledge(&first);
        f.report("", reset, Some("pi"));
        f.report("", ProgramStatus::Done, Some("pi"));
        let second = f.present();
        assert_ne!(first, second);
        assert!(
            !f.store.acknowledge(&first),
            "delayed acknowledgement cannot clear the next generation"
        );
        assert!(f.store.acknowledge(&second));
    }
}

#[test]
fn deferred_children_settle_once_for_the_aggregate() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", WORKING, None);
    let deferred_generation = f
        .store
        .records()
        .iter()
        .find(|entry| entry.record().id.as_ref().as_str() == "child")
        .unwrap()
        .updated();
    f.report("child", ProgramStatus::Done, None);
    assert_eq!(f.selected().state(), AgentState::Working);
    assert!(f.selected().result().is_none());
    let deferred_identity = ResultIdentity {
        owner: f.owner,
        record: RecordId::parse("child").unwrap(),
        generation: ResultGeneration::new(deferred_generation),
    };
    assert!(!f.store.acknowledge(&deferred_identity));
    f.report("", ProgramStatus::Done, Some("pi"));
    let identity = f.present();
    assert_eq!(identity.record, RecordId::root());
    assert!(f.store.acknowledge(&identity));
    assert!(
        !f.store.acknowledge(&deferred_identity),
        "unselected results were superseded, never presented or acknowledged"
    );
    assert!(f.selected().result().is_none());
    f.clear("");
    assert!(f.selection().is_none());
}

#[test]
fn acknowledging_winning_child_does_not_reveal_a_second_completion() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", WORKING, None);
    f.report("child", ProgramStatus::Error, None);
    f.report("", ProgramStatus::Done, Some("pi"));
    let selected = f.selected();
    assert_eq!(selected.result().unwrap().kind, ResultKind::Failure);
    let identity = f.present();
    f.store.acknowledge(&identity);
    assert!(f.selected().result().is_none());
    assert_eq!(f.selected().state(), AgentState::Idle);
    f.report("", ProgramStatus::Done, Some("pi"));
    assert!(f.selected().result().is_none());
}

#[test]
fn new_aggregate_cycle_supersedes_old_child_error_without_deleting_history() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", ProgramStatus::Error, None);
    f.report("", ProgramStatus::Done, Some("pi"));
    let failed = f.present();
    f.report("", WORKING, Some("pi"));
    assert!(!f.mark_presented(&ProgramSelection::clone(&f.selected())));
    f.report("", ProgramStatus::Done, Some("pi"));
    let selected = f.selected();
    assert_eq!(selected.record_id(), &RecordId::root());
    assert_eq!(selected.result().unwrap().kind, ResultKind::Completion);
    assert!(f
        .store
        .records()
        .iter()
        .any(|entry| entry.record().status == ProgramStatus::Error));
    assert_ne!(f.present(), failed);
}

#[test]
fn stale_selection_cannot_present_after_new_work_or_owner_loss() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Done, Some("pi"));
    let stale = f.selected();
    f.report("", WORKING, Some("pi"));
    assert!(!f.mark_presented(&stale));
    f.store.invalidate_episode();
    assert!(!f.mark_presented(&stale));
}

#[test]
fn unpresented_result_survives_a_dialog_in_the_same_processing_batch() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Done, Some("pi"));
    let initial = f.selected().result().unwrap().identity.clone();
    f.report("", BLOCKED, Some("pi"));
    assert!(f.selected().result().is_none());
    f.report("", ProgramStatus::Done, Some("pi"));
    assert_eq!(f.selected().result().unwrap().identity, initial);
    assert!(f.selected().result().unwrap().pending);
    assert_eq!(f.present(), initial);
}

#[test]
fn foreign_idle_reset_cannot_supersede_or_advance_the_eligible_aggregate() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Done, Some("pi"));
    let initial = f.selected().result().unwrap().clone();
    f.report("cargo", ProgramStatus::Done, Some("cargo"));
    f.report("cargo", ProgramStatus::Idle, Some("cargo"));
    assert_eq!(f.selected().result().unwrap(), &initial);
    assert_eq!(f.present(), initial.identity);
}

#[test]
fn idle_reset_starts_a_new_cycle_and_supersedes_old_child_error() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", ProgramStatus::Error, None);
    f.report("", ProgramStatus::Done, Some("pi"));
    f.present();
    f.report("", ProgramStatus::Idle, Some("pi"));
    f.report("", ProgramStatus::Done, Some("pi"));
    assert_eq!(f.selected().record_id(), &RecordId::root());
    assert_eq!(f.selected().result().unwrap().kind, ResultKind::Completion);
}

#[test]
fn recovery_reconciles_exact_settled_results_without_manufacturing_attention() {
    for disposition in [
        ResultDisposition::Presented,
        ResultDisposition::Acknowledged,
    ] {
        let mut f = Fixture::new();
        f.report("", ProgramStatus::Done, Some("pi"));
        let old = f.present();
        f.store.acknowledge(&old);
        f.store.invalidate_episode();
        assert!(f.store.records().iter().all(StoredRecord::is_historical));
        assert!(f.selection().is_none());
        f.owner = EpisodeId::new(2);
        f.report("", ProgramStatus::Done, Some("pi"));
        let selected = f.selected();
        let live = LiveEpisode {
            owner: f.owner,
            apps: &f.apps,
        };
        assert!(f.store.reconcile_result(&selected, live, disposition));
        let result = f.selected().result().cloned();
        match disposition {
            ResultDisposition::Presented => assert!(!result.unwrap().pending),
            ResultDisposition::Acknowledged => assert!(result.is_none()),
        }
        assert!(!f.store.acknowledge(&old));
    }
}

#[test]
fn presentation_and_reconciliation_cannot_consume_a_deferred_child() {
    let mut f = Fixture::new();
    f.report("child", ProgramStatus::Done, Some("pi"));
    let uncommitted = f.selected();
    f.report("", WORKING, Some("pi"));
    let live = LiveEpisode {
        owner: f.owner,
        apps: &f.apps,
    };
    assert!(!f
        .store
        .reconcile_result(&uncommitted, live, ResultDisposition::Acknowledged));
    assert!(!f.store.acknowledge(&uncommitted.result().unwrap().identity));
}

#[test]
fn replacement_at_capacity_refreshes_only_the_replaced_record_and_query_is_inert() {
    let mut f = Fixture::new();
    for index in 0..MAX_RECORDS {
        f.report(&format!("r{index}"), ProgramStatus::Idle, Some("pi"));
    }
    f.report("r0", ProgramStatus::Idle, Some("pi"));
    let ordinal = f.selected().updated();
    f.store.apply(ProgramCommandRef::Query, 0, None);
    assert_eq!(f.selected().updated(), ordinal);
    assert_eq!(f.store.records().len(), MAX_RECORDS);
    f.report("new", ProgramStatus::Idle, Some("pi"));
    assert!(f
        .store
        .records()
        .iter()
        .any(|entry| entry.record().id.as_ref().as_str() == "r0"));
    assert!(!f
        .store
        .records()
        .iter()
        .any(|entry| entry.record().id.as_ref().as_str() == "r1"));
    f.clear("");
    assert_eq!(f.store.retained_bytes(), 0);
}

#[test]
fn selection_return_to_a_spent_terminal_record_is_not_another_completion() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Done, Some("pi"));
    let first = f.present();
    assert!(f.store.acknowledge(&first));
    f.report("child", WORKING, None);
    assert_eq!(f.selected().state(), AgentState::Working);
    f.clear("child");
    assert_eq!(f.selected().protocol_state(), ProgramStatus::Done);
    assert!(f.selected().result().is_none());
}

#[test]
fn observed_work_during_a_result_dialog_starts_a_new_aggregate_cycle() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Error, Some("pi"));
    f.report("", BLOCKED, Some("pi"));
    f.report("", BLOCKED, Some("pi"));
    f.report("foreign", ProgramStatus::Idle, Some("cargo"));
    f.report("child", WORKING, None);
    f.report("child", ProgramStatus::Done, None);
    f.report("", ProgramStatus::Error, Some("pi"));
    let result = f.selected();
    assert_eq!(result.record_id(), &RecordId::parse("child").unwrap());
    assert_eq!(result.result().unwrap().kind, ResultKind::Completion);
}

#[test]
fn matching_ancestor_adopts_retained_anonymous_output_from_the_validated_episode() {
    let mut f = Fixture::new();
    f.report("child", BLOCKED, None);
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Blocked);
    f.report("child", BLOCKED, None);
    assert_eq!(f.selected().state(), AgentState::Blocked);
}

#[test]
fn fresh_result_that_restores_an_active_lineage_is_deferred_not_superseded() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", WORKING, None);
    f.report("", ProgramStatus::Idle, Some("foreign"));
    assert!(f.selection().is_none());
    f.report("", ProgramStatus::Done, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Working);
    f.report("child", ProgramStatus::Idle, None);
    assert_eq!(f.selected().protocol_state(), ProgramStatus::Done);
    assert!(f.selected().result().unwrap().pending);
    f.present();
}

#[test]
fn retained_anonymous_output_before_binding_cannot_join_a_later_episode() {
    let mut f = Fixture::new();
    let child = RecordRef::new(RecordIdRef::parse("child").unwrap(), BLOCKED);
    f.store.apply(ProgramCommandRef::Replace(child), 1, None);
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Idle);
    f.store.invalidate_episode();
    f.owner = EpisodeId::new(2);
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Idle);
}

#[test]
fn first_matching_terminal_defers_until_retained_same_episode_child_work_settles() {
    let mut f = Fixture::new();
    f.report("child", WORKING, None);
    assert!(f.selection().is_none());
    f.report("", ProgramStatus::Done, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Working);
    f.report("child", ProgramStatus::Idle, None);
    assert_eq!(f.selected().protocol_state(), ProgramStatus::Done);
    assert!(f.selected().result().unwrap().pending);
    f.present();
}

#[test]
fn adopting_retained_children_preserves_their_unpresented_results() {
    let mut f = Fixture::new();
    f.report("finished", ProgramStatus::Done, None);
    f.report("running", WORKING, None);
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().state(), AgentState::Working);
    f.report("running", ProgramStatus::Idle, None);
    let selected = f.selected();
    assert_eq!(selected.record_id(), &RecordId::parse("finished").unwrap());
    assert!(selected.result().unwrap().pending);
    f.present();
}

#[test]
fn presenting_an_aggregate_retires_temporarily_revoked_results_from_that_cycle() {
    let mut f = Fixture::new();
    f.report("", WORKING, Some("pi"));
    f.report("child", ProgramStatus::Error, None);
    f.report("", ProgramStatus::Idle, Some("foreign"));
    assert!(f.selection().is_none());
    f.report("independent", ProgramStatus::Done, Some("pi"));
    let presented = f.present();
    assert!(f.store.acknowledge(&presented));
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert!(f.selected().result().is_none());
    assert_eq!(f.selected().detail(), None);
}

#[test]
fn a_foreign_reset_of_the_same_record_cannot_rearm_an_acknowledged_result() {
    for foreign in [ProgramStatus::Idle, WORKING] {
        let mut f = Fixture::new();
        f.report("", ProgramStatus::Done, Some("pi"));
        let presented = f.present();
        assert!(f.store.acknowledge(&presented));
        f.report("", foreign, Some("foreign"));
        assert!(f.selection().is_none());
        f.report("", ProgramStatus::Done, Some("pi"));
        assert!(f.selected().result().is_none());
        f.report("", ProgramStatus::Idle, Some("pi"));
        f.report("", ProgramStatus::Done, Some("pi"));
        assert_ne!(f.present(), presented);
    }
}

#[test]
fn stale_presentation_after_lineage_revocation_leaves_the_result_pending() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Idle, Some("pi"));
    f.report("child", ProgramStatus::Done, None);
    let selected = f.selected();
    let identity = selected.result().unwrap().identity.clone();
    f.report("", ProgramStatus::Idle, Some("foreign"));
    assert!(!f.mark_presented(&selected));
    assert!(!f.store.acknowledge(&identity));
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert_eq!(f.selected().result().unwrap().identity, identity);
    assert!(f.selected().result().unwrap().pending);
}

#[test]
fn invalidation_prevents_old_result_ownership_from_rejoining_a_new_episode() {
    let mut f = Fixture::new();
    f.report("", ProgramStatus::Idle, Some("pi"));
    f.report("child", ProgramStatus::Error, None);
    let old = f.selected();
    f.store.invalidate_episode();
    f.owner = EpisodeId::new(2);
    f.report("", ProgramStatus::Idle, Some("pi"));
    assert!(f.selected().result().is_none());
    assert!(!f.mark_presented(&old));
    assert!(!f.store.acknowledge(&old.result().unwrap().identity));
    assert!(f
        .store
        .records()
        .iter()
        .any(|entry| { entry.is_historical() && entry.record().status == ProgramStatus::Error }));
}
