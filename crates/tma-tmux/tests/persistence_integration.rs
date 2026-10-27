//! Real server acceptance for the private persistence primitives. Every server is explicitly
//! named and owned by this test, without inspecting or cleaning unrelated servers.
use std::sync::atomic::{AtomicU64, Ordering};

use tma_core::persistence::{
    opt, CollectorLease, MetadataOwner, PaneOwner, PersistedResult, ProcessChain, ProjectionRead,
    Token,
};
use tma_core::program_status::{
    EpisodeId, RecordId, ResultGeneration, ResultIdentity, ResultKind, WorkCycle,
};
use tma_core::render::{Guard, Publish};
use tma_core::snapshot::{ProcessBirth, ProcessIdentity};
use tma_core::stamp::opt as public;
use tma_core::{AgentState, Detail, Provenance};
use tma_tmux::persistence::{
    self as store, CommitOutcome, LeaseOutcome, OptionEdit, PersistenceError,
};
use tma_tmux::tmux::Tmux;

const NOW: u64 = 1_800_000_000_000;
static NEXT: AtomicU64 = AtomicU64::new(1);
// Race cases create their own concurrent clients. Keep unrelated servers from competing for
// process slots against the adapter's three-second wall cap on loaded native test hosts.
static NATIVE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Scratch {
    _serial: std::sync::MutexGuard<'static, ()>,
    socket: String,
    tmux: Tmux,
    pane: String,
    workdir: std::path::PathBuf,
}
impl Scratch {
    fn new() -> Option<Self> {
        if !tma_test_support::tmux_available() {
            return None;
        }
        let serial = NATIVE_TEST
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let socket = format!("tma_test_b5a_{}", tma_test_support::unique_id());
        let out = tma_test_support::scratch_tmux(
            &socket,
            &[
                "new-session",
                "-d",
                "-s",
                "first",
                "-P",
                "-F",
                "#{pane_id}",
                "sleep 120",
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pane = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let workdir =
            std::env::temp_dir().join(format!("tma_persistence_{}", tma_test_support::unique_id()));
        std::fs::create_dir_all(&workdir).unwrap();
        Some(Self {
            _serial: serial,
            tmux: Tmux::new(Some(socket.clone())),
            socket,
            pane,
            workdir,
        })
    }
    fn run(&self, args: &[&str]) {
        let out = tma_test_support::scratch_tmux(&self.socket, args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn read(&self) -> store::PaneRead {
        store::read_pane(&self.tmux, &self.pane).unwrap()
    }
    fn lease(&self, token: &Token) -> CollectorLease {
        match store::acquire_collector(&self.tmux, &self.pane, token, || NOW).unwrap() {
            LeaseOutcome::Won(Some(lease)) => lease,
            other => panic!("{other:?}"),
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        tma_test_support::kill_scratch_server(&self.socket);
        tma_test_support::cleanup_scratch_socket(&self.socket);
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}
fn token(n: u64) -> Token {
    Token::parse(&format!("{n:032x}")).unwrap()
}
fn owner(n: u64) -> PaneOwner {
    PaneOwner {
        collector: token(1),
        episode: EpisodeId::new(n),
        stream_epoch: n,
        processes: ProcessChain::new(vec![
            ProcessIdentity {
                pid: 10,
                birth: ProcessBirth::LinuxStartTicks(99),
            },
            ProcessIdentity {
                pid: 11,
                birth: ProcessBirth::LinuxStartTicks(100 + n),
            },
        ])
        .unwrap(),
    }
}
fn publish(s: &Scratch, state: AgentState) -> Publish {
    Publish {
        pane_id: s.pane.clone(),
        state,
        detail: None,
        source: Provenance::Capture,
        evidence_at: NOW,
        since: NOW,
        stamped_at: NOW,
        hash: Some(12),
        pid: 11,
        name: "pi".into(),
        set_attention: true,
        episode_reset: false,
        guard: Guard::Unconditional,
    }
}
fn result(owner: &PaneOwner, generation: u64, kind: ResultKind) -> PersistedResult {
    PersistedResult {
        owner: owner.clone(),
        identity: ResultIdentity {
            owner: owner.episode,
            record: RecordId::root(),
            generation: ResultGeneration::new(generation),
        },
        cycle: WorkCycle::new(generation),
        kind,
    }
}
fn commit(
    s: &Scratch,
    owner: &PaneOwner,
    p: &Publish,
    r: Option<&PersistedResult>,
) -> store::PaneRead {
    let out = store::publish_program(
        &s.tmux,
        &s.read(),
        owner,
        || NOW,
        p,
        r,
        &[OptionEdit::set(public::TURN_AT, NOW.to_string())],
    )
    .unwrap();
    assert!(out.won(), "{out:?}");
    out.winner().clone()
}

#[test]
fn capability_proves_true_false_unset_quoting_and_partial_failure() {
    let Some(s) = Scratch::new() else {
        return;
    };
    assert!(store::probe_capability(&s.tmux, &s.pane).unwrap());
    store::require_capability(&s.tmux, &s.pane).unwrap();
}

#[test]
fn live_owner_suppresses_entire_fallback_tuple_even_unconditional_and_reset_cleanup() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    s.lease(&owner.collector);
    let before = commit(&s, &owner, &publish(&s, AgentState::Blocked), None);
    for guard in [
        Guard::Unconditional,
        Guard::ProtectHook,
        Guard::CarveOut {
            capture_at: NOW + 1,
        },
        Guard::RefreshClaim {
            state: AgentState::Blocked,
        },
        Guard::HookArbitrate {
            evidence_at: NOW + 1,
        },
    ] {
        let edits = [
            OptionEdit::set(public::STATE, "idle"),
            OptionEdit::unset(public::ATTENTION),
            OptionEdit::unset(public::PERMISSION_REQUEST),
            OptionEdit::set(public::NAME, "other"),
            OptionEdit::set(public::PID, "99"),
            OptionEdit::set(public::HASH, "99"),
            OptionEdit::set(public::STAMPED_AT, (NOW + 9).to_string()),
        ];
        let out = store::write_fallback(&s.tmux, &s.read(), guard, || NOW, &edits).unwrap();
        assert!(!out.won());
        assert_eq!(out.winner(), &before);
    }
    assert!(matches!(
        before.projection(NOW).unwrap(),
        ProjectionRead::Live { .. }
    ));
    let mut reset = publish(&s, AgentState::Idle);
    reset.episode_reset = true;
    for plan in [
        tma_tmux::stamp::StampPlan::Hold {
            stamped_at: NOW + 1,
            hash: Some(999),
        },
        tma_tmux::stamp::StampPlan::Remove,
        tma_tmux::stamp::StampPlan::EndSession,
        tma_tmux::stamp::StampPlan::Publish(reset),
    ] {
        let edits = store::edits_for_plan(&s.pane, &plan, None).unwrap();
        let out = store::write_fallback(&s.tmux, &s.read(), Guard::Unconditional, || NOW, &edits)
            .unwrap();
        assert_eq!(out, CommitOutcome::Lost(before.clone()));
    }
}

#[test]
fn owner_replacement_and_release_freeze_the_condition_for_all_dependents() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let first = owner(1);
    commit(&s, &first, &publish(&s, AgentState::Working), None);
    let second = owner(2);
    let mut p = publish(&s, AgentState::Blocked);
    p.episode_reset = true;
    let replaced = commit(&s, &second, &p, None);
    let stale = store::release_program(
        &s.tmux,
        &s.read(),
        &first,
        &[
            OptionEdit::unset(public::STATE),
            OptionEdit::unset(public::NAME),
        ],
    )
    .unwrap();
    assert_eq!(stale, CommitOutcome::Lost(replaced));
    let release = store::release_program(
        &s.tmux,
        &s.read(),
        &second,
        &[
            OptionEdit::unset(public::ATTENTION),
            OptionEdit::unset(public::TURN_AT),
            OptionEdit::set(public::NAME, "after-owner-clear"),
            OptionEdit::set(public::STAMPED_AT, "1800000000001"),
        ],
    )
    .unwrap();
    assert!(release.won());
    assert_eq!(release.winner().options[public::NAME], "after-owner-clear");
    assert!(matches!(
        release.winner().projection(NOW).unwrap(),
        ProjectionRead::Stale
    ));
}

#[test]
fn expired_and_implausibly_future_program_hook_tokens_do_not_protect_fallback() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    commit(&s, &owner, &publish(&s, AgentState::Blocked), None);
    let old = s.read();
    assert!(matches!(
        old.projection(NOW + 15_000).unwrap(),
        ProjectionRead::Stale
    ));
    assert!(matches!(
        old.projection(NOW - 2_001).unwrap(),
        ProjectionRead::Stale
    ));
    let edits = [
        OptionEdit::set(public::STATE, "working"),
        OptionEdit::set(public::SOURCE, "capture"),
        OptionEdit::unset(public::ATTENTION),
    ];
    let out =
        store::write_fallback(&s.tmux, &old, Guard::ProtectHook, || NOW + 15_000, &edits).unwrap();
    assert!(out.won());
    assert!(matches!(
        out.winner().projection(NOW + 15_000).unwrap(),
        ProjectionRead::Legacy(_)
    ));
}

#[test]
fn stale_public_tuple_and_malformed_private_markers_allow_fallback_recovery() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    s.lease(&owner.collector);
    for (key, value) in [
        (public::PID, "99".to_owned()),
        (public::STATE, String::new()),
        (public::SOURCE, "capture".to_owned()),
        (public::STAMPED_AT, (NOW - 1).to_string()),
        (public::PID, "malformed".into()),
        (public::STATE, "malformed".into()),
        (public::SOURCE, "malformed".into()),
        (public::HASH, "malformed".into()),
        (public::NOTIFIED_AT, "malformed".into()),
        (opt::OWNER, owner.encode().replacen(":1:1:", ":0:1:", 1)),
        (opt::COMMIT, format!("invalid|{}", owner.encode())),
        (
            opt::COLLECTOR,
            format!("0{}:{}", NOW + 15_000, owner.collector.as_str()),
        ),
    ] {
        commit(&s, &owner, &publish(&s, AgentState::Blocked), None);
        if key == opt::COLLECTOR {
            s.tmux.set_server_option(key, &value).unwrap();
        } else {
            s.tmux.set_pane_option(&s.pane, key, &value).unwrap();
        }
        let stale = s.read();
        assert!(
            !matches!(stale.projection(NOW), Ok(ProjectionRead::Live { .. })),
            "{key}: {:?}",
            stale.projection(NOW)
        );
        let outcome = store::write_fallback(
            &s.tmux,
            &stale,
            Guard::ProtectHook,
            || NOW,
            &[OptionEdit::set(public::STATE, "working")],
        )
        .unwrap();
        assert!(outcome.won(), "stale {key} blocked recovery: {outcome:?}");
        // Repair the intentionally corrupted option before the next independently valid publish.
        if key != opt::COLLECTOR {
            s.tmux.set_pane_option(&s.pane, key, "").unwrap();
        }
    }
}

#[test]
fn revoked_result_history_cannot_authorize_a_new_notification() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    s.lease(&owner.collector);
    let result = result(&owner, 1, ResultKind::Completion);
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&result));
    assert!(store::revoke_program(&s.tmux, &s.pane, &owner).unwrap());
    let stale = s.read();
    assert_eq!(stale.projection(NOW).unwrap(), ProjectionRead::Stale);
    let outcome = store::claim_notification(&s.tmux, &stale, &result, || NOW).unwrap();
    assert!(!outcome.won(), "revoked history authorized a notification");
    assert!(!s.read().options.contains_key(opt::NOTIFIED));
    assert!(store::acknowledge_result(&s.tmux, &s.read(), &result)
        .unwrap()
        .won());
}

