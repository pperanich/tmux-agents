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
use tma_core::stamp::StampedState;
use tma_core::{
    verdict, AgentState, Evaluation, FoldConfig, Manifest, Provenance, RuleEngine, SnapshotFacts,
    Verdict,
};

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

/// Run the full detector (engine + fold) on a fixture, as a live producer would. `after_ms` places
/// the fold clock relative to the capture, so a test can sit either side of the working→idle dwell.
fn fold_verdict(name: &str, prev: Option<StampedState>, after_ms: u64) -> Verdict {
    let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
    let snap = snapshot(&fx);
    let ev = engine().evaluate(&snap);
    let facts = SnapshotFacts {
        pid: 1,
        foreground_is_agent: true,
        scrolled: false,
        history_view: ev.history_view,
    };
    verdict(
        prev,
        &facts,
        &ev.evidence,
        &manifest(),
        &FoldConfig::default(),
        snap.captured_at + after_ms,
    )
}

/// A screen-stamped `working` prior — the state a pane is pinned at once the turn's chrome leaves
/// the screen. `Provenance::Capture` (not `Hook`) keeps the fold on the plain ladder, which is the
/// path that used to dead-end in `hold previous`.
fn working_prior(now: u64) -> StampedState {
    StampedState {
        state: AgentState::Working,
        detail: None,
        source: Provenance::Capture,
        evidence_at: now,
        since: now,
        turn_at: 0,
        stamped_at: now,
        attention: false,
        notified_at: None,
        hash: None,
        pid: 1,
        session: None,
        subagents: vec![],
    }
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
    // the working rule NOT at all and the idle rule exactly once, so the engine raises EXACTLY
    // one claim, `idle`. Keeps the working rule honest.
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
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: the composer frame + context gauge must raise an idle claim"
        );
        assert_eq!(
            ev.evidence.len(),
            1,
            "{name}: exactly one claim, the idle one"
        );
    }
}

// ---- the pinned-`working` trap: a finished turn now lands ------------------------

#[test]
fn idle_screen_releases_a_pinned_working_stamp() {
    // The bug batch B closes: with no idle rule the settled screen raised nothing, so every cycle
    // after a turn hit `hold previous` and the pane stayed `working` forever.
    for name in ["pi_idle_w100.txt", "pi_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        let held = fold_verdict(name, Some(working_prior(fx.captured_at)), 1_000);
        assert_eq!(
            held.state,
            AgentState::Working,
            "{name}: inside the dwell, working→idle is still suppressed"
        );
        let landed = fold_verdict(name, Some(working_prior(fx.captured_at)), 10_000);
        assert_eq!(
            landed.state,
            AgentState::Idle,
            "{name}: past the dwell the composer claim releases the pinned working stamp"
        );
    }
}

// ---- the idle rule co-renders with working, and must lose to it ------------------

#[test]
fn working_screen_also_matches_the_idle_rule_but_still_folds_working() {
    // pi's composer frame and status row render identically in both states — that is exactly what
    // the older manifest note called disqualifying. It is not: the fold's slot order
    // (blocked → working → idle) resolves the co-render while `Working...` is on screen, the same
    // shape as claude's `⏵⏵` rule. If this ever folds to idle, the ladder in `fold.rs` has been
    // reordered.
    for name in ["pi_working_w100.txt", "pi_working_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: pi's bottom chrome renders mid-turn too"
        );
        assert!(has_state(&ev, AgentState::Working), "{name}: working too");
        let v = fold_verdict(name, None, 10);
        assert_eq!(
            v.state,
            AgentState::Working,
            "{name}: working outranks the co-rendered idle claim"
        );
    }
}

// ---- idle has a rule but is deliberately not capture-visible ---------------------

#[test]
fn idle_has_a_rule_but_stays_outside_capture_visible() {
    let m = manifest();
    assert_eq!(m.capture.visible, [AgentState::Working]);
    assert!(
        m.rules
            .iter()
            .any(|r| r.state == AgentState::Idle && !r.skip_state_update),
        "pi ships a positive idle rule"
    );
    // The composer renders mid-turn too, so it is not evidence a turn ENDED and must never be
    // allowed to decay an idle hook claim.
    assert!(
        !m.capture.visible.contains(&AgentState::Idle),
        "idle has a rule but must stay outside [capture].visible"
    );
}
