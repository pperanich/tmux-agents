//! Parse round-trips and gate assertions for the bundled action manifests. These are the
//! compiled-in `keys` actions (`approve`, `deny`, `interrupt`, `compact`); the loader that
//! embeds them lives in `tma-runtime`, so here we assert each parses under its own stem and gates
//! as ACTIONS.md pins.

use tma_core::action::{ActionKind, ContextKeys, GateInput, GateOutcome, RefusalReason};
use tma_core::evidence::Claim;
use tma_core::{ActionManifest, AgentState, Manifest};

const APPROVE: &str = include_str!("../actions/approve.toml");
const DENY: &str = include_str!("../actions/deny.toml");
const INTERRUPT: &str = include_str!("../actions/interrupt.toml");
const COMPACT: &str = include_str!("../actions/compact.toml");

/// The bundled agent manifests, so the coverage assertions read what each agent actually declares
/// instead of restating a hand-written list that drifts the moment an agent is added.
const AGENTS: &[(&str, &str)] = &[
    ("claude", include_str!("../manifests/claude.toml")),
    ("codex", include_str!("../manifests/codex.toml")),
    ("cursor", include_str!("../manifests/cursor.toml")),
    ("gemini", include_str!("../manifests/gemini.toml")),
    ("opencode", include_str!("../manifests/opencode.toml")),
    ("pi", include_str!("../manifests/pi.toml")),
];

fn agent_manifests() -> Vec<(&'static str, Manifest)> {
    AGENTS
        .iter()
        .map(|(name, src)| {
            let m = Manifest::parse(src, &format!("{name}.toml"))
                .unwrap_or_else(|e| panic!("{name} manifest must parse: {e}"));
            (*name, m)
        })
        .collect()
}

/// Whether `m` claims `state` from either channel it has: a hook mapping or a screen rule.
fn claims_state(m: &Manifest, state: AgentState) -> bool {
    let hooked = m
        .hooks
        .iter()
        .flat_map(|h| &h.map)
        .any(|hm| matches!(&hm.claim, Claim::State(sc) if sc.state == state));
    hooked || m.rules.iter().any(|r| r.state == state)
}

/// Whether `m` declares a permission dialog: a `blocked` claim carrying the `permission` detail,
/// from a hook mapping or a screen rule.
fn declares_permission(m: &Manifest) -> bool {
    let hooked = m.hooks.iter().flat_map(|h| &h.map).any(|hm| {
        matches!(&hm.claim, Claim::State(sc)
            if sc.state == AgentState::Blocked
                && sc.detail.as_ref().map(|d| d.as_str()) == Some("permission"))
    });
    hooked
        || m.rules.iter().any(|r| {
            r.state == AgentState::Blocked
                && r.detail.as_ref().map(|d| d.as_str()) == Some("permission")
        })
}

fn action(stem: &str, src: &str) -> ActionManifest {
    ActionManifest::parse(src, stem, &format!("{stem}.toml"))
        .unwrap_or_else(|e| panic!("{stem} must parse: {e}"))
}

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
    // Applies only to agents with a transport entry. pi has no permission prompt at all (R29), so
    // it is the agent this can never cover.
    assert_eq!(
        a.evaluate_gate(&GateInput {
            detail: Some("permission"),
            ..row("pi", AgentState::Blocked)
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

/// A-277. The action rows, read against what the agent manifests declare rather than against a
/// list retyped here: every agent with a permission dialog can answer it in both directions, and
/// every agent that can be working can be interrupted.
#[test]
fn every_declared_dialog_has_an_answer_and_every_working_agent_an_interrupt() {
    let approve = action("approve", APPROVE);
    let deny = action("deny", DENY);
    let interrupt = action("interrupt", INTERRUPT);

    for (name, m) in agent_manifests() {
        let permission = declares_permission(&m);
        assert_eq!(
            approve.applies_to(name),
            permission,
            "{name}: approve coverage must match whether it declares a permission dialog"
        );
        assert_eq!(
            deny.applies_to(name),
            permission,
            "{name}: deny coverage must match whether it declares a permission dialog"
        );
        assert_eq!(
            interrupt.applies_to(name),
            claims_state(&m, AgentState::Working),
            "{name}: interrupt must cover exactly the agents that have a working state"
        );
    }
}

/// A-277, R26. `Run Everything (shift+tab)` sits one modifier from cursor's tool-scoped grant, and
/// it is a session-wide YOLO grant. No bundled action may reach it, under any name.
#[test]
fn no_bundled_action_maps_cursor_to_shift_tab() {
    for (stem, src) in [
        ("approve", APPROVE),
        ("deny", DENY),
        ("interrupt", INTERRUPT),
        ("compact", COMPACT),
    ] {
        let keys = action(stem, src).keys_for("cursor").unwrap_or(&[]).to_vec();
        for spelling in ["S-Tab", "BTab", "shift+tab"] {
            assert!(
                !keys.iter().any(|k| k == spelling),
                "{stem} must not send cursor's session-wide grant ({spelling})"
            );
        }
    }
}

/// The rows U7 filled, pinned as data. Each is a keystroke into somebody else's TUI, so a change
/// here is a change to what a phone tap does and has to be re-evidenced, not merely re-reviewed.
#[test]
fn the_filled_rows_are_the_evidenced_ones() {
    let approve = action("approve", APPROVE);
    let deny = action("deny", DENY);
    let interrupt = action("interrupt", INTERRUPT);

    for (agent, want) in [("gemini", "1"), ("cursor", "y")] {
        assert_eq!(approve.keys_for(agent), Some([want.to_string()].as_slice()));
    }
    for (agent, want) in [("gemini", "3"), ("cursor", "n")] {
        assert_eq!(deny.keys_for(agent), Some([want.to_string()].as_slice()));
    }
    for (agent, want) in [
        ("cursor", "C-c"),
        ("gemini", "Escape"),
        ("opencode", "Escape"),
        ("pi", "Escape"),
    ] {
        assert_eq!(
            interrupt.keys_for(agent),
            Some([want.to_string()].as_slice()),
            "{agent} interrupt"
        );
    }
    // R29: pi has no permission prompt, so no answer to one may resolve for it in any state.
    assert!(!approve.applies_to("pi") && !deny.applies_to("pi"));
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