#[test]
fn malformed_raw_options_compare_literally_and_invalid_leases_are_reclaimable() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let marker = std::env::temp_dir().join(format!(
        "tma_b5a_format_effect_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let malformed = format!(
        "bad,#{{pane_id}}#}}#[fg=red]\n  # literal comment\n\\\n雪 #(touch {})",
        marker.display()
    );
    for key in [opt::OWNER, opt::COMMIT, opt::RECEIPT, opt::COLLECTOR] {
        s.tmux.set_pane_option(&s.pane, key, &malformed).unwrap();
    }
    s.tmux.set_pane_option(&s.pane, opt::ORIGIN, "1").unwrap();
    assert!(store::write_fallback(
        &s.tmux,
        &s.read(),
        Guard::ProtectHook,
        || NOW,
        &[OptionEdit::set(public::STATE, "working")],
    )
    .unwrap()
    .won());
    for invalid in [
        format!("{}{malformed}", "x".repeat(13_000)),
        malformed,
        format!(" {}:{}\n", NOW + 15_000, token(9).as_str()),
        format!("0{}:{}", NOW + 15_000, token(9).as_str()),
        format!("{}:invalid", NOW + 15_000),
    ] {
        s.tmux.set_server_option(opt::COLLECTOR, &invalid).unwrap();
        assert!(CollectorLease::parse(&invalid).is_none());
        assert!(matches!(
            store::acquire_collector(&s.tmux, &s.pane, &token(1), || NOW).unwrap(),
            LeaseOutcome::Won(Some(_))
        ));
    }
    assert!(
        !marker.exists(),
        "raw comparison text executed a format job"
    );
    assert!(store::probe_capability(&s.tmux, &s.pane).unwrap());
}

