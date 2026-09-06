//! A-113's property leg: the round trip holds for values nobody wrote a vector for.
//!
//! The corpus pins the wire; this pins the codec. It matters most for the two places the crate does
//! not simply derive: the `Other` arms of the open vocabularies, and the `""` encoding an edge uses
//! for the open ends of a pane's life.

use proptest::prelude::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::*;

/// The strings a synthesized value may hold. Short and boring: this leg is about the codec, and a
/// generator that spends its budget on interesting Unicode is testing serde_json instead.
fn text() -> impl Strategy<Value = String> {
    "[a-z0-9 %:/._-]{0,24}"
}

/// A token no build knows. Every vocabulary in this crate starts its known tokens with a letter
/// other than `z`, so a generated `Other` cannot collide with a real variant and round-trip into it.
fn unknown_token() -> impl Strategy<Value = String> {
    "z[a-z_-]{0,12}"
}

fn state() -> impl Strategy<Value = State> {
    (0..State::ALL.len()).prop_map(|i| State::ALL[i])
}

fn detail() -> impl Strategy<Value = Detail> {
    prop_oneof![
        (0..Detail::TOKENS.len()).prop_map(|i| Detail::from_token(Detail::TOKENS[i])),
        unknown_token().prop_map(Detail::Other),
    ]
}

fn outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        (0..Outcome::TOKENS.len()).prop_map(|i| Outcome::from_token(Outcome::TOKENS[i])),
        unknown_token().prop_map(Outcome::Other),
    ]
}

fn reason() -> impl Strategy<Value = Reason> {
    prop_oneof![
        (0..Reason::TOKENS.len()).prop_map(|i| Reason::from_token(Reason::TOKENS[i])),
        unknown_token().prop_map(Reason::Other),
    ]
}

fn option_kind() -> impl Strategy<Value = OptionKind> {
    prop_oneof![
        (0..OptionKind::TOKENS.len()).prop_map(|i| OptionKind::from_token(OptionKind::TOKENS[i])),
        unknown_token().prop_map(OptionKind::Other),
    ]
}

prop_compose! {
    fn quota()(pct in 0u8..=100, window in text(), resets_at_ms in proptest::option::of(any::<u64>()))
        -> Quota {
        Quota { pct, window, resets_at_ms }
    }
}

prop_compose! {
    #[allow(clippy::too_many_arguments)]
    fn fleet_row()(
        pane in text(),
        agent in text(),
        state in state(),
        detail in proptest::option::of(detail()),
        since in any::<u64>(),
        episode_ms in any::<u64>(),
        locator in text(),
        attention in any::<bool>(),
        done in any::<bool>(),
        session in proptest::option::of(text()),
        stamped_at_ms in proptest::option::of(any::<u64>()),
        context in proptest::option::of(any::<u8>()),
        muted in any::<bool>(),
        tokens in proptest::option::of(any::<u64>()),
        quota in proptest::option::of(quota()),
        cost_usd in proptest::option::of(0.0f64..100_000.0),
        repo in proptest::option::of(text()),
        worktree in proptest::option::of(any::<bool>()),
        pending_summary in proptest::option::of(text()),
    ) -> FleetRow {
        FleetRow {
            pane, agent, state, detail,
            since, since_ms: since.saturating_mul(1000), episode_ms,
            locator, attention, done, session,
            transcript: None,
            permission_request: None,
            stamped_at_ms, context, context_at_ms: stamped_at_ms, muted, tokens, quota, cost_usd,
            branch: repo.clone(), repo, worktree,
            pending_tool: None,
            pending_call: None,
            pending_summary,
            server: String::new(),
            host: String::new(),
        }
    }
}

prop_compose! {
    fn edge()(
        at_ms in any::<u64>(),
        pane in text(),
        agent in text(),
        from in proptest::option::of(state()),
        to in proptest::option::of(state()),
        detail in proptest::option::of(detail()),
        locator in text(),
        repo in proptest::option::of(text()),
    ) -> Edge {
        Edge { at_ms, pane, agent, from, to, detail, locator, branch: repo.clone(), repo }
    }
}

prop_compose! {
    fn receipt()(
        slot in text(),
        pane in text(),
        action in text(),
        outcome in outcome(),
        reason in proptest::option::of(reason()),
        exit_code in any::<i32>(),
        cached in any::<bool>(),
        device in proptest::option::of(text()),
        at_ms in any::<u64>(),
    ) -> Receipt {
        Receipt { slot, pane, action, outcome, reason, exit_code, cached, device, at_ms }
    }
}

