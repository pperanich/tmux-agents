//! Parse round-trips and gate assertions for the bundled action manifests: the compiled-in `keys`
//! actions (`approve`, `deny`, `interrupt`, `compact`) and `text` actions (`steer`, `steer_now`).
//! The loader that embeds them lives in `tma-runtime`, so here we assert each parses under its own
//! stem and gates as ACTIONS.md pins.

use tma_core::action::{ActionKind, ContextKeys, GateInput, GateOutcome, RefusalReason};
use tma_core::{ActionManifest, AgentState, TextRefusal};

const APPROVE: &str = include_str!("../actions/approve.toml");
const DENY: &str = include_str!("../actions/deny.toml");
const INTERRUPT: &str = include_str!("../actions/interrupt.toml");
const COMPACT: &str = include_str!("../actions/compact.toml");
const STEER: &str = include_str!("../actions/steer.toml");
const STEER_NOW: &str = include_str!("../actions/steer_now.toml");

fn row<'a>(agent: &'a str, state: AgentState) -> GateInput<'a> {
    GateInput {
        agent,
        state,
        detail: None,
        context_pct: None,
        context_covered: false,
        context_keys: ContextKeys::default(),
    }
}

#[test]
fn every_bundled_action_parses_under_its_stem() {
    for (stem, src) in [
        ("approve", APPROVE),
        ("deny", DENY),
        ("interrupt", INTERRUPT),
        ("compact", COMPACT),
    ] {
        let a = ActionManifest::parse(src, stem, &format!("{stem}.toml"))
            .unwrap_or_else(|e| panic!("{stem} must parse: {e}"));
        assert_eq!(a.name, stem);
        assert_eq!(a.kind, ActionKind::Keys);
        assert!(!a.keys.is_empty(), "{stem} is a keys action");
    }
}

#[test]
fn the_bundled_text_actions_parse_and_wrap_with_enter() {
    for (stem, src) in [("steer", STEER), ("steer_now", STEER_NOW)] {
        let a = ActionManifest::parse(src, stem, &format!("{stem}.toml"))
            .unwrap_or_else(|e| panic!("{stem} must parse: {e}"));
        assert_eq!(a.kind, ActionKind::Text);
        assert!(!a.confirm, "{stem} is not a second-factor action");
        assert_eq!(a.sigils, ['/', '!'], "{stem} sigils");
        for (agent, t) in &a.text {
            assert!(t.prefix.is_empty(), "{stem}/{agent} needs no prefix");
            assert_eq!(t.suffix, ["Enter"], "{stem}/{agent} submits with Enter");
        }
        // The command plane the sigils exist to keep out of reach.
        let claude = a.check_text("/compact");
        assert_eq!(
            claude,
            Err(TextRefusal::Sigil),
            "{stem} must refuse /compact"
        );
    }
}

/// `steer` is idle-only, and `steer_now` is the working-pane action, offered only to the two agents
/// that declared they queue a message typed mid-turn. gemini is in neither, permanently.
#[test]
fn steer_and_steer_now_split_idle_from_working() {
    let steer = ActionManifest::parse(STEER, "steer", "steer.toml").unwrap();
    assert_eq!(
        steer.evaluate_gate(&row("claude", AgentState::Idle)),
        GateOutcome::Fireable
    );
    assert_eq!(
        steer.evaluate_gate(&row("claude", AgentState::Working)),
        GateOutcome::Refused(RefusalReason::Gated)
    );
    assert_eq!(
        steer.evaluate_gate(&row("gemini", AgentState::Idle)),
        GateOutcome::Refused(RefusalReason::WrongAgent)
    );

    let now = ActionManifest::parse(STEER_NOW, "steer_now", "steer_now.toml").unwrap();
    for agent in ["claude", "codex"] {
        assert!(
            now.text_for(agent).is_some_and(|t| t.steer_now),
            "{agent} must declare steer_now to be offered here"
        );
        assert_eq!(
            now.evaluate_gate(&row(agent, AgentState::Working)),
            GateOutcome::Fireable
        );
    }
    for agent in ["gemini", "opencode", "pi", "cursor"] {
        assert_eq!(
            now.evaluate_gate(&row(agent, AgentState::Working)),
            GateOutcome::Refused(RefusalReason::WrongAgent),
            "{agent} has not declared it queues a mid-turn message"
        );
    }
}