#[test]
fn lease_renewal_invalidates_delayed_fallback_decisions_without_pane_receipt_changes() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    let lease = s.lease(&owner.collector);
    let observed = commit(&s, &owner, &publish(&s, AgentState::Working), None);
    assert!(matches!(
        store::renew_collector(&s.tmux, &s.pane, &lease, || NOW + 8_000).unwrap(),
        LeaseOutcome::Won(_)
    ));
    assert_eq!(
        s.read().options[opt::RECEIPT],
        observed.options[opt::RECEIPT]
    );
    // At this old timestamp the renewed lease appears >17 seconds into the future.
    // An exact lease comparison must reject that stale recovery decision.
    let stale = s.read();
    assert_eq!(stale.projection(NOW).unwrap(), ProjectionRead::Stale);
    let lost = store::write_fallback(
        &s.tmux,
        &observed,
        Guard::Unconditional,
        || NOW,
        &[OptionEdit::set(public::STATE, "idle")],
    )
    .unwrap();
    assert!(!lost.won());
    // Sampling the fresh boundary clock also protects a newly read renewed lease.
    assert!(!store::write_fallback(
        &s.tmux,
        &stale,
        Guard::Unconditional,
        || NOW + 8_000,
        &[OptionEdit::set(public::STATE, "idle")],
    )
    .unwrap()
    .won());
    assert_eq!(s.read().options[public::STATE], "working");
}

#[test]
fn submission_that_finishes_after_lease_expiry_never_grants_authority_or_effects() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    let lease = s.lease(&owner.collector);
    // The new expiry is still future at completion, but the previous coverage expired.
    let mut times = [NOW + 5_000, NOW + 16_000].into_iter();
    assert!(matches!(
        store::renew_collector(&s.tmux, &s.pane, &lease, || times.next().unwrap()).unwrap(),
        LeaseOutcome::Lost(_)
    ));
    s.tmux
        .set_server_option(opt::COLLECTOR, &lease.encode())
        .unwrap();
    let mut times = [NOW + 5_000, NOW + 20_000].into_iter();
    assert!(matches!(
        store::renew_collector(&s.tmux, &s.pane, &lease, || times.next().unwrap()).unwrap(),
        LeaseOutcome::Lost(_)
    ));
    // The renewal did apply, but the completion-time clock must reject its authority.
    let mut times = [NOW + 5_000, NOW + 20_000].into_iter();
    assert!(!store::publish_program(
        &s.tmux,
        &s.read(),
        &owner,
        || times.next().unwrap(),
        &publish(&s, AgentState::Working),
        None,
        &[],
    )
    .unwrap()
    .won());
    s.tmux
        .set_server_option(opt::COLLECTOR, &lease.encode())
        .unwrap();
    let current = result(&owner, 1, ResultKind::Completion);
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&current));
    let mut times = [NOW + 5_000, NOW + 20_000].into_iter();
    let outcome =
        store::claim_notification(&s.tmux, &s.read(), &current, || times.next().unwrap()).unwrap();
    assert!(!outcome.won());
    assert_eq!(
        outcome.winner().program.notified.as_ref().unwrap().result,
        current
    );
    // Acquisition uses the same completion-time liveness rule.
    s.run(&["set-option", "-su", opt::COLLECTOR]);
    let mut times = [NOW, NOW + 15_000].into_iter();
    assert!(matches!(
        store::acquire_collector(&s.tmux, &s.pane, &token(2), || times.next().unwrap()).unwrap(),
        LeaseOutcome::Lost(_)
    ));
}

#[test]
fn renewal_between_advisory_read_and_mutation_cannot_revoke_a_healthy_owner() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let Some(s) = Scratch::new() else {
        return;
    };
    let owner = owner(1);
    let lease = s.lease(&owner.collector);
    let before = commit(&s, &owner, &publish(&s, AgentState::Working), None);
    let script = std::env::temp_dir().join(format!(
        "tma_b5a_advisory_renew_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let ready = script.with_extension("ready");
    let gate = script.with_extension("gate");
    std::fs::write(&script, format!(
        "#!/bin/sh\nfor arg do\nif [ \"$arg\" = if-shell ] && [ ! -e '{}' ]; then\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\nfi\ndone\nexec tmux \"$@\"\n",
        ready.display(), ready.display(), gate.display(),
    )).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let delayed = Tmux::connect(&tma_tmux::tmux::Server {
        socket_name: Some(s.socket.clone()),
        bin: Some(script.to_string_lossy().into()),
        ..Default::default()
    });
    let clock = AtomicU64::new(NOW);
    std::thread::scope(|scope| {
        let fallback = scope.spawn(|| {
            store::advisory_fallback(
                &delayed,
                &before,
                Guard::Unconditional,
                &[
                    OptionEdit::set(public::STATE, "idle"),
                    OptionEdit::set(public::NAME, "late"),
                ],
                || clock.load(Ordering::SeqCst),
            )
            .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "advisory mutation did not reach gate"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            store::renew_collector(&s.tmux, &s.pane, &lease, || NOW + 8_000).unwrap(),
            LeaseOutcome::Won(_)
        ));
        clock.store(NOW + 8_000, Ordering::SeqCst);
        std::fs::write(&gate, "release").unwrap();
        let after = fallback.join().unwrap();
        for key in [
            opt::OWNER,
            opt::COMMIT,
            opt::RECEIPT,
            public::STATE,
            public::NAME,
        ] {
            assert_eq!(after.options[key], before.options[key], "{key}");
        }
        assert!(matches!(
            after.projection(NOW + 8_000),
            Ok(ProjectionRead::Live { .. })
        ));
    });
    for path in [script, ready, gate] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn legacy_hook_without_program_metadata_keeps_existing_arbitration() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.tmux
        .set_pane_option(&s.pane, public::SOURCE, "hook")
        .unwrap();
    s.tmux
        .set_pane_option(&s.pane, public::STATE, "blocked")
        .unwrap();
    s.tmux
        .set_pane_option(&s.pane, public::EVIDENCE_AT, &NOW.to_string())
        .unwrap();
    for guard in [
        Guard::ProtectHook,
        Guard::CarveOut { capture_at: NOW },
        Guard::HookArbitrate {
            evidence_at: NOW - 1,
        },
    ] {
        assert!(!store::write_fallback(
            &s.tmux,
            &s.read(),
            guard,
            || NOW,
            &[OptionEdit::set(public::STATE, "working")]
        )
        .unwrap()
        .won());
    }
    assert!(store::write_fallback(
        &s.tmux,
        &s.read(),
        Guard::HookArbitrate { evidence_at: NOW },
        || NOW,
        &[OptionEdit::set(public::STATE, "idle")]
    )
    .unwrap()
    .won());
}

#[test]
fn old_collector_cannot_renew_or_release_new_lease_and_silence_keeps_report_time() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let lease = s.lease(&token(1));
    commit(&s, &owner(1), &publish(&s, AgentState::Working), None);
    let before = s.read();
    let renewed = store::renew_collector(&s.tmux, &s.pane, &lease, || NOW + 5_000).unwrap();
    let LeaseOutcome::Won(Some(renewed)) = renewed else {
        panic!("renew lost")
    };
    assert_eq!(
        before.options[public::EVIDENCE_AT],
        s.read().options[public::EVIDENCE_AT]
    );
    assert!(matches!(
        store::acquire_collector(&s.tmux, &s.pane, &token(2), || NOW + 5_000).unwrap(),
        LeaseOutcome::Lost(_)
    ));
    assert!(matches!(
        store::acquire_collector(&s.tmux, &s.pane, &token(2), || NOW + 20_000).unwrap(),
        LeaseOutcome::Won(_)
    ));
    assert!(matches!(
        store::renew_collector(&s.tmux, &s.pane, &renewed, || NOW + 20_001).unwrap(),
        LeaseOutcome::Lost(_)
    ));
    assert!(matches!(
        store::release_collector(&s.tmux, &s.pane, &token(1)).unwrap(),
        LeaseOutcome::Lost(_)
    ));
    assert_eq!(
        CollectorLease::parse(&s.tmux.get_server_option(opt::COLLECTOR).unwrap().unwrap())
            .unwrap()
            .token,
        token(2)
    );
}

