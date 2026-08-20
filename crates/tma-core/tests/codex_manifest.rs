//! Acceptance (Codex CLI 0.145.0): the bundled Codex manifest, tested against redacted real
//! captures driven live in a scratch tmux server. Gated on the `fixtures` feature.
//!
//! The audit drove a real completed turn: every turn-gated hooks.json event fired
//! (PreToolUse/PostToolUse/PermissionRequest/Stop), so `blocked` is now hook-covered
//! (PermissionRequest), and the streaming (`working`) and approval-prompt (`blocked`) screens were
//! captured at two widths. Tests cover the shape, the working/blocked screen rules, and the
//! negative that the real idle screen never mis-reads as blocked.
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

const CODEX_TOML: &str = include_str!("../manifests/codex.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(CODEX_TOML, "codex.toml").expect("bundled manifest parses")
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

/// Run the full detector (engine + fold) on a fixture, as a live producer would.
fn fold_verdict(name: &str, prev: Option<StampedState>) -> Verdict {
    fold_verdict_at(name, prev, 10)
}

/// [`fold_verdict`] with the fold clock placed `after_ms` past the capture, so a test can sit
/// either side of the working→idle dwell.
fn fold_verdict_at(name: &str, prev: Option<StampedState>, after_ms: u64) -> Verdict {
    let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
    let snap = snapshot(&fx);
    let ev = engine().evaluate(&snap);
    let facts = SnapshotFacts {
        pid: 1,
        // The Codex binary is the pane foreground (a direct launch), so the foreground cap is lifted —
        // this is what the manifest's second process-name spelling (`codex-aarch64-a`) buys at
        // runtime. Set true here to exercise the screen path rather than the cap.
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
        stamped_at: now,
        attention: false,
        notified_at: None,
        hash: None,
        pid: 1,
        session: None,
        subagents: vec![],
    }
}

fn has_state(ev: &Evaluation, state: AgentState) -> bool {
    ev.evidence
        .iter()
        .any(|e| matches!(&e.claim, Claim::State(s) if s.state == state))
}

fn idle_prior(now: u64) -> StampedState {
    StampedState {
        state: AgentState::Idle,
        detail: None,
        // The Codex idle stamp comes from the notify hook (turn-complete).
        source: Provenance::Hook,
        evidence_at: now,
        since: now,
        stamped_at: now,
        attention: false,
        notified_at: None,
        hash: None,
        pid: 1,
        session: None,
        subagents: vec![],
    }
}

// ---- the manifest shape (all turn events verified live; blocked now hook-covered) ------

#[test]
fn bundled_manifest_declares_verified_hook_coverage() {
    let m = manifest();
    // Both comm spellings: `codex` for the ps-walk identity, `codex-aarch64-a` for the
    // libproc-truncated `#{pane_current_command}` foreground check (the cap).
    assert_eq!(m.identity.process_names, ["codex", "codex-aarch64-a"]);

    // The working (streaming) and blocked (approval-prompt) screens were driven live
    // and captured, so both are capture-visible with real screen rules. Idle now has a screen
    // rule (the composer arrow) but deliberately stays OUT of `visible`: the composer renders
    // mid-turn too, so it is not evidence a turn ENDED and must never decay an idle hook claim.
    assert_eq!(
        m.capture.visible,
        [AgentState::Working, AgentState::Blocked]
    );
    assert!(
        !m.capture.visible.contains(&AgentState::Idle),
        "idle has a rule but must stay outside [capture].visible"
    );
    assert!(
        !m.rules.is_empty(),
        "codex ships working/blocked screen rules from real captures"
    );

    let hooks = m.hooks.as_ref().expect("codex is hook-capable");
    // Coverage after the turn-gated verification: working (UserPromptSubmit / PreToolUse
    // / PostToolUse) + idle (Stop + notify agent-turn-complete) + blocked (PermissionRequest,
    // verified live) + lifecycle (SessionStart/SessionEnd). Codex is no longer the
    // blocked-not-hook-covered differentiator: PermissionRequest hook-covers blocked.
    assert_eq!(
        hooks.covers,
        [
            CoverToken::State(AgentState::Working),
            CoverToken::State(AgentState::Idle),
            CoverToken::State(AgentState::Blocked),
            CoverToken::Lifecycle,
        ]
    );

    // Eight map entries: seven live-verified hooks.json events + the notify matcher.
    assert_eq!(hooks.map.len(), 8);
    let by_event = |ev: &str| {
        hooks
            .map
            .iter()
            .find(|m| m.event == ev)
            .unwrap_or_else(|| panic!("map entry for {ev}"))
    };

    let start = by_event("SessionStart");
    assert_eq!(start.matcher, None);
    assert_eq!(
        start.claim,
        Claim::Lifecycle {
            lifecycle: tma_core::evidence::Lifecycle::Start
        }
    );
    let end = by_event("SessionEnd");
    assert_eq!(end.matcher, None);
    assert_eq!(
        end.claim,
        Claim::Lifecycle {
            lifecycle: tma_core::evidence::Lifecycle::End
        }
    );

    let working_claim = Claim::State(tma_core::evidence::StateClaim {
        state: AgentState::Working,
        detail: None,
    });
    // UserPromptSubmit / PreToolUse / PostToolUse all mean the turn is active ⇒ working.
    for ev in ["UserPromptSubmit", "PreToolUse", "PostToolUse"] {
        let m = by_event(ev);
        assert_eq!(m.matcher, None, "{ev} maps unconditionally");
        assert_eq!(m.claim, working_claim, "{ev} ⇒ working");
    }

    // PermissionRequest ⇒ blocked/permission — the hook that covers blocked (no matcher:
    // codex fires it only when it actually needs approval).
    let perm = by_event("PermissionRequest");
    assert_eq!(perm.matcher, None);
    assert_eq!(
        perm.claim,
        Claim::State(tma_core::evidence::StateClaim {
            state: AgentState::Blocked,
            detail: Some(tma_core::state::Detail::new("permission")),
        })
    );

    // Stop and notify(agent-turn-complete) both ⇒ idle (two channels, same turn-end signal).
    let idle_claim = Claim::State(tma_core::evidence::StateClaim {
        state: AgentState::Idle,
        detail: None,
    });
    let stop = by_event("Stop");
    assert_eq!(stop.matcher, None);
    assert_eq!(stop.claim, idle_claim);
    let n = by_event("notify");
    assert_eq!(n.matcher.as_deref(), Some("agent-turn-complete"));
    assert_eq!(n.claim, idle_claim);

    // Context telemetry: Codex declares the rollout file-tail channel, so it covers the
    // context metric (a context-gated action refuses `gated`, not the permanent `no-coverage`).
    let ctx = m
        .telemetry
        .as_ref()
        .and_then(|t| t.context.as_ref())
        .expect("codex declares [telemetry.context]");
    assert_eq!(ctx.channel, tma_core::Channel::FileTail);
    assert_eq!(ctx.format, "codex-rollout-jsonl");
    assert!(m.covers_context());
}

// ---- working detected from the real streaming captures -----------------------------

#[test]
fn working_detected_at_wide_and_narrow() {
    // The streaming footer (`esc to interrupt`) and the braille-spinner title both raise a
    // working claim; the fold verdict is working. Driven live 2026-07-25.
    for name in ["codex_working_w100.txt", "codex_working_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "codex");
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Working),
            "{name}: streaming chrome must raise a working claim"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: a working screen must never read blocked"
        );
        let v = fold_verdict(name, None);
        assert_eq!(v.state, AgentState::Working, "{name}: verdict working");
    }
}