prop_compose! {
    fn permission_option()(
        kind in option_kind(),
        name in text(),
        option_id in proptest::option::of(text()),
    ) -> PermissionOption {
        PermissionOption { kind, name, option_id }
    }
}

fn card() -> impl Strategy<Value = Card> {
    prop_oneof![
        (
            text(),
            text(),
            (0..Lane::ALL.len()),
            proptest::collection::vec(permission_option(), 0..5),
            (0..Extraction::ALL.len()),
            any::<u64>(),
            proptest::option::of(text()),
        )
            .prop_map(
                |(pane, agent, lane, options, extraction, episode, request)| Card::Permission(
                    PermissionCard {
                        pane,
                        agent,
                        lane: Lane::ALL[lane],
                        options,
                        extraction: Extraction::ALL[extraction],
                        pending_call: None,
                        binder: Binder {
                            expect_episode_ms: episode,
                            expect_permission_request: request,
                        },
                    }
                )
            ),
        (detail(), text()).prop_map(|(detail, headline)| Card::Informational { detail, headline }),
        Just(Card::None),
    ]
}

/// Serialize, parse, serialize, parse: both the bytes and the value have to be stable.
///
/// Stated as a fixed point rather than as "the input comes back", because one field normalizes on
/// the way out: a cost is rounded to the two decimals the host publishes ([`crate::money`]). Every
/// other field is carried unchanged, and `money_is_rounded_to_the_hosts_two_decimals` pins the one
/// exception so this weaker form cannot quietly cover a second one.
fn survives<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let first = encode(value).expect("serialize");
    let parsed: T = decode(&first).expect("parse what we just wrote");
    let second = encode(&parsed).expect("re-serialize");
    let reparsed: T = decode(&second).expect("re-parse");
    assert_eq!(second, first, "the bytes changed on re-emission");
    assert_eq!(reparsed, parsed, "the value changed on re-parse");
}

/// The one normalizing field, pinned. A cost carries the two decimals the host publishes, so a
/// 17-significant-digit amount cannot round-trip into a different number through `serde_json`'s
/// float parser, which is within 1 ULP rather than exact.
#[test]
fn money_is_rounded_to_the_hosts_two_decimals() {
    let cost = |v: f64| {
        let event = EventKind::Usage {
            input: None,
            output: None,
            total: None,
            context_window: None,
            cost_usd: Some(v),
        };
        let line = encode(&event).expect("serialize");
        let back: EventKind = decode(&line).expect("parse");
        let EventKind::Usage { cost_usd, .. } = back else {
            panic!("expected usage");
        };
        (line, cost_usd.expect("a cost"))
    };

    let (line, value) = cost(506.416_746_353_498_35);
    assert!(line.contains("506.42"), "{line}");
    assert_eq!(value, 506.42);
    // And an amount already at two decimals is untouched.
    assert_eq!(cost(3.5).1, 3.5);
}

/// A transcript event is the one frame with two flattens stacked on it, the envelope's and the
/// header's, so its values deserialize through serde's buffered representation rather than straight
/// off the parser. A count above `i64::MAX` is where that buffering would show.
#[test]
fn a_doubly_flattened_event_keeps_its_large_counts() {
    let frame = ResponseFrame::new(
        "8",
        Response::Event(EventHeader {
            cursor: Cursor("t1.1.1.1.1.0".to_string()),
            kind: EventKind::AssistantText { bytes: u64::MAX },
            ts: None,
            preview: None,
            body: None,
        }),
    );
    survives(&frame);
    let line = encode(&frame).expect("serialize");
    assert!(line.contains("18446744073709551615"), "{line}");
}

proptest! {
    #[test]
    fn a_generated_row_survives_a_round_trip(row in fleet_row()) {
        survives(&row);
    }

    #[test]
    fn a_generated_edge_survives_a_round_trip(edge in edge()) {
        survives(&edge);
    }

    #[test]
    fn a_generated_receipt_survives_a_round_trip(receipt in receipt()) {
        survives(&receipt);
    }

    #[test]
    fn a_generated_card_survives_a_round_trip(card in card()) {
        survives(&card);
    }

    /// And inside the envelope, where the flattened tag has to survive too.
    #[test]
    fn a_generated_frame_survives_a_round_trip(id in text(), receipt in receipt()) {
        survives(&ResponseFrame::new(id, Response::Receipt(receipt)));
    }
}