#[test]
fn exact_ack_and_claim_distinguish_idle_to_idle_generations_at_equal_clocks() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    let first = result(&owner, 1, ResultKind::Completion);
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&first));
    assert!(
        store::claim_notification(&s.tmux, &s.read(), &first, || NOW)
            .unwrap()
            .won()
    );
    assert!(
        !store::claim_notification(&s.tmux, &s.read(), &first, || NOW)
            .unwrap()
            .won()
    );
    assert!(store::acknowledge_result(&s.tmux, &s.read(), &first)
        .unwrap()
        .won());
    let second = result(&owner, 2, ResultKind::Failure);
    let mut p = publish(&s, AgentState::Idle);
    p.detail = Some(Detail::new(Detail::ERROR));
    commit(&s, &owner, &p, Some(&second));
    assert!(!store::acknowledge_result(&s.tmux, &s.read(), &first)
        .unwrap()
        .won());
    assert!(
        !store::claim_notification(&s.tmux, &s.read(), &first, || NOW)
            .unwrap()
            .won()
    );
    assert!(
        store::claim_notification(&s.tmux, &s.read(), &second, || NOW)
            .unwrap()
            .won()
    );
    assert!(store::acknowledge_result(&s.tmux, &s.read(), &second)
        .unwrap()
        .won());
    let read = s.read();
    assert_eq!(read.result(), Some(second));
    assert!(matches!(
        read.projection(NOW).unwrap(),
        ProjectionRead::Live { .. }
    ));
    assert!(!read.options.contains_key(public::ATTENTION));
}

#[test]
fn metadata_lifetime_is_independent_and_report_text_is_literal() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    let mut p = publish(&s, AgentState::Working);
    p.name = "pi '\" \\ $HOME `false` ; #{pane_id},\nline".into();
    let committed = commit(&s, &owner, &p, None);
    assert_eq!(committed.options[public::NAME], p.name);
    let metadata = MetadataOwner {
        process: owner.processes.agent(),
        session: "session,#{pane_id}'".into(),
    };
    let out = store::write_metadata(
        &s.tmux,
        &s.read(),
        None,
        Some(&metadata),
        &[
            OptionEdit::set(public::SESSION, &metadata.session),
            OptionEdit::set(public::MODEL, "model1"),
        ],
    )
    .unwrap();
    assert!(out.won());
    assert_eq!(out.winner().options[public::STATE], "working");
    assert!(matches!(
        out.winner().projection(NOW).unwrap(),
        ProjectionRead::Live { .. }
    ));
    assert!(!store::write_metadata(
        &s.tmux,
        &s.read(),
        None,
        None,
        &[OptionEdit::unset(public::SESSION)]
    )
    .unwrap()
    .won());
    assert!(store::write_metadata(
        &s.tmux,
        &s.read(),
        Some(&metadata),
        None,
        &[OptionEdit::unset(public::SESSION)]
    )
    .unwrap()
    .won());
}

#[test]
fn argv_and_streamed_values_preserve_multiline_parser_metacharacters() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let literal = concat!(
        "' \" \\ $HOME `false` ; #{pane_id} #(false),{} 雪\n",
        "  indented\n\t# comment stays\n",
        "\\\nbackslash newline\n",
        "%if 1\nTMA_TRANSPORT_ASSIGNMENT=literal\n%endif\n",
        " trailing \\"
    );
    for repeats in [1, 512] {
        let value = literal.repeat(repeats);
        let outcome = store::write_fallback(
            &s.tmux,
            &s.read(),
            Guard::Unconditional,
            || NOW,
            &[OptionEdit::set(public::NAME, &value)],
        )
        .unwrap();
        assert!(
            outcome.won(),
            "literal value changed with {repeats} repeats"
        );
        assert_eq!(outcome.winner().options[public::NAME], value);
    }
}

#[test]
fn streamed_loading_finishes_before_receipt_lease_and_hook_decisions() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    for change in ["receipt", "lease", "hook"] {
        let Some(s) = Scratch::new() else {
            return;
        };
        let owner = owner(1);
        s.lease(&owner.collector);
        let path = std::env::temp_dir().join(format!(
            "tma_b5a_loading_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let ready = path.with_extension("ready");
        let gate = path.with_extension("gate");
        // Stream every byte into the real client, then hold its pipe open. source-file must
        // await EOF, so a second producer can change the decision before the group runs.
        std::fs::write(&path, format!(
            "#!/bin/sh\nfor arg do\nif [ \"$arg\" = source-file ]; then\n{{ cat; touch '{}'; i=0; while [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done; }} | tmux \"$@\"\nexit $?\nfi\ndone\nexec tmux \"$@\"\n",
            ready.display(), gate.display(),
        )).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&tma_tmux::tmux::Server {
            socket_name: Some(s.socket.clone()),
            bin: Some(path.to_string_lossy().into()),
            ..Default::default()
        });
        let observed = s.read();
        let mut p = publish(&s, AgentState::Working);
        p.name = "streamed loading".repeat(1500);
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                store::publish_program(&delayed, &observed, &owner, || NOW, &p, None, &[])
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "streamed input did not load");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(s.read(), observed, "group ran before loading reached EOF");
            match change {
                "receipt" => s
                    .tmux
                    .set_pane_option(&s.pane, opt::RECEIPT, token(9).as_str())
                    .unwrap(),
                "lease" => s
                    .tmux
                    .set_server_option(
                        opt::COLLECTOR,
                        &CollectorLease {
                            token: token(2),
                            expires_at: NOW + 15_000,
                        }
                        .encode(),
                    )
                    .unwrap(),
                "hook" => s.run(&[
                    "set-hook",
                    "-t",
                    "first",
                    "after-set-option",
                    "run-shell 'sleep 0.02'",
                ]),
                _ => unreachable!(),
            }
            let changed = s.read();
            std::fs::write(&gate, "release").unwrap();
            let outcome = pending.join().unwrap();
            if change == "hook" {
                assert!(matches!(outcome, Err(PersistenceError::Unsupported)));
            } else {
                assert!(!outcome.unwrap().won());
            }
            assert_eq!(
                s.read(),
                changed,
                "a loading-time {change} change was missed"
            );
        });
        for file in [path, ready, gate] {
            std::fs::remove_file(file).unwrap();
        }
    }
}