// ---- blocked detected from the real approval-prompt captures -----------------------

#[test]
fn blocked_detected_at_wide_and_narrow() {
    // The command-approval dialog (`Would you like to run the following command?` +
    // `Press enter to confirm`) raises a blocked/permission claim; the fold verdict is
    // blocked. Driven live 2026-07-25.
    for name in ["codex_blocked_w100.txt", "codex_blocked_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "codex");
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Blocked),
            "{name}: approval chrome must raise a blocked claim"
        );
        let v = fold_verdict(name, None);
        assert_eq!(v.state, AgentState::Blocked, "{name}: verdict blocked");
        assert_eq!(v.detail.as_ref().map(|d| d.as_str()), Some("permission"));
    }
}

// ---- the real idle screen reads idle, and never blocked (safety) -------------------

#[test]
fn idle_screen_never_reads_as_blocked_at_wide_and_narrow() {
    // The two real idle captures (composer prompt, welcome box, model/effort footer) at the
    // wide and narrow widths driven live. The idle chrome matches NONE of the working/blocked
    // rules (no `esc to interrupt`, no braille title, no approval dialog); it matches the idle
    // rule and nothing else, so the engine raises EXACTLY one claim, `idle` — critically it must
    // NEVER synthesize `blocked` from the idle screen (the forbidden direction). This negative
    // regression keeps the working/blocked rules honest.
    for name in ["codex_idle_w100.txt", "codex_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "codex");
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: idle chrome must not raise a blocked claim"
        );
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: the composer arrow must raise an idle claim"
        );
        assert_eq!(
            ev.evidence.len(),
            1,
            "{name}: exactly one claim, the idle one"
        );
        // A prior idle notify stamp is held, never flipped to blocked by the idle screen.
        let v = fold_verdict(name, Some(idle_prior(fx.captured_at)));
        assert_ne!(
            v.state,
            AgentState::Blocked,
            "{name}: verdict never blocked"
        );
    }
}

