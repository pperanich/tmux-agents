//! Parse round-trips and gate assertions for the bundled action manifests. These are the
//! compiled-in `keys` actions (`approve`, `deny`, `interrupt`, `compact`); the loader that
//! embeds them lives in `tma-runtime`, so here we assert each parses under its own stem and gates
//! as ACTIONS.md pins.

use tma_core::action::{ActionKind, ContextKeys, GateInput, GateOutcome, RefusalReason};
use tma_core::{ActionManifest, AgentState};

const APPROVE: &str = include_str!("../actions/approve.toml");
const DENY: &str = include_str!("../actions/deny.toml");
const INTERRUPT: &str = include_str!("../actions/interrupt.toml");
const COMPACT: &str = include_str!("../actions/compact.toml");

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
fn approve_gates_on_blocked_permission() {
    let a = ActionManifest::parse(APPROVE, "approve", "approve.toml").unwrap();
    assert_eq!(a.keys_for("claude"), Some(["1".to_string()].as_slice()));
    assert_eq!(a.keys_for("codex"), Some(["Enter".to_string()].as_slice()));

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