#[test]
fn hook_added_after_probe_refuses_writes_but_single_mutation_revocation_works() {
    let Some(s) = Scratch::new() else {
        return;
    };
    assert!(store::probe_capability(&s.tmux, &s.pane).unwrap());
    let lease = s.lease(&token(1));
    let owner = owner(1);
    commit(&s, &owner, &publish(&s, AgentState::Working), None);
    s.run(&[
        "set-hook",
        "-g",
        "after-set-option[4]",
        "run-shell 'sleep 0.02'",
    ]);
    assert!(!store::probe_capability(&s.tmux, &s.pane).unwrap());
    let read = s.read();
    assert!(matches!(
        store::publish_program(
            &s.tmux,
            &read,
            &owner,
            || NOW,
            &publish(&s, AgentState::Idle),
            None,
            &[]
        ),
        Err(PersistenceError::Unsupported)
    ));
    assert!(matches!(
        store::renew_collector(&s.tmux, &s.pane, &lease, || NOW + 5_000),
        Err(PersistenceError::Unsupported)
    ));
    assert_eq!(s.read().options[public::STATE], "working");
    assert!(store::revoke_program(&s.tmux, &s.pane, &owner).unwrap());
    assert!(matches!(
        s.read().projection(NOW).unwrap(),
        ProjectionRead::Stale
    ));
    assert!(matches!(
        store::release_collector(&s.tmux, &s.pane, &token(1)).unwrap(),
        LeaseOutcome::Won(_)
    ));
}

#[test]
fn explicit_targets_use_same_hook_scope_for_lease_and_pane_mutations() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.run(&["new-session", "-d", "-s", "second", "sleep 120"]);
    let pane2 = s.tmux.pane_format("second:", "#{pane_id}").unwrap();
    s.run(&[
        "set-hook",
        "-t",
        "second",
        "after-set-option",
        "run-shell 'sleep 0.02'",
    ]);
    assert!(!store::probe_capability(&s.tmux, &pane2).unwrap());
    assert!(store::probe_capability(&s.tmux, &s.pane).unwrap());
    s.lease(&token(1));
    commit(&s, &owner(1), &publish(&s, AgentState::Working), None);
    assert!(matches!(
        store::renew_collector(
            &s.tmux,
            &pane2,
            &CollectorLease {
                token: token(1),
                expires_at: NOW + 15_000
            },
            || NOW + 5_000
        ),
        Err(PersistenceError::Unsupported)
    ));
    // The same physical pane has a supported and an unsupported session context.
    s.run(&["link-window", "-s", "first:0", "-t", "second:9"]);
    let supported = "first:0.0";
    let unsupported = "second:9.0";
    assert!(store::probe_capability(&s.tmux, supported).unwrap());
    assert!(!store::probe_capability(&s.tmux, unsupported).unwrap());
    assert!(matches!(
        store::renew_collector(
            &s.tmux,
            supported,
            &CollectorLease {
                token: token(1),
                expires_at: NOW + 15_000
            },
            || NOW + 5_000,
        )
        .unwrap(),
        LeaseOutcome::Won(_)
    ));
    let mut p = publish(&s, AgentState::Blocked);
    p.pane_id = supported.into();
    p.name = "explicit streamed target '\n  # comment\n".repeat(512);
    assert!(store::publish_program(
        &s.tmux,
        &store::read_pane(&s.tmux, supported).unwrap(),
        &owner(2),
        || NOW + 5_000,
        &p,
        None,
        &[],
    )
    .unwrap()
    .won());
    assert!(!store::revoke_program(&s.tmux, unsupported, &owner(1)).unwrap());
    assert!(store::revoke_program(&s.tmux, unsupported, &owner(2)).unwrap());
    assert_eq!(
        s.read().projection(NOW + 5_000).unwrap(),
        ProjectionRead::Stale
    );
}

#[test]
fn maximum_lineage_fits_native_publication_and_contextual_guards() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let mut owner = owner(1);
    owner.episode = EpisodeId::new(u64::MAX);
    owner.stream_epoch = u64::MAX;
    owner.processes = ProcessChain::new(
        (0..256)
            .map(|offset| ProcessIdentity {
                pid: u32::MAX - offset,
                birth: ProcessBirth::MacOsStart {
                    seconds: u64::MAX,
                    microseconds: 999_999,
                },
            })
            .collect(),
    )
    .unwrap();
    s.lease(&owner.collector);
    let mut p = publish(&s, AgentState::Idle);
    p.pid = owner.processes.agent().pid;
    p.name = "pi '\" \\ $HOME `false` ; #{pane_id},\nline".into();
    let mut current = result(&owner, u64::MAX, ResultKind::Failure);
    current.identity.record = RecordId::parse(
        &[
            "x".repeat(32),
            "y".repeat(32),
            "z".repeat(32),
            "w".repeat(29),
        ]
        .join("/"),
    )
    .unwrap();
    p.detail = Some(Detail::new(Detail::ERROR));
    let before = commit(&s, &owner, &p, Some(&current));
    assert_eq!(before.options[public::NAME], p.name);
    assert!(!store::write_fallback(
        &s.tmux,
        &before,
        Guard::Unconditional,
        || NOW,
        &[OptionEdit::set(public::STATE, "idle")],
    )
    .unwrap()
    .won());
    assert!(
        store::claim_notification(&s.tmux, &s.read(), &current, || NOW,)
            .unwrap()
            .won()
    );
    let notified = s.read();
    assert_eq!(notified.result(), Some(current.clone()));
    assert_eq!(notified.program.notified.as_ref().unwrap().result, current);
    assert!(store::acknowledge_result(&s.tmux, &notified, &current)
        .unwrap()
        .won());
    let acknowledged = s.read();
    assert_eq!(acknowledged.program.acknowledged, Some(current.clone()));
    assert!(!acknowledged.options.contains_key(public::ATTENTION));
    store::invalidate_history(
        &s.tmux,
        &acknowledged,
        &owner,
        &tma_core::persistence::HistoryInvalidation::All,
    )
    .unwrap();
    let mut replacement = owner.clone();
    replacement.episode = EpisodeId::new(u64::MAX - 1);
    replacement.stream_epoch = u64::MAX - 1;
    let mut rebound = current.clone();
    rebound.owner = replacement.clone();
    rebound.identity.owner = replacement.episode;
    p.set_attention = false;
    let read = store::publish_program_with_history(
        &s.tmux,
        &s.read(),
        &replacement,
        || NOW,
        &p,
        Some(&rebound),
        &[],
        &tma_core::persistence::ResultHistory::Rebind {
            acknowledged: true,
            notified: true,
        },
        None,
    )
    .unwrap();
    assert!(read.won());
    assert_eq!(read.winner().program.acknowledged, Some(rebound.clone()));
    assert_eq!(
        read.winner()
            .program
            .notified
            .as_ref()
            .map(|claim| &claim.result),
        Some(&rebound)
    );
    assert!(read.winner().program.invalidated.is_none());
    assert!(
        store::release_program(&s.tmux, read.winner(), &replacement, &[])
            .unwrap()
            .won()
    );
    assert_eq!(s.read().projection(NOW).unwrap(), ProjectionRead::Stale);
}