#[test]
fn approve_gates_on_blocked_permission() {
    let a = ActionManifest::parse(APPROVE, "approve", "approve.toml").unwrap();
    assert_eq!(a.keys_for("claude"), Some(["1".to_string()].as_slice()));
    // A-509. codex's approve option prints its own accelerator (`Yes, proceed (y)`), so `y` is
    // position-independent. `Enter` is not: it confirms whatever the `›` marker is resting on, which
    // tma never reads. Measured on codex 0.146.0 — cursor on "No", `y` approved, `Enter` denied.
    assert_eq!(a.keys_for("codex"), Some(["y".to_string()].as_slice()));
    assert!(
        !a.keys_for("codex").unwrap().contains(&"Enter".to_string()),
        "approve must not deliver a position-dependent confirm"
    );

    let blocked = GateInput {
        detail: Some("permission"),
        ..row("claude", AgentState::Blocked)
    };
    assert_eq!(a.evaluate_gate(&blocked), GateOutcome::Fireable);
    // Applies only to agents with a [keys] entry.
    assert_eq!(
        a.evaluate_gate(&GateInput {
            detail: Some("permission"),
            ..row("gemini", AgentState::Blocked)
        }),
        GateOutcome::Refused(RefusalReason::WrongAgent)
    );
    // Idle claude ⇒ gated (state gate not satisfied).
    assert_eq!(
        a.evaluate_gate(&row("claude", AgentState::Idle)),
        GateOutcome::Refused(RefusalReason::Gated)
    );
}

/// A-504 / A-505, at the gate. **The user-visible fix.** `approve` fires `1` on claude, and on the
/// plan dialog `1` is "Yes, and use auto mode" (auto-approve everything that follows) while on the
/// trust gate it is "Yes, I trust this folder" (a whole-folder grant). Before the manifest split
/// both dialogs were stamped `blocked/permission` and this gate returned `Fireable`.
///
/// Nothing in `approve.toml` changed to make this pass — re-typing the dialog is sufficient, because
/// the gate matches `detail` by exact string.
#[test]
fn approve_refuses_at_the_plan_and_trust_dialogs() {
    let a = ActionManifest::parse(APPROVE, "approve", "approve.toml").unwrap();
    for detail in ["plan", "trust"] {
        assert_eq!(
            a.evaluate_gate(&GateInput {
                detail: Some(detail),
                ..row("claude", AgentState::Blocked)
            }),
            GateOutcome::Refused(RefusalReason::Gated),
            "approve must not fire `1` at a blocked/{detail} pane"
        );
    }
    // The dialog it exists for still works.
    assert_eq!(
        a.evaluate_gate(&GateInput {
            detail: Some("permission"),
            ..row("claude", AgentState::Blocked)
        }),
        GateOutcome::Fireable
    );
}

/// The OpenCode `question` dialog is `blocked`, but it is a question to answer rather than a
/// permission to grant, so neither `approve` nor `deny` resolves at it. Nothing in either manifest
/// says `question`: they gate on `detail = permission` by exact string, which is what makes a new
/// blocked detail orphan them by default instead of silently inheriting a reply.
///
/// Pinned rather than assumed: OpenCode is the one agent whose `approve`/`deny` answer over HTTP,
/// and a `permission-reply` POST fired at a question would quote a request id that is not pending.
#[test]
fn approve_and_deny_refuse_at_the_opencode_question_dialog() {
    for (stem, src) in [("approve", APPROVE), ("deny", DENY)] {
        let a = ActionManifest::parse(src, stem, &format!("{stem}.toml")).unwrap();
        assert_eq!(
            a.evaluate_gate(&GateInput {
                detail: Some("question"),
                ..row("opencode", AgentState::Blocked)
            }),
            GateOutcome::Refused(RefusalReason::Gated),
            "{stem} must not answer a blocked/question pane"
        );
        // The dialog it does exist for is untouched on the same agent.
        assert_eq!(
            a.evaluate_gate(&GateInput {
                detail: Some("permission"),
                ..row("opencode", AgentState::Blocked)
            }),
            GateOutcome::Fireable,
            "{stem} still resolves an OpenCode permission prompt"
        );
    }
}