// ---- the approval dialog must not raise an idle claim ------------------------------

#[test]
fn blocked_screen_raises_no_idle_claim() {
    // Codex numbers the approval options with the SAME arrow the composer uses
    // (`› 1. Yes, proceed (y)`). The idle rule's `tail_lines(6)` window is what keeps them
    // apart: in both blocked captures the nearest `›` is seven rows from the end. If this
    // fails, widen the guard rather than the window.
    for name in ["codex_blocked_w100.txt", "codex_blocked_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Idle),
            "{name}: the approval option list must not read as the composer"
        );
    }
}

// ---- the pinned-`working` trap: a finished turn now lands --------------------------

#[test]
fn idle_screen_releases_a_pinned_working_stamp() {
    // The bug batch B closes: with no idle rule the idle screen raised nothing, so every cycle
    // after a turn hit `hold previous` and the pane stayed `working` forever.
    for name in ["codex_idle_w100.txt", "codex_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        let held = fold_verdict_at(name, Some(working_prior(fx.captured_at)), 1_000);
        assert_eq!(
            held.state,
            AgentState::Working,
            "{name}: inside the dwell, working→idle is still suppressed"
        );
        let landed = fold_verdict_at(name, Some(working_prior(fx.captured_at)), 10_000);
        assert_eq!(
            landed.state,
            AgentState::Idle,
            "{name}: past the dwell the composer claim releases the pinned working stamp"
        );
    }
}

// ---- the idle rule co-renders with working, and must lose to it --------------------

#[test]
fn working_screen_also_matches_the_idle_rule_but_still_folds_working() {
    // Codex keeps the composer on screen under the streaming footer, so the working screen
    // raises BOTH claims. That is by design (claude's `⏵⏵` precedent); the fold's slot order is
    // what resolves it. If this ever folds to idle, the ladder in `fold.rs` has been reordered.
    for name in ["codex_working_w100.txt", "codex_working_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: the composer renders mid-turn too"
        );
        assert!(has_state(&ev, AgentState::Working), "{name}: working too");
        let v = fold_verdict(name, None);
        assert_eq!(
            v.state,
            AgentState::Working,
            "{name}: working outranks the co-rendered idle claim"
        );
    }
}