#[test]
fn concurrent_equal_clock_fallback_writers_commit_one_complete_winner() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let observed = s.read();
    let barrier = std::sync::Barrier::new(8);
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (1..=8)
            .map(|i| {
                let observed = &observed;
                let barrier = &barrier;
                let tmux = &s.tmux;
                scope.spawn(move || {
                    barrier.wait();
                    let value = i.to_string();
                    store::write_fallback(
                        tmux,
                        observed,
                        Guard::Unconditional,
                        || NOW,
                        &[
                            OptionEdit::set(
                                public::STATE,
                                if i % 2 == 0 { "working" } else { "blocked" },
                            ),
                            OptionEdit::set(public::NAME, &value),
                            OptionEdit::set(public::PID, &value),
                            OptionEdit::set(public::HASH, &value),
                            OptionEdit::set(public::STAMPED_AT, NOW.to_string()),
                            OptionEdit::set(public::PENDING_CALL, &value),
                            OptionEdit::set(public::TURN_AT, NOW.to_string()),
                            OptionEdit::set(public::ATTENTION, "1"),
                        ],
                    )
                    .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|o| o.won()).count(), 1);
    let winner = s.read();
    let identity = &winner.options[public::NAME];
    for key in [public::PID, public::HASH, public::PENDING_CALL] {
        assert_eq!(&winner.options[key], identity);
    }
    assert_eq!(winner.options[public::STAMPED_AT], NOW.to_string());
    for outcome in outcomes {
        assert_eq!(outcome.winner(), &winner);
    }
}

#[test]
fn partial_execution_with_yielding_command_error_hook_never_commits_marker() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    let read = commit(&s, &owner, &publish(&s, AgentState::Working), None);
    s.run(&["set-hook", "-g", "command-error", "run-shell 'sleep 0.02'"]);
    let body = [
        tma_core::render::unset_pane_option(&s.pane, opt::COMMIT),
        tma_core::render::set_pane_option(&s.pane, public::STATE, "idle"),
        tma_core::render::StampCommand {
            argv: vec![
                "set-option".into(),
                "-p".into(),
                "-t".into(),
                s.pane.clone(),
                "__tma_invalid_builtin_option".into(),
                "value".into(),
            ],
        },
        tma_core::render::set_pane_option(&s.pane, opt::COMMIT, &read.options[opt::COMMIT]),
        tma_core::render::set_pane_option(&s.pane, opt::RECEIPT, token(999).as_str()),
    ];
    // A command-error hook can change the client exit status, but the failed group's suffix
    // is still removed. A receipt and full readback decide success.
    let _exit = s
        .tmux
        .apply(&[tma_core::render::conditional_group(&s.pane, "1", &body)]);
    let after = s.read();
    assert_eq!(after.options[public::STATE], "idle");
    assert!(!after.options.contains_key(opt::COMMIT));
    assert_eq!(after.options[opt::RECEIPT], read.options[opt::RECEIPT]);
    assert_eq!(after.projection(NOW).unwrap(), ProjectionRead::Stale);
}

#[test]
fn streamed_runtime_failure_invalidates_commit_before_any_new_owner_can_be_live() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let before = commit(&s, &owner(1), &publish(&s, AgentState::Working), None);
    s.run(&["set-hook", "-g", "command-error", "run-shell 'sleep 0.02'"]);
    let path = std::env::temp_dir().join(format!(
        "tma_b5a_stream_runtime_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "#!/bin/sh\nfor arg do\nif [ \"$arg\" = source-file ]; then\nsed 's/@agent_name/__tma_invalid_builtin_option/g' | tmux \"$@\"\nexit $?\nfi\ndone\nexec tmux \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failing = Tmux::connect(&tma_tmux::tmux::Server {
        socket_name: Some(s.socket.clone()),
        bin: Some(path.to_string_lossy().into()),
        ..Default::default()
    });
    let next_owner = owner(2);
    let next_result = result(&next_owner, 1, ResultKind::Completion);
    let mut p = publish(&s, AgentState::Idle);
    p.name = "runtime failure".repeat(1500);
    let outcome = store::publish_program(
        &failing,
        &before,
        &next_owner,
        || NOW,
        &p,
        Some(&next_result),
        &[],
    );
    std::fs::remove_file(path).unwrap();
    assert!(!outcome.is_ok_and(|outcome| outcome.won()));
    let after = s.read();
    assert_eq!(after.owner(), Some(next_owner));
    assert!(!after.options.contains_key(opt::COMMIT));
    assert_eq!(after.options[opt::RECEIPT], before.options[opt::RECEIPT]);
    assert_eq!(after.projection(NOW).unwrap(), ProjectionRead::Stale);
    assert!(
        !store::claim_notification(&s.tmux, &after, &next_result, || NOW)
            .unwrap()
            .won()
    );
}

#[test]
fn failed_readback_never_returns_won_even_after_server_applied_the_write() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Scratch::new() else {
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "tma_b5a_readback_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "#!/bin/sh\nfor arg do\nif [ \"$arg\" = display-message ]; then exit 71; fi\ndone\nexec tmux \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failing = Tmux::connect(&tma_tmux::tmux::Server {
        socket_name: Some(s.socket.clone()),
        bin: Some(path.to_string_lossy().into()),
        ..Default::default()
    });
    let large_name = "literal '#{pane_id},\n".repeat(1000);
    let outcome = store::write_fallback(
        &failing,
        &s.read(),
        Guard::Unconditional,
        || NOW,
        &[
            OptionEdit::set(public::STATE, "working"),
            OptionEdit::set(public::NAME, &large_name),
        ],
    );
    std::fs::remove_file(path).unwrap();
    assert!(matches!(outcome, Err(PersistenceError::Tmux(_))));
    assert_eq!(s.read().options[public::STATE], "working");
    assert_eq!(s.read().options[public::NAME], large_name);
}