/// A-506. The resolved action table for a claude pane, asserted as data across the whole blocked
/// detail vocabulary. The split's entire intended effect is the two `plan`/`trust` rows: approve and
/// deny stop resolving there, and nothing else moves.
#[test]
fn the_claude_action_table_gains_exactly_two_refusing_rows() {
    let actions = [
        ("approve", APPROVE),
        ("deny", DENY),
        ("interrupt", INTERRUPT),
    ]
    .map(|(stem, src)| {
        (
            stem,
            ActionManifest::parse(src, stem, &format!("{stem}.toml")).unwrap(),
        )
    });

    // (blocked detail, the actions that resolve at it).
    let table: Vec<(&str, Vec<&str>)> = ["permission", "plan", "trust"]
        .iter()
        .map(|detail| {
            let fireable = actions
                .iter()
                .filter(|(_, a)| {
                    a.evaluate_gate(&GateInput {
                        detail: Some(detail),
                        ..row("claude", AgentState::Blocked)
                    }) == GateOutcome::Fireable
                })
                .map(|(stem, _)| *stem)
                .collect();
            (*detail, fireable)
        })
        .collect();

    assert_eq!(
        table,
        vec![
            ("permission", vec!["approve", "deny"]),
            ("plan", vec![]),
            ("trust", vec![]),
        ],
        "plan and trust must orphan approve and deny, and nothing else"
    );
}

#[test]
fn interrupt_gates_on_working() {
    let a = ActionManifest::parse(INTERRUPT, "interrupt", "interrupt.toml").unwrap();
    assert_eq!(
        a.keys_for("claude"),
        Some(["Escape".to_string()].as_slice())
    );
    assert_eq!(
        a.evaluate_gate(&row("claude", AgentState::Working)),
        GateOutcome::Fireable
    );
    assert_eq!(
        a.evaluate_gate(&row("claude", AgentState::Blocked)),
        GateOutcome::Refused(RefusalReason::Gated)
    );
}

#[test]
fn compact_gated_on_idle_and_high_context_fails_closed() {
    let a = ActionManifest::parse(COMPACT, "compact", "compact.toml").unwrap();
    assert_eq!(
        a.keys_for("claude"),
        Some(["/compact".to_string(), "Enter".to_string()].as_slice())
    );
    let when = a.when.as_ref().unwrap();
    assert_eq!(when.state, [AgentState::Idle]);
    assert_eq!(when.context_pct_min, Some(75));

    // No telemetry channel ⇒ no-coverage (permanent), regardless of state.
    assert_eq!(
        a.evaluate_gate(&row("claude", AgentState::Idle)),
        GateOutcome::Refused(RefusalReason::NoCoverage)
    );
    // Channel present, idle, context at/over threshold ⇒ fireable.
    let hot_idle = GateInput {
        context_covered: true,
        context_pct: Some(82),
        ..row("claude", AgentState::Idle)
    };
    assert_eq!(a.evaluate_gate(&hot_idle), GateOutcome::Fireable);
    // Channel present, idle, context below threshold ⇒ gated.
    let cool_idle = GateInput {
        context_covered: true,
        context_pct: Some(40),
        ..row("claude", AgentState::Idle)
    };
    assert_eq!(
        a.evaluate_gate(&cool_idle),
        GateOutcome::Refused(RefusalReason::Gated)
    );
    // Channel present, working (wrong state), high context ⇒ gated.
    let hot_working = GateInput {
        context_covered: true,
        context_pct: Some(82),
        ..row("claude", AgentState::Working)
    };
    assert_eq!(
        a.evaluate_gate(&hot_working),
        GateOutcome::Refused(RefusalReason::Gated)
    );
}
