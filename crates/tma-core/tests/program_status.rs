//! OSC 7501 revision 0.4 cases, independent of the manifest Claim grammar.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use proptest::prelude::*;
use tma_core::program_status::*;

fn frame(body: &str) -> Vec<u8> {
    [b"\x1b]7501;".as_slice(), body.as_bytes(), b"\x1b\\"].concat()
}

fn parse(body: &str) -> Result<Option<ProgramCommand>, RejectReason> {
    let input = frame(body);
    let mut scratch = DecodeScratch::default();
    parse_frame(&input, &mut scratch).map(|outcome| match outcome {
        ParseOutcome::Command(command) => Some(command.into_owned()),
        ParseOutcome::Ignored(_) => None,
    })
}

fn record(body: &str) -> Record {
    match parse(body).unwrap().unwrap() {
        ProgramCommand::Replace(record) => record,
        other => panic!("expected replacement, got {other:?}"),
    }
}

fn encoded(bytes: &[u8]) -> String {
    let mut output = vec![0; base64::encoded_len(bytes.len(), true).unwrap()];
    let len = STANDARD.encode_slice(bytes, &mut output).unwrap();
    output.truncate(len);
    String::from_utf8(output).unwrap()
}

#[test]
fn native_pi_fixture_retains_typed_transitions_and_safe_text() {
    let commands: Vec<_> = include_str!("../fixtures/program_status/pi-1.3.0.frames")
        .lines()
        .map(|body| parse(body).unwrap().unwrap())
        .collect();
    assert_eq!(commands.len(), 8);
    let states: Vec<_> = commands
        .iter()
        .filter_map(|command| match command {
            ProgramCommand::Replace(record) => Some(record.status),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        [
            ProgramStatus::Idle,
            ProgramStatus::Working { progress: None },
            ProgramStatus::Done,
            ProgramStatus::Blocked {
                kind: Some(BlockedKind::Permission),
                progress: None
            },
            ProgramStatus::Done,
            ProgramStatus::Working { progress: None },
            ProgramStatus::Error,
        ]
    );
    assert_eq!(
        commands.last(),
        Some(&ProgramCommand::Clear(RecordId::root()))
    );
    let ProgramCommand::Replace(blocked) = &commands[3] else {
        unreachable!()
    };
    assert_eq!(
        blocked.msg.as_ref().unwrap().as_ref().as_str(),
        "OSC 7501 prototype"
    );
}

#[test]
fn clear_and_query_are_commands_not_states() {
    assert_eq!(parse("?"), Ok(Some(ProgramCommand::Query)));
    assert_eq!(
        parse("state=clear"),
        Ok(Some(ProgramCommand::Clear(RecordId::root())))
    );
    assert_eq!(
        parse("state=clear:id=build/test"),
        Ok(Some(ProgramCommand::Clear(
            RecordId::parse("build/test").unwrap()
        )))
    );
    assert_eq!(parse("?:states=idle,working"), Ok(None));
}

#[test]
fn framing_accepts_bel_and_st_but_requires_exact_complete_frame() {
    let mut scratch = DecodeScratch::default();
    for input in [
        b"\x1b]7501;state=idle\x07".as_slice(),
        b"\x1b]7501;state=idle\x1b\\",
    ] {
        assert!(matches!(
            parse_frame(input, &mut scratch),
            Ok(ParseOutcome::Command(_))
        ));
    }
    for input in [
        b"state=idle".as_slice(),
        b"\x1b]7501;state=idle",
        b"\x1b]7501;state=idle\x07extra",
        b"\x1b]7501;state=idle\x07:state=done\x07",
    ] {
        assert_eq!(
            parse_frame(input, &mut scratch),
            Err(RejectReason::InvalidFrame)
        );
    }
}

#[test]
fn missing_and_unknown_states_ignore_entire_report() {
    let mut scratch = DecodeScratch::default();
    assert_eq!(
        parse_frame(&frame("app=pi"), &mut scratch),
        Ok(ParseOutcome::Ignored(IgnoreReason::MissingState))
    );
    assert_eq!(
        parse_frame(&frame("state=future"), &mut scratch),
        Ok(ParseOutcome::Ignored(IgnoreReason::UnknownState))
    );
    assert_eq!(parse("state=working:state=future"), Ok(None));
}

#[test]
fn malformed_pairs_skip_unknown_keys_ignore_and_last_duplicates_win() {
    let parsed = record(" state = blocked :bad:=empty:UPPER=x:oops=a!b:future=abc:kind=auth:kind=question:progress=10:progress=77:app=pi:app=other:msg=QQ==:msg=Qg:title=QQ==:title=Qw==");
    assert_eq!(
        parsed.status,
        ProgramStatus::Blocked {
            kind: Some(BlockedKind::Question),
            progress: Some(Progress::new(77).unwrap())
        }
    );
    assert_eq!(parsed.app.as_ref().unwrap().as_ref().as_str(), "other");
    assert_eq!(parsed.msg.as_ref().unwrap().as_ref().as_str(), "B");
    assert_eq!(parsed.title().unwrap().as_str(), "C");
    assert_eq!(
        record("state=working:state=bad!token").status,
        ProgramStatus::Working { progress: None }
    );
    assert_eq!(
        record("state=done:id=old:id=new").id,
        RecordId::parse("new").unwrap()
    );
}

#[test]
fn optional_fields_are_absent_when_invalid_and_ignored_for_other_states() {
    for progress in ["", "-1", "+1", "1.0", "101", "256", "9999999999999999999"] {
        assert_eq!(
            record(&format!("state=working:progress={progress}")).status,
            ProgramStatus::Working { progress: None }
        );
    }
    for (input, expected) in [("0", 0), ("100", 100), ("0000001", 1)] {
        assert_eq!(
            record(&format!("state=working:progress={input}")).status,
            ProgramStatus::Working {
                progress: Some(Progress::new(expected).unwrap())
            }
        );
    }
    assert_eq!(
        record("state=blocked:kind=future:kind=unknown:progress=999").status,
        ProgramStatus::Blocked {
            kind: None,
            progress: None
        }
    );
    assert_eq!(
        record("state=done:kind=permission:progress=45").status,
        ProgramStatus::Done
    );
    for app in ["", "bad/name", "bad,name", "bad=value"] {
        assert!(record(&format!("state=idle:app={app}")).app.is_none());
    }
    assert!(record("state=idle:app=ok:app=bad/name").app.is_none());
}

#[test]
fn strict_id_grammar_never_falls_back_to_root() {
    for id in [
        "",
        "/",
        "a/",
        "/a",
        "a//b",
        "bad,value",
        "bad=value",
        "bad!value",
        "bad name",
        "é",
    ] {
        let mut scratch = DecodeScratch::default();
        for state in ["idle", "clear"] {
            assert_eq!(
                parse_frame(&frame(&format!("state={state}:id={id}")), &mut scratch),
                Ok(ParseOutcome::Ignored(IgnoreReason::InvalidId)),
                "{id}"
            );
        }
    }
    // Duplicate identity uses the final value; hard limits remain atomic violations.
    assert_eq!(
        record("state=idle:id=bad//id:id=valid").id,
        RecordId::parse("valid").unwrap()
    );
    assert_eq!(parse("state=idle:id=valid:id="), Ok(None));
}

#[test]
fn id_ancestry_obeys_segment_boundaries() {
    let root = RecordIdRef::ROOT;
    let parent = RecordIdRef::parse("build").unwrap();
    let child = RecordIdRef::parse("build/test").unwrap();
    assert_eq!(child.parent(), Some(parent));
    assert_eq!(parent.parent(), Some(root));
    assert_eq!(root.parent(), None);
    assert!(parent.contains(child));
    assert!(parent.contains(parent));
    assert!(root.contains(child));
    assert!(!parent.contains(RecordIdRef::parse("builder").unwrap()));
    assert!(!child.contains(parent));
}

#[test]
fn identifier_limits_accept_boundaries_and_reject_every_overrun() {
    let segment = "x".repeat(32);
    assert!(record(&format!("state=idle:id={segment}:app={segment}"))
        .app
        .is_some());
    let id128 = format!("{segment}/{segment}/{segment}/{}", "x".repeat(29));
    assert_eq!(id128.len(), 128);
    assert_eq!(
        record(&format!("state=idle:id={id128}"))
            .id
            .as_ref()
            .as_str(),
        id128
    );
    assert!(parse("state=idle:id=a/b/c/d/e/f/g/h").unwrap().is_some());
    for (body, error) in [
        (
            format!("state=idle:app={}x:app=pi", segment),
            RejectReason::AppTooLong,
        ),
        (
            format!("state=idle:id={id128}x:id=ok"),
            RejectReason::IdTooLong,
        ),
        (
            format!("state=idle:id={}x:id=ok", segment),
            RejectReason::IdSegmentTooLong,
        ),
        (
            "state=idle:id=a/b/c/d/e/f/g/h/i:id=ok".into(),
            RejectReason::IdTooDeep,
        ),
        (
            "state=idle:abcdefghijklmnopq=x".into(),
            RejectReason::KeyTooLong,
        ),
    ] {
        assert_eq!(parse(&body), Err(error), "{body}");
    }
    assert!(parse("state=idle:abcdefghijklmnop=x").unwrap().is_some());
}

#[test]
fn whole_frame_limit_includes_introducer_and_terminator() {
    let base = "state=idle:extra=";
    let body = format!("{base}{}", "x".repeat(MAX_FRAME_BYTES - 9 - base.len()));
    assert_eq!(frame(&body).len(), MAX_FRAME_BYTES);
    assert!(parse(&body).unwrap().is_some());
    assert_eq!(
        parse(&(body.clone() + "x")),
        Err(RejectReason::FrameTooLong)
    );
    let mut bel = frame(&(body + "x"));
    bel.truncate(bel.len() - 2);
    bel.push(7);
    assert_eq!(bel.len(), MAX_FRAME_BYTES);
    assert!(parse_frame(&bel, &mut DecodeScratch::default()).is_ok());
}

#[test]
fn text_limits_measure_decoded_utf8_bytes_not_characters() {
    for (key, max, encoded_max, field) in [
        (
            "msg",
            MAX_MSG_BYTES,
            MAX_MSG_ENCODED_BYTES,
            TextField::Message,
        ),
        (
            "title",
            MAX_TITLE_BYTES,
            MAX_TITLE_ENCODED_BYTES,
            TextField::Title,
        ),
    ] {
        let text = "é".repeat(max / 2);
        let body = format!("state=idle:{key}={}", encoded(text.as_bytes()));
        let parsed = record(&body);
        let value = if key == "msg" {
            parsed.msg.as_ref().map(SingleLineText::as_ref)
        } else {
            parsed.title()
        };
        assert_eq!(value.unwrap().as_str(), text);
        let oversized = encoded(&vec![b'x'; max + 1]);
        let expected = if oversized.len() > encoded_max {
            RejectReason::EncodedTextTooLong(field)
        } else {
            RejectReason::DecodedTextTooLong(field)
        };
        assert_eq!(
            parse(&format!("state=idle:{key}={oversized}")),
            Err(expected)
        );
        assert_eq!(
            parse(&format!("state=idle:{key}={}", "A".repeat(encoded_max + 1))),
            Err(RejectReason::EncodedTextTooLong(field))
        );
    }
}

#[test]
fn base64_padding_utf8_and_control_validation() {
    for input in ["Zg", "Zg=="] {
        assert_eq!(
            record(&format!("state=idle:msg={input}"))
                .msg
                .unwrap()
                .as_ref()
                .as_str(),
            "f"
        );
    }
    assert_eq!(
        record("state=idle:msg=:title=")
            .msg
            .unwrap()
            .as_ref()
            .as_str(),
        ""
    );
    for bad in ["A", "====", "Zg===", "Zg=A", "_w==", "Zh=="] {
        assert_eq!(
            parse(&format!("state=idle:msg={bad}")),
            Err(RejectReason::InvalidBase64(TextField::Message)),
            "{bad}"
        );
    }
    assert_eq!(
        parse("state=idle:msg=/w=="),
        Err(RejectReason::InvalidUtf8(TextField::Message))
    );
    for control in (0..=0x1f).chain(0x7f..=0x9f) {
        let text = char::from_u32(control).unwrap().to_string();
        for key in ["msg", "title"] {
            let field = if key == "msg" {
                TextField::Message
            } else {
                TextField::Title
            };
            assert_eq!(
                parse(&format!("state=idle:{key}={}", encoded(text.as_bytes()))),
                Err(RejectReason::ControlText(field))
            );
        }
    }
}

#[test]
fn base64_padding_is_either_canonical_or_absent() {
    for (input, expected) in [("Zg", "f"), ("Zg==", "f"), ("Zm8", "fo"), ("Zm8=", "fo")] {
        let parsed = record(&format!("state=idle:msg={input}:title={input}"));
        assert_eq!(parsed.msg.as_ref().unwrap().as_ref().as_str(), expected);
        assert_eq!(parsed.title().unwrap().as_str(), expected);
    }
    for (key, field) in [("msg", TextField::Message), ("title", TextField::Title)] {
        for state in ["idle", "clear", "future"] {
            assert_eq!(
                parse(&format!("state={state}:{key}=Zg=:{key}=QQ==")),
                Err(RejectReason::InvalidBase64(field))
            );
        }
    }
}

#[test]
fn public_record_construction_enforces_title_bound_before_retention() {
    let boundary = "x".repeat(MAX_TITLE_BYTES);
    let title = SingleLineRef::parse(&boundary).unwrap();
    let base = RecordRef::new(RecordIdRef::ROOT, ProgramStatus::Idle);
    let retained = base.with_title(Some(title)).unwrap().into_owned();
    assert_eq!(retained.title(), Some(title));
    assert_eq!(retained.as_ref().title(), Some(title));
    assert_eq!(base.with_title(None).unwrap().title(), None);

    for len in [MAX_TITLE_BYTES + 1, MAX_MSG_BYTES] {
        let text = "x".repeat(len);
        let message = SingleLineRef::parse(&text).unwrap();
        assert_eq!(
            base.with_title(Some(message)),
            Err(ValueError::TitleTooLong)
        );
        let mut message_record = base;
        message_record.msg = Some(message);
        assert_eq!(message_record.into_owned().msg.unwrap().as_ref(), message);
    }
}

#[test]
fn invalid_overwritten_text_rejects_even_clear_and_unknown_states() {
    for state in ["idle", "clear", "future"] {
        for bad in ["A", "Cg==", "/w=="] {
            assert!(parse(&format!("state={state}:msg={bad}:msg=QQ==")).is_err());
            assert!(parse(&format!("state={state}:title={bad}:title=QQ==")).is_err());
        }
        assert!(parse(&format!(
            "state={state}:msg={}:msg=QQ==",
            "A".repeat(MAX_MSG_ENCODED_BYTES + 1)
        ))
        .is_err());
    }
}

#[test]
fn parser_borrows_tokens_and_scratch_before_explicit_retention() {
    assert!(!std::mem::needs_drop::<ParseOutcome<'_>>());
    assert!(!std::mem::needs_drop::<RejectReason>());
    assert_eq!(
        std::mem::size_of::<DecodeScratch>(),
        MAX_MSG_BYTES + 1 + MAX_TITLE_BYTES
    );
    let input = frame("state=working:id=build/test:app=pi:msg=SGVsbG8=:title=V29ybGQ=");
    let mut scratch = DecodeScratch::default();
    let scratch_start = std::ptr::from_ref(&scratch).addr();
    let scratch_end = scratch_start + std::mem::size_of::<DecodeScratch>();
    let ParseOutcome::Command(ProgramCommandRef::Replace(parsed)) =
        parse_frame(&input, &mut scratch).unwrap()
    else {
        panic!("replacement")
    };
    for token in [parsed.id.as_str(), parsed.app.unwrap().as_str()] {
        assert!(token.as_ptr().addr() >= input.as_ptr().addr());
        assert!(token.as_ptr().addr() + token.len() <= input.as_ptr().addr() + input.len());
    }
    for text in [parsed.msg.unwrap(), parsed.title().unwrap()] {
        assert!((scratch_start..scratch_end).contains(&text.as_str().as_ptr().addr()));
    }
    let retained = parsed.into_owned();
    let _ = parse_frame(&frame("state=idle:msg=TmV3"), &mut scratch).unwrap();
    assert_eq!(retained.msg.unwrap().as_ref().as_str(), "Hello");
}

#[test]
fn rejected_input_cannot_mutate_retained_records_or_eviction_order() {
    let mut retained = vec![
        (record("state=idle:id=a:app=pi"), 1_u64),
        (record("state=working:id=b"), 2),
    ];
    let before = retained.clone();
    let mut ordinal = 2;
    let mut scratch = DecodeScratch::default();
    for body in [
        "state=clear:id=a:msg=Cg==",
        "state=done:id=b:title=A:title=QQ==",
        "state=done:id=",
        "state=future:id=a",
    ] {
        if let Ok(ParseOutcome::Command(command)) = parse_frame(&frame(body), &mut scratch) {
            match command {
                ProgramCommandRef::Replace(parsed) => {
                    ordinal += 1;
                    retained.retain(|(record, _)| record.id.as_ref() != parsed.id);
                    retained.push((parsed.into_owned(), ordinal));
                }
                ProgramCommandRef::Clear(id) => {
                    retained.retain(|(record, _)| !id.contains(record.id.as_ref()))
                }
                ProgramCommandRef::Query => {}
            }
        }
        assert_eq!(retained, before);
        assert_eq!(ordinal, 2);
    }
}

#[test]
fn numeric_tmux_ids_validate_at_the_boundary() {
    assert_eq!(PaneId::parse("%42"), Ok(PaneId::new(42)));
    assert_eq!(SessionId::parse("$7"), Ok(SessionId::new(7)));
    for value in ["42", "%", "%+1", "%-1", "%1x", "%4294967296", "$1"] {
        assert_eq!(PaneId::parse(value), Err(ValueError::InvalidPaneId));
    }
    assert_eq!(ConnectionEpoch::new(7).get(), 7);
    assert_eq!(ObservationEpoch::new(8).get(), 8);
    assert_eq!(ObservationSequence::new(9).get(), 9);
}

#[test]
fn manifest_claim_grammar_does_not_gain_protocol_states() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        claim: tma_core::Claim,
    }
    assert!(matches!(
        toml::from_str::<Wrapper>("claim = { state = \"working\" }")
            .unwrap()
            .claim,
        tma_core::Claim::State(_)
    ));
    for value in ["done", "error", "clear", "query", "working:progress=5"] {
        assert!(
            toml::from_str::<Wrapper>(&format!("claim = {{ state = {value:?} }}")).is_err(),
            "{value}"
        );
    }
    for body in [
        "state = \"working\", progress = 5",
        "state = \"blocked\", kind = \"auth\"",
        "state = \"working\", lifecycle = \"start\"",
        "",
    ] {
        assert!(toml::from_str::<Wrapper>(&format!("claim = {{ {body} }}")).is_err());
    }
}

proptest! {
    #[test]
    fn arbitrary_bytes_do_not_escape_validated_command_boundary(body in prop::collection::vec(any::<u8>(), 0..5000)) {
        let input = [b"\x1b]7501;".as_slice(), &body, b"\x1b\\"].concat();
        let mut scratch = DecodeScratch::default();
        if let Ok(ParseOutcome::Command(ProgramCommandRef::Replace(parsed))) = parse_frame(&input, &mut scratch) {
            prop_assert!(input.len() <= MAX_FRAME_BYTES);
            prop_assert!(parsed.id.is_root() || RecordIdRef::parse(parsed.id.as_str()).is_ok());
            for text in [parsed.msg, parsed.title()].into_iter().flatten() {
                prop_assert!(SingleLineRef::parse(text.as_str()).is_ok());
            }
        }
    }

    #[test]
    fn control_duplicate_cannot_commit_after_valid_replacement(id in "[a-z]{1,16}", control in 0_u8..32) {
        let body = format!("state=done:id={id}:msg={}:msg=QQ==", encoded(&[control]));
        prop_assert_eq!(parse(&body), Err(RejectReason::ControlText(TextField::Message)));
    }
}