#[test]
fn advisory_poll_hint_requires_its_complete_receipt_and_target_capability() {
    use std::os::unix::fs::PermissionsExt;
    {
        let Some(s) = Scratch::new() else { return };
        let original = s.read();
        s.lease(&token(1));
        let live = commit(&s, &owner(1), &publish(&s, AgentState::Working), None);
        let outcome = store::write_fallback_with_hint(
            &s.tmux,
            &original,
            Guard::Unconditional,
            || NOW,
            &[OptionEdit::set(public::STATE, "idle")],
            Some(store::PollHint::new(NOW)),
        )
        .unwrap();
        assert!(!outcome.won());
        assert_eq!(s.read(), live);
        assert_eq!(s.tmux.get_server_option(public::LAST_POLL).unwrap(), None);
    }
    for capability_loss in [true, false] {
        let Some(s) = Scratch::new() else { return };
        let original = s.read();
        let session = s.tmux.pane_format(&s.pane, "#{session_id}").unwrap();
        let target = format!("{session}:.{}", s.pane);
        let mut observed = original.clone();
        observed.target = target.clone();
        let ready = s.workdir.join("hint-ready");
        let release = s.workdir.join("hint-release");
        let wrapper = s.workdir.join("hint-gate");
        let command = if capability_loss {
            "if-shell"
        } else {
            "display-message"
        };
        std::fs::write(&wrapper, format!("#!/bin/sh\nfor arg do\nif [ \"$arg\" = '{command}' ] && [ ! -e '{ready}' ]; then\ntouch '{ready}'\ni=0\nwhile [ ! -e '{release}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\n[ -e '{release}' ] || exit 125\nfi\ndone\nexec tmux \"$@\"\n", ready=ready.display(), release=release.display())).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = Tmux::connect(&tma_tmux::tmux::Server {
            socket_name: Some(s.socket.clone()),
            bin: Some(wrapper.display().to_string()),
            ..Default::default()
        });
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                store::write_fallback_with_hint(
                    &delayed,
                    &observed,
                    Guard::Unconditional,
                    || NOW,
                    &[OptionEdit::set(public::STATE, "working")],
                    Some(store::PollHint::new(NOW)),
                )
            });
            tma_test_support::poll_until("hint invocation reached native gate", || ready.exists());
            if capability_loss {
                s.run(&[
                    "set-hook",
                    "-t",
                    &session,
                    "after-set-option",
                    "run-shell true",
                ]);
            } else {
                let executed = s.read();
                assert_eq!(executed.options[public::STATE], "working");
                assert_ne!(
                    executed.options.get(opt::RECEIPT),
                    original.options.get(opt::RECEIPT)
                );
                assert_eq!(
                    s.tmux
                        .get_server_option(public::LAST_POLL)
                        .unwrap()
                        .as_deref(),
                    Some(NOW.to_string().as_str())
                );
                assert!(store::write_fallback(
                    &s.tmux,
                    &executed,
                    Guard::Unconditional,
                    || NOW,
                    &[OptionEdit::set(public::STATE, "idle")]
                )
                .unwrap()
                .won());
            }
            std::fs::write(&release, "release").unwrap();
            let outcome = pending.join().unwrap();
            if capability_loss {
                assert!(
                    matches!(outcome, Err(PersistenceError::Unsupported)),
                    "{outcome:?}"
                );
                assert_eq!(s.read().options, original.options);
                assert_eq!(s.tmux.get_server_option(public::LAST_POLL).unwrap(), None);
            } else {
                assert!(!outcome.unwrap().won());
                assert_eq!(s.read().options[public::STATE], "idle");
                assert_eq!(
                    s.tmux
                        .get_server_option(public::LAST_POLL)
                        .unwrap()
                        .as_deref(),
                    Some(NOW.to_string().as_str())
                );
            }
        });
    }
}

#[test]
fn streamed_parse_failure_and_script_bound_leave_the_pane_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Scratch::new() else {
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "tma_b5a_stream_parse_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "#!/bin/sh\nfor arg do\nif [ \"$arg\" = source-file ]; then\ncat >/dev/null\nprintf '%s\\n' 'not-a-tmux-command' | tmux \"$@\"\nexit $?\nfi\ndone\nexec tmux \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failing = Tmux::connect(&tma_tmux::tmux::Server {
        socket_name: Some(s.socket.clone()),
        bin: Some(path.to_string_lossy().into()),
        ..Default::default()
    });
    let before = s.read();
    assert!(matches!(
        store::write_fallback(
            &failing,
            &before,
            Guard::Unconditional,
            || NOW,
            &[
                OptionEdit::set(public::STATE, "working"),
                OptionEdit::set(public::NAME, "x".repeat(20_000))
            ],
        ),
        Err(PersistenceError::Tmux(_))
    ));
    std::fs::remove_file(path).unwrap();
    assert_eq!(s.read(), before);
    assert!(matches!(
        store::write_fallback(
            &s.tmux,
            &before,
            Guard::Unconditional,
            || NOW,
            &[
                OptionEdit::set(public::STATE, "working"),
                OptionEdit::set(public::NAME, "'".repeat(70_000))
            ],
        ),
        Err(PersistenceError::Tmux(
            tma_tmux::tmux::TmuxError::Parse { .. }
        ))
    ));
    assert_eq!(s.read(), before);
}

#[test]
fn old_capability_probe_failure_disables_authority_without_a_version_check() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Scratch::new() else {
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "tma_b5a_old_capability_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    for unavailable in ["if-shell", "source-file"] {
        std::fs::write(&path, format!("#!/bin/sh\nfor arg do\nif [ \"$arg\" = {unavailable} ]; then exit 72; fi\ndone\nexec tmux \"$@\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let unsupported = Tmux::connect(&tma_tmux::tmux::Server {
            socket_name: Some(s.socket.clone()),
            bin: Some(path.to_string_lossy().into()),
            ..Default::default()
        });
        assert!(matches!(
            store::require_capability(&unsupported, &s.pane),
            Err(PersistenceError::Unsupported)
        ));
    }
    std::fs::remove_file(path).unwrap();
    assert!(!s.read().options.contains_key(opt::OWNER));
}

