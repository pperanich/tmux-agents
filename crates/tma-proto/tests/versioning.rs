//! The four rules that let a device and a host of different ages talk to each other.
//!
//! Their inputs are written inline rather than committed as vectors, because three of the four are
//! about a frame whose re-emission is deliberately *not* byte-identical to its input, which is the
//! one thing a golden vector cannot express.

use tma_proto::*;

/// An unknown enum variant round-trips preserved, not dropped and not normalized.
///
/// The counterpart of unknown-field dropping, and the opposite answer: dropping a field loses detail, dropping a
/// variant changes meaning. An older device meeting a newer host must degrade to "I cannot type
/// this dialog", never to "this must be a permission prompt".
#[test]
fn an_unknown_variant_survives_a_round_trip() {
    let line = r#"{"schema":1,"id":"1","t":"card","card":"informational","detail":"some-future-detail","headline":"xxxx"}"#;

    let frame: ResponseFrame = decode(line).expect("a future detail token still parses");
    let Response::Card(Card::Informational { detail, .. }) = &frame.body else {
        panic!("expected an informational card");
    };
    assert_eq!(*detail, Detail::Other("some-future-detail".to_string()));
    assert!(!detail.is_known());
    assert_eq!(encode(&frame).expect("re-emit"), line);
}

/// The same rule on the vocabularies a receipt carries, where guessing is worst.
#[test]
fn unknown_outcome_and_reason_tokens_survive_too() {
    for (token, outcome) in [
        ("sent", Outcome::Sent),
        ("swallowed", Outcome::Other("swallowed".to_string())),
    ] {
        assert_eq!(Outcome::from_token(token), outcome);
        assert_eq!(outcome.token(), token);
    }
    let reason = Reason::from_token("some-future-refusal");
    assert!(!reason.is_known());
    assert_eq!(reason.token(), "some-future-refusal");
}

/// An unknown field parses, is ignored, and is omitted on re-emission.
#[test]
fn an_unknown_field_is_ignored_and_not_re_emitted() {
    let line = r#"{"schema":1,"id":"1","t":"card","card":"none","headline_v2":"xxxx"}"#;

    let frame: ResponseFrame = decode(line).expect("an unknown field does not fail the parse");
    let out = encode(&frame).expect("re-emit");
    assert!(
        !out.contains("headline_v2"),
        "the unknown field was carried through: {out}"
    );
    assert_eq!(out, r#"{"schema":1,"id":"1","t":"card","card":"none"}"#);
}

/// A hello naming a schema this build does not implement earns a typed error response, not a
/// parse failure and not a silent downgrade.
#[test]
fn a_future_schema_earns_a_typed_error_not_a_parse_failure() {
    let line = r#"{"schema":2,"id":"1","t":"hello","app":"xxxxxxxx","app_version":"9.9.9","device":"xxxxxxxxxxxxxxxx"}"#;

    let frame: RequestFrame = decode(line).expect("the version claim is a field, not a shape");
    assert_eq!(frame.schema, 2);

    let refusal = frame
        .accept_hello()
        .expect_err("schema 2 is not implemented here");
    assert_eq!(refusal.code, ErrorCode::UnsupportedSchema);
    assert!(
        refusal.message.contains("2"),
        "the refusal says which schema was asked for: {}",
        refusal.message
    );

    // And the frame the host would write back is an ordinary response, so a device that cannot
    // speak this schema can still read why.
    let response = encode(&ResponseFrame::new("1", Response::Error(refusal))).expect("re-emit");
    assert!(response.starts_with(r#"{"schema":1,"id":"1","t":"error","code":"unsupported-schema""#));
}

/// The schema this build does implement is accepted, so the refusal above is about the version and
/// not about the frame.
#[test]
fn the_current_schema_is_accepted() {
    let line = r#"{"schema":1,"id":"1","t":"hello","app":"xxxxxxxx","app_version":"0.1.0","device":"xxxxxxxxxxxxxxxx"}"#;
    let frame: RequestFrame = decode(line).expect("parse");
    assert_eq!(frame.accept_hello().expect("accepted").app_version, "0.1.0");
}

/// A frame that is not a hello cannot open a session, and says so in the same typed shape.
#[test]
fn a_session_that_does_not_open_with_hello_is_refused() {
    let line = r#"{"schema":1,"id":"1","t":"snapshot"}"#;
    let frame: RequestFrame = decode(line).expect("parse");
    assert_eq!(
        frame.accept_hello().expect_err("not a hello").code,
        ErrorCode::BadRequest
    );
}

/// A frame from an older writer, with an additive field absent, parses on the newer reader
/// with that field defaulted.
#[test]
fn an_older_writers_frame_parses_with_the_new_field_defaulted() {
    // `device` postdates the dispatch frame: an older writer simply does not emit it.
    let older = r#"{"schema":1,"id":"5","t":"dispatch","slot":"xxxx-0001","host":"xxxxxxxx","pane":"%1","action":"approve","binder":{"expect_episode_ms":1757030400000}}"#;
    let frame: RequestFrame = decode(older).expect("parse");
    let Request::Dispatch(dispatch) = &frame.body else {
        panic!("expected a dispatch");
    };
    assert_eq!(dispatch.device, None);
    assert_eq!(dispatch.binder.expect_permission_request, None);
    // Skipped rather than nulled on the way back out, so an older reader sees what it wrote.
    assert_eq!(encode(&frame).expect("re-emit"), older);

    // A budget the older writer had no concept of arrives as this build's defaults rather than as
    // zeroes, which would be a request for nothing at all.
    let older = r#"{"schema":1,"id":"7","t":"window","pane":"%1","last":200}"#;
    let frame: RequestFrame = decode(older).expect("parse");
    let Request::Window(window) = &frame.body else {
        panic!("expected a window request");
    };
    assert_eq!(window.budget, Budget::default());
    assert_eq!(window.budget.header_bytes, 256);

    // And a row from before a key existed reads as that key's default rather than failing.
    let older = r#"{"pane":"%1","agent":"claude","state":"blocked","locator":"xxxx:1.0","server":"xxxxxxxx","host":"xxxxxxxx"}"#;
    let row: FleetRow = decode(older).expect("parse");
    assert_eq!(row.detail, None);
    assert_eq!(row.episode_ms, 0);
    assert!(!row.done);
}

/// A closed vocabulary refuses what it does not know, which is the other half of rule 2: `State` is
/// frozen, so an unrecognized state token is a bug on the wire and not a future value.
#[test]
fn a_closed_vocabulary_refuses_an_unknown_token() {
    assert_eq!(State::from_token("running"), None);
    let refused = decode::<FleetRow>(
        r#"{"pane":"%1","agent":"claude","state":"running","locator":"x","server":"x","host":"x"}"#,
    );
    assert!(refused.is_err(), "an invented state must not parse");
}

/// The receipt's terminality is derived from its reason rather than carried beside it, so the two
/// cannot disagree. `locked` is the one refusal that leaves the slot open for a retry.
#[test]
fn only_a_locked_refusal_leaves_the_slot_open() {
    let receipt = |reason| Receipt {
        slot: "xxxx-0001".to_string(),
        pane: "%1".to_string(),
        action: "approve".to_string(),
        outcome: Outcome::Refused,
        reason,
        exit_code: 4,
        cached: false,
        device: None,
        at_ms: 0,
    };
    assert!(!receipt(Some(Reason::Locked)).terminal());
    assert!(receipt(Some(Reason::Gated)).terminal());
    assert!(receipt(None).terminal());
}
