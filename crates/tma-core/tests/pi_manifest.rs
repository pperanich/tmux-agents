//! Acceptance (pi 0.82.1): the bundled pi manifest, tested against redacted real captures driven
//! live in a scratch tmux server. Gated on the `fixtures` feature.
//!
//! pi is hook-capable via its EXTENSION system (a JS module, not a JSON hook block):
//! session_start/shutdown (lifecycle), before_agent_start + tool_execution_start (working),
//! agent_settled (idle). Identity is registration-first with a `title_patterns` signal (the stable
//! `π - <cwd>` title). blocked is not a state pi has (auto-runs tools), so no blocked rule. Tests
//! cover the shape, the working screen rule, and the negative that idle never reads as working.
#![cfg(feature = "fixtures")]

use std::path::{Path, PathBuf};

use tma_core::evidence::Claim;
use tma_core::fixture::Fixture;
use tma_core::manifest::CoverToken;
use tma_core::snapshot::PaneSnapshot;
use tma_core::{AgentState, Evaluation, Manifest, RuleEngine};

const PI_TOML: &str = include_str!("../manifests/pi.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(PI_TOML, "pi.toml").expect("bundled manifest parses")
}

fn engine() -> RuleEngine {
    RuleEngine::build(&manifest()).expect("bundled manifest regexes compile")
}

fn snapshot(fx: &Fixture) -> PaneSnapshot {
    PaneSnapshot {
        pane_id: "%0".to_string(),
        pid_tree: vec![],
        title: fx.title.clone(),
        tail_text: fx.capture.clone(),
        tail_hash: 0,
        alternate_on: true,
        scroll_position: None,
        visible_height: None,
        captured_at: fx.captured_at,
    }
}

fn evaluate(name: &str) -> Evaluation {
    let fx = Fixture::load(&fixtures_dir().join(name))
        .unwrap_or_else(|e| panic!("load fixture {name}: {e}"));
    engine().evaluate(&snapshot(&fx))
}

fn has_state(ev: &Evaluation, state: AgentState) -> bool {
    ev.evidence
        .iter()
        .any(|e| matches!(&e.claim, Claim::State(s) if s.state == state))
}

// ---- the manifest shape (live audit) ---------------------------------------------

#[test]
fn bundled_manifest_declares_title_narrowed_identity() {
    let m = manifest();
    // `pi` is the ps-walk comm; `node` is the `#{pane_current_command}` interpreter. Safe only
    // because title_patterns narrow them to the stable `π - <cwd>` title.
    assert_eq!(m.identity.process_names, ["node", "pi"]);
    assert_eq!(m.identity.title_patterns, ["^π "]);
}

#[test]
fn title_patterns_match_the_real_pi_title_only() {
    let eng = engine();
    assert!(eng.has_title_patterns());
    // The real title `π - <cwd-basename>`, stable even during a working turn (no flicker).
    assert!(eng.title_matches("π - myproj"), "the real pi title");
    assert!(eng.title_matches("π - pi-work"));
    // A plain hostname (the pre-trust startup title) or a bare node title must NOT match.
    assert!(!eng.title_matches("pp-ml1"));
    assert!(!eng.title_matches("node"));
}

#[test]
fn bundled_manifest_declares_verified_hook_coverage() {
    let m = manifest();
    let hooks = m
        .hooks
        .as_ref()
        .expect("pi is hook-capable (extension events)");
    // working + idle + lifecycle. blocked is NOT covered — pi has no approval state at all.
    assert_eq!(
        hooks.covers,
        [
            CoverToken::State(AgentState::Working),
            CoverToken::State(AgentState::Idle),
            CoverToken::Lifecycle,
        ]
    );
    assert!(
        !hooks
            .covers
            .contains(&CoverToken::State(AgentState::Blocked)),
        "pi auto-runs tools; blocked is not a pi state (honest gap)"
    );

    let by_event = |ev: &str| {
        hooks
            .map
            .iter()
            .find(|m| m.event == ev)
            .unwrap_or_else(|| panic!("map entry for {ev}"))
    };
    use tma_core::evidence::{Lifecycle, StateClaim};
    assert_eq!(
        by_event("session_start").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::Start
        }
    );
    assert_eq!(
        by_event("session_shutdown").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::End
        }
    );
    let working = Claim::State(StateClaim {
        state: AgentState::Working,
        detail: None,
    });
    for ev in ["before_agent_start", "tool_execution_start"] {
        assert_eq!(by_event(ev).claim, working, "{ev} ⇒ working");
        assert_eq!(by_event(ev).matcher, None, "{ev} maps unconditionally");
    }
    assert_eq!(
        by_event("agent_settled").claim,
        Claim::State(StateClaim {
            state: AgentState::Idle,
            detail: None,
        })
    );
}

#[test]
fn bundled_manifest_declares_pi_context_telemetry() {
    // pi's getContextUsage() push channel: the extension forwards it to `tma event --kind
    // context`, parsed by `pi-context-json`. `event` = a push shim (like Claude's statusline).
    let m = manifest();
    let ctx = m
        .telemetry
        .as_ref()
        .expect("pi declares a [telemetry] block")
        .context
        .as_ref()
        .expect("pi declares [telemetry.context]");
    assert_eq!(ctx.channel, tma_core::Channel::Event);
    assert_eq!(ctx.format, "pi-context-json");
    assert!(
        m.covers_context(),
        "a declared channel makes pi context-covered"
    );
}

// ---- working detected from the real streaming captures ---------------------------

#[test]
fn working_detected_at_wide_and_narrow() {
    // pi's built-in loader row (`Working...`) raises a working claim at both widths. Driven
    // live 2026-07-26.
    for name in ["pi_working_w100.txt", "pi_working_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "pi");
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Working),
            "{name}: the `Working...` loader row must raise a working claim"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: a working screen must never read blocked"
        );
    }
}

// ---- the real idle screen must never read working/blocked (safety) ---------------

#[test]
fn idle_screen_never_reads_working_or_blocked_at_wide_and_narrow() {
    // The real idle screen (empty composer, footer status + model row, no `Working...`) matches
    // NONE of the rules, so the engine raises no state evidence — the stop is signaled by the
    // agent_settled hook, not a screen rule. Keeps the working rule honest.
    for name in ["pi_idle_w100.txt", "pi_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "pi");
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Working),
            "{name}: the idle screen must never read working"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: the idle screen must never read blocked"
        );
    }
}
