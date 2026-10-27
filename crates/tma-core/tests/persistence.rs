use std::collections::HashMap;

use tma_core::persistence::*;
use tma_core::program_status::{
    EpisodeId, ProgramResult, RecordId, ResultGeneration, ResultIdentity, ResultKind, WorkCycle,
};
use tma_core::snapshot::{ProcessBirth, ProcessIdentity};
use tma_core::stamp::opt as public;

const NOW: u64 = 1_800_000_000_000;
fn time(monotonic_ms: u64, epoch_ms: u64) -> LeaseTime {
    LeaseTime {
        monotonic_ms,
        epoch_ms,
    }
}
fn token(n: u64) -> Token {
    Token::parse(&format!("{n:032x}")).unwrap()
}
fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        birth: ProcessBirth::MacOsStart {
            seconds: 1_700_000_000,
            microseconds: pid,
        },
    }
}
fn owner() -> PaneOwner {
    PaneOwner {
        collector: token(1),
        episode: EpisodeId::new(2),
        processes: ProcessChain::new(vec![process(10), process(11), process(12)]).unwrap(),
        stream_epoch: 3,
    }
}
fn options() -> HashMap<String, String> {
    let owner = owner();
    [
        (public::STATE, "idle".into()),
        (public::SOURCE, "hook".into()),
        (public::EVIDENCE_AT, NOW.to_string()),
        (public::STAMPED_AT, NOW.to_string()),
        (public::SINCE, NOW.to_string()),
        (public::PID, "12".into()),
        (opt::OWNER, owner.encode()),
        (opt::ORIGIN, "1".into()),
        (
            opt::COMMIT,
            CommitMarker {
                write: token(9),
                owner: owner.clone(),
            }
            .encode(),
        ),
        (
            opt::COLLECTOR,
            CollectorLease {
                token: owner.collector,
                expires_at: NOW + LEASE_DURATION_MS,
            }
            .encode(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

#[test]
fn tokens_are_exact_canonical_128_bit_machine_values() {
    for invalid in [
        "",
        "0",
        "0123456789ABCDEF0123456789ABCDEF",
        "0000000000000000000000000000000g",
        "00000000000000000000000000000000:extra",
    ] {
        assert!(Token::parse(invalid).is_none(), "{invalid}");
    }
    assert_eq!(token(123).as_str(), "0000000000000000000000000000007b");
}

#[test]
fn full_lineage_roundtrips_and_intermediate_birth_changes_identity() {
    let original = owner();
    assert_eq!(PaneOwner::parse(&original.encode()), Some(original.clone()));
    let mut lineage = original.processes.as_slice().to_vec();
    lineage[1].birth = ProcessBirth::LinuxStartTicks(999);
    let mut changed = original.clone();
    changed.processes = ProcessChain::new(lineage).unwrap();
    assert_ne!(changed, original);
    assert_eq!(PaneOwner::parse(&changed.encode()), Some(changed));
    assert!(ProcessChain::new(vec![]).is_none());
    assert!(ProcessChain::new(vec![process(0)]).is_none());
    assert!(ProcessChain::new(vec![process(1), process(1)]).is_none());
    assert!(ProcessChain::new((1..=256).map(process).collect()).is_some());
    assert!(ProcessChain::new((1..=257).map(process).collect()).is_none());
    assert!(PaneOwner::parse(&(original.encode() + "~malformed")).is_none());
}

#[test]
fn lease_grammar_bounds_and_owner_mismatch_are_fail_closed() {
    let lease = CollectorLease {
        token: token(1),
        expires_at: NOW + 15_000,
    };
    assert_eq!(CollectorLease::parse(&lease.encode()), Some(lease.clone()));
    assert!(lease.live_at(NOW));
    assert!(!lease.live_at(NOW + 15_000));
    assert!(lease.live_at(NOW - 2_000));
    assert!(!lease.live_at(NOW - 2_001));
    assert!(CollectorLease::parse(&format!("+{}:{}", NOW, token(1).as_str())).is_none());
    assert!(CollectorLease::parse(&format!("0{}:{}", NOW, token(1).as_str())).is_none());
    let mut opts = options();
    assert!(matches!(
        read_projection(&opts, NOW).unwrap(),
        ProjectionRead::Live { .. }
    ));
    opts.insert(
        opt::COLLECTOR.into(),
        CollectorLease {
            token: token(2),
            ..lease
        }
        .encode(),
    );
    assert_eq!(read_projection(&opts, NOW).unwrap(), ProjectionRead::Stale);
}

#[test]
fn incomplete_or_revoked_program_hook_projection_is_stale_while_legacy_remains_legacy() {
    for key in [opt::COMMIT, opt::OWNER, opt::COLLECTOR, opt::ORIGIN] {
        let mut opts = options();
        opts.remove(key);
        assert_eq!(
            read_projection(&opts, NOW).unwrap(),
            ProjectionRead::Stale,
            "{key}"
        );
    }
    let mut opts = options();
    let mut wrong = owner();
    wrong.stream_epoch += 1;
    opts.insert(
        opt::COMMIT.into(),
        CommitMarker {
            write: token(9),
            owner: wrong,
        }
        .encode(),
    );
    assert_eq!(read_projection(&opts, NOW).unwrap(), ProjectionRead::Stale);
    for key in opt::PANE_KEYS {
        opts.remove(*key);
    }
    assert!(matches!(
        read_projection(&opts, NOW).unwrap(),
        ProjectionRead::Legacy(Some(_))
    ));
}

#[test]
fn normalized_failure_ack_and_claim_preserve_exact_generation_cycle_and_record() {
    let owner = owner();
    let result = ProgramResult {
        identity: ResultIdentity {
            owner: owner.episode,
            record: RecordId::parse("root/tool-1").unwrap(),
            generation: ResultGeneration::new(3),
        },
        kind: ResultKind::Failure,
        cycle: WorkCycle::new(4),
        pending: true,
    };
    let persisted = PersistedResult::new(owner.clone(), &result).unwrap();
    assert_eq!(
        PersistedResult::parse(&persisted.encode()),
        Some(persisted.clone())
    );
    let claim = NotificationClaim {
        token: token(99),
        result: persisted.clone(),
    };
    assert_eq!(NotificationClaim::parse(&claim.encode()), Some(claim));
    let mut wrong_owner = owner;
    wrong_owner.episode = EpisodeId::new(999);
    assert!(PersistedResult::new(wrong_owner, &result).is_none());
    let mut opts = options();
    opts.remove(opt::COMMIT);
    opts.insert(opt::RESULT.into(), persisted.encode());
    let parsed = ProgramMetadata::from_options(&opts);
    assert_eq!(parsed.result, Some(persisted));
    assert_eq!(
        parsed.projection(&opts, NOW).unwrap(),
        ProjectionRead::Stale
    );
}

#[test]
fn independent_metadata_encodes_session_text_without_format_metacharacters() {
    let owner = MetadataOwner {
        process: process(123),
        session: "weird,#{session_id}|π'".into(),
    };
    let encoded = owner.encode();
    assert!(!encoded.contains(','));
    assert!(!encoded.contains('#'));
    assert_eq!(MetadataOwner::parse(&encoded), Some(owner));
    assert!(MetadataOwner::parse("123:l:1|ff").is_none());
    assert!(MetadataOwner::parse("123:l:1|0a").is_none());
}

#[test]
fn private_decoders_bound_input_without_rejecting_maximum_valid_lineage() {
    let excessive = "9".repeat(65_536);
    assert!(CollectorLease::parse(&excessive).is_none());
    assert!(PaneOwner::parse(&excessive).is_none());
    assert!(CommitMarker::parse(&excessive).is_none());
    assert!(PersistedResult::parse(&excessive).is_none());
    assert!(NotificationClaim::parse(&excessive).is_none());
    assert!(MetadataOwner::parse(&excessive).is_none());
    let mut largest = owner();
    largest.episode = EpisodeId::new(u64::MAX);
    largest.stream_epoch = u64::MAX;
    largest.processes = ProcessChain::new(
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
    assert_eq!(PaneOwner::parse(&largest.encode()), Some(largest.clone()));
    let marker = CommitMarker {
        write: token(1),
        owner: largest.clone(),
    };
    assert_eq!(CommitMarker::parse(&marker.encode()), Some(marker));
    let result = PersistedResult {
        owner: largest.clone(),
        identity: ResultIdentity {
            owner: largest.episode,
            record: RecordId::parse(&vec!["a".repeat(32); 3].join("/")).unwrap(),
            generation: ResultGeneration::new(u64::MAX),
        },
        cycle: WorkCycle::new(u64::MAX),
        kind: ResultKind::Failure,
    };
    assert_eq!(
        PersistedResult::parse(&result.encode()),
        Some(result.clone())
    );
    let claim = NotificationClaim {
        token: token(1),
        result,
    };
    assert_eq!(NotificationClaim::parse(&claim.encode()), Some(claim));
    let metadata = MetadataOwner {
        process: largest.processes.agent(),
        session: "z".repeat(1024),
    };
    assert_eq!(MetadataOwner::parse(&metadata.encode()), Some(metadata));
}

#[test]
fn monotonic_cadence_requires_confirmed_renewal_and_does_not_refresh_report_time() {
    let mut schedule = LeaseSchedule::committed(time(100, NOW));
    assert_eq!(schedule.action(time(5_099, NOW + 4_999)), LeaseAction::Wait);
    assert_eq!(
        schedule.action(time(5_100, NOW + 5_000)),
        LeaseAction::Renew {
            expires_at: NOW + 20_000
        }
    );
    assert!(schedule.confirm(time(5_100, NOW + 5_000), time(5_100, NOW + 5_000)));
    assert_eq!(
        schedule.action(time(10_099, NOW + 9_999)),
        LeaseAction::Wait
    );
    assert_eq!(
        schedule.action(time(10_100, NOW + 10_000)),
        LeaseAction::Renew {
            expires_at: NOW + 25_000
        }
    );
}

#[test]
fn deadline_miss_and_clock_steps_revoke_terminally_before_any_renewal() {
    for (mono, wall) in [
        (15_100, NOW + 15_000),
        (5_100, NOW + 7_001),
        (5_100, NOW + 2_999),
        (200, NOW - 1),
        (99, NOW + 1),
    ] {
        let mut schedule = LeaseSchedule::committed(time(100, NOW));
        assert_eq!(schedule.action(time(mono, wall)), LeaseAction::Revoke);
        assert!(!schedule.confirm(time(5_100, NOW + 5_000), time(5_100, NOW + 5_000)));
        assert_eq!(
            schedule.action(time(5_100, NOW + 5_000)),
            LeaseAction::Revoke
        );
    }
}

#[test]
fn slow_renewal_keeps_submission_deadline_and_cannot_bridge_expiry_or_clock_steps() {
    let mut schedule = LeaseSchedule::committed(time(0, NOW));
    assert!(schedule.confirm(time(5_000, NOW + 5_000), time(14_000, NOW + 14_000)));
    assert_eq!(
        schedule.action(time(20_000, NOW + 20_000)),
        LeaseAction::Revoke
    );
    for completion in [
        time(15_000, NOW + 15_000),
        time(6_000, NOW + 3_000),
        time(4_999, NOW + 6_000),
        time(6_000, NOW + 8_001),
    ] {
        let mut schedule = LeaseSchedule::committed(time(0, NOW));
        assert!(!schedule.confirm(time(5_000, NOW + 5_000), completion));
        assert_eq!(
            schedule.action(time(6_000, NOW + 6_000)),
            LeaseAction::Revoke
        );
    }
}

#[test]
fn hook_attention_revision_is_bounded_typed_and_rejects_newer_or_malformed_truth() {
    let raw = format!(
        "$2|%3|{}|{}|{}|1|idle|hook|12|{NOW}||1",
        token(5).as_str(),
        token(6).as_str(),
        token(9).as_str()
    );
    let revision = AttentionRevision::parse(&raw).unwrap();
    assert_eq!(revision.session().get(), 2);
    assert_eq!(revision.pane().get(), 3);
    let mut truth = options();
    truth.insert(public::ATTENTION.into(), "1".into());
    truth.insert(opt::RECEIPT.into(), token(5).as_str().into());
    truth.insert(opt::ATTENTION_EPISODE.into(), token(6).as_str().into());
    assert!(revision.matches(&truth));
    for (key, changed) in [
        (opt::RECEIPT, token(7).as_str().to_owned()),
        (opt::RECEIPT, format!("{}0", token(5).as_str())),
        (opt::ATTENTION_EPISODE, token(8).as_str().to_owned()),
        (opt::ATTENTION_EPISODE, format!("{}0", token(6).as_str())),
        (public::TURN_AT, "1".into()),
        (public::SINCE, format!("{NOW}0")),
        (public::PID, "120".into()),
        (opt::COMMIT, format!("{}|malformed", token(9).as_str())),
    ] {
        let mut changed_truth = truth.clone();
        changed_truth.insert(key.into(), changed);
        assert!(!revision.matches(&changed_truth), "{key}");
    }
    for malformed in [
        "x".repeat(257),
        format!("{raw}|extra"),
        raw.replace("|12|", "|4294967296|"),
        raw.replace("|12|", "|012|"),
        raw.replace("|hook|", "|untrusted|"),
        raw.replace(token(5).as_str(), "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
        raw.replace("|1|idle|", "|x|idle|"),
    ] {
        assert!(AttentionRevision::parse(&malformed).is_none());
    }
}

#[test]
fn unmarked_legacy_attention_revision_preserves_raw_seconds_and_activity_source() {
    let revision = AttentionRevision::parse("$0|%0|||||idle|activity|0|100|200|1").unwrap();
    let truth: HashMap<_, _> = [
        (public::STATE, "idle"),
        (public::SOURCE, "activity"),
        (public::SINCE, "100"),
        (public::TURN_AT, "200"),
        (public::ATTENTION, "1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    assert!(revision.matches(&truth));
    assert!(AttentionRevision::parse("$0|%0|||||idle|activity|0|100|200|").is_none());
}