#[test]
fn yielding_advisory_suffix_cannot_touch_protocol_owner_after_hook_removal() {
    use std::time::{Duration, Instant};
    let Some(s) = Scratch::new() else {
        return;
    };
    s.tmux
        .set_pane_option(&s.pane, public::STATE, "working")
        .unwrap();
    let gate = std::env::temp_dir().join(format!(
        "tma_b5a_advisory_gate_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let hook = format!("if-shell -F '#{{==:#{{@agent_state}},idle}}' \"run-shell 'i=0; while [ ! -e {} ] && [ \\$i -lt 200 ]; do sleep 0.01; i=\\$((i+1)); done'\"", gate.display());
    s.run(&["set-hook", "-g", "after-set-option", &hook]);
    std::thread::scope(|scope| {
        let fallback = scope.spawn(|| {
            store::advisory_fallback(
                &s.tmux,
                &store::read_pane(&s.tmux, &s.pane).unwrap(),
                Guard::Unconditional,
                &[
                    OptionEdit::set(public::STATE, "idle"),
                    OptionEdit::set(public::NAME, "late-fallback"),
                    OptionEdit::set(public::HASH, "999"),
                    OptionEdit::unset(public::ATTENTION),
                ],
                || NOW,
            )
            .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while s
            .tmux
            .get_pane_option(&s.pane, public::STATE)
            .unwrap()
            .as_deref()
            != Some("idle")
        {
            assert!(
                Instant::now() < deadline,
                "advisory write did not reach yielding state mutation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        s.run(&["set-hook", "-gu", "after-set-option"]);
        s.lease(&token(1));
        commit(&s, &owner(1), &publish(&s, AgentState::Blocked), None);
        std::fs::write(&gate, "release").unwrap();
        let persisted = fallback.join().unwrap();
        std::fs::remove_file(&gate).unwrap();
        assert_eq!(persisted.options[public::STATE], "blocked");
        assert_eq!(persisted.options[public::NAME], "pi");
        assert_eq!(persisted.options[public::HASH], "12");
        assert_eq!(persisted.options[public::ATTENTION], "1");
        assert!(matches!(
            persisted.projection(NOW).unwrap(),
            ProjectionRead::Live { .. }
        ));
    });
}

#[test]
fn advisory_metadata_and_ack_continue_with_yielding_hooks_under_exact_owners() {
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    let current = result(&owner, 1, ResultKind::Completion);
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&current));
    s.run(&[
        "set-hook",
        "-g",
        "after-set-option",
        "run-shell 'sleep 0.01'",
    ]);
    let metadata = MetadataOwner {
        process: owner.processes.agent(),
        session: "independent".into(),
    };
    let bound = store::advisory_metadata(
        &s.tmux,
        &s.read(),
        None,
        Some(&metadata),
        &[
            OptionEdit::set(public::SESSION, "independent"),
            OptionEdit::set(public::MODEL, "model"),
        ],
    )
    .unwrap();
    assert_eq!(bound.options[public::MODEL], "model");
    let rejected = store::advisory_metadata(
        &s.tmux,
        &s.read(),
        None,
        None,
        &[OptionEdit::unset(public::SESSION)],
    )
    .unwrap();
    assert_eq!(rejected.options[public::SESSION], "independent");
    let acknowledged = store::advisory_acknowledge(&s.tmux, &s.read(), &current).unwrap();
    assert_eq!(acknowledged.program.acknowledged, Some(current));
    assert!(!acknowledged.options.contains_key(public::ATTENTION));
}

#[test]
fn interrupted_advisory_ack_persists_identity_before_visual_clear_and_skips_deferred_results() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Scratch::new() else {
        return;
    };
    s.lease(&token(1));
    let owner = owner(1);
    let first = result(&owner, 1, ResultKind::Completion);
    let mut deferred = publish(&s, AgentState::Idle);
    deferred.set_attention = false;
    commit(&s, &owner, &deferred, Some(&first));
    let absent = store::advisory_acknowledge(&s.tmux, &s.read(), &first).unwrap();
    assert!(absent.program.acknowledged.is_none());
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&first));
    s.run(&[
        "set-hook",
        "-g",
        "after-set-option",
        "run-shell 'sleep 0.02'",
    ]);
    let path = std::env::temp_dir().join(format!(
        "tma_b5a_ack_prefix_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "#!/bin/sh\nfor arg do\ncase \"$arg\" in *\"'@agent_attention'\"*) exit 73;; esac\ndone\nexec tmux \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let interrupted = Tmux::connect(&tma_tmux::tmux::Server {
        socket_name: Some(s.socket.clone()),
        bin: Some(path.to_string_lossy().into()),
        ..Default::default()
    });
    let outcome = store::advisory_acknowledge(&interrupted, &s.read(), &first);
    std::fs::remove_file(path).unwrap();
    assert!(outcome.is_err());
    let prefix = s.read();
    assert_eq!(prefix.program.acknowledged, Some(first.clone()));
    assert_eq!(prefix.options[public::ATTENTION], "1");
    s.run(&["set-hook", "-gu", "after-set-option"]);
    assert!(
        !store::claim_notification(&s.tmux, &s.read(), &first, || NOW)
            .unwrap()
            .won(),
        "durably acknowledged result remained actionable after an interrupted clear"
    );
    let second = result(&owner, 2, ResultKind::Completion);
    commit(&s, &owner, &publish(&s, AgentState::Idle), Some(&second));
    let late = store::advisory_acknowledge(&s.tmux, &s.read(), &first).unwrap();
    assert_eq!(late.options[public::ATTENTION], "1");
    assert_eq!(late.result(), Some(second));
}

#[test]
fn maximum_lineage_blocked_history_publication_claim_and_ack_use_bounded_transport() {
    let Some(s) = Scratch::new() else {
        return;
    };
    let mut current = owner(1);
    current.processes = ProcessChain::new(
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
    .unwrap();
    s.lease(&current.collector);
    let mut tuple = publish(&s, AgentState::Blocked);
    tuple.pid = current.processes.agent().pid;
    let outcome =
        store::publish_program(&s.tmux, &s.read(), &current, || NOW, &tuple, None, &[]).unwrap();
    assert!(outcome.won());
    let history = tma_core::persistence::BlockedHistory::parse(
        &outcome.winner().options[opt::BLOCKED_HISTORY],
    )
    .unwrap();
    assert_eq!(history.processes, current.processes);
    assert_eq!(history.processes.as_slice().len(), 256);
    let observation = store::AttentionObservation::from_read(s.read(), NOW).unwrap();
    assert!(store::claim_observation(&s.tmux, &observation, || NOW)
        .unwrap()
        .claimed());
    let observation = store::AttentionObservation::from_read(s.read(), NOW).unwrap();
    assert!(store::acknowledge_observation(&s.tmux, &observation)
        .unwrap()
        .won());
    let final_read = s.read();
    assert!(!final_read.options.contains_key(public::ATTENTION));
    assert!(
        tma_core::persistence::BlockedHistory::parse(&final_read.options[opt::BLOCKED_HISTORY])
            .unwrap()
            .acknowledged
    );
    let mut malformed = final_read.options[opt::BLOCKED_HISTORY].clone();
    malformed.push_str("|trailing");
    assert!(tma_core::persistence::BlockedHistory::parse(&malformed).is_none());
    assert!(tma_core::persistence::BlockedHistory::parse(&"x".repeat(32_768)).is_none());
}
