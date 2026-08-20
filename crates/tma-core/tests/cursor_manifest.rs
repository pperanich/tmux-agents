//! Acceptance (Cursor CLI 2026.07.23-e383d2b): the bundled Cursor manifest, tested against
//! redacted real captures driven live in a scratch tmux server. Gated on the `fixtures` feature.
//!
//! A re-drive overturned the earlier "hookless" finding: cursor fires USER-level hooks
//! (`~/.cursor/hooks.json`), so it has a hook registration path (sessionStart/End,
//! beforeSubmitPrompt, preToolUse/postToolUse, stop) plus the `title_patterns` signal. Tests cover
//! the shape, the working/blocked screen rules, and the negative that idle never reads as blocked.
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

const CURSOR_TOML: &str = include_str!("../manifests/cursor.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(CURSOR_TOML, "cursor.toml").expect("bundled manifest parses")
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
    // Both generic comms cursor really reports; safe only because title_patterns narrow them.
    assert_eq!(m.identity.process_names, ["node", "agent"]);
    assert_eq!(m.identity.title_patterns, ["^Cursor Agent$"]);
}

#[test]
fn title_patterns_match_the_real_idle_title_only() {
    let eng = engine();
    assert!(eng.has_title_patterns());
    assert!(eng.title_matches("Cursor Agent"), "the real idle title");
    // The tool-name flicker titles must NOT title-match — stickiness holds them, the
    // pattern does not.
    assert!(!eng.title_matches("Shell Command Output"));
    assert!(!eng.title_matches("node"));
}

#[test]
fn bundled_manifest_declares_verified_hook_coverage() {
    let m = manifest();
    let hooks = m
        .hooks
        .as_ref()
        .expect("cursor is hook-capable (user-level hooks)");
    // working + idle + lifecycle. blocked is NOT hook-covered (no permission hook) — it rides the
    // screen rule only.
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
        "cursor has no approval hook; blocked is screen-only (honest gap)"
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
        by_event("sessionStart").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::Start
        }
    );
    assert_eq!(
        by_event("sessionEnd").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::End
        }
    );
    let working = Claim::State(StateClaim {
        state: AgentState::Working,
        detail: None,
    });
    for ev in ["beforeSubmitPrompt", "preToolUse", "postToolUse"] {
        assert_eq!(by_event(ev).claim, working, "{ev} ⇒ working");
        assert_eq!(by_event(ev).matcher, None, "{ev} maps unconditionally");
    }
    // postToolUseFailure: a mid-turn tool error is a working continuation, but gated on
    // the non-interrupt flag so a user-abort variant cannot false-stamp working.
    assert_eq!(
        by_event("postToolUseFailure").claim,
        working,
        "postToolUseFailure ⇒ working"
    );
    assert_eq!(
        by_event("postToolUseFailure").matcher.as_deref(),
        Some(r#""is_interrupt":\s*false"#),
        "postToolUseFailure is gated on the non-interrupt flag"
    );
    assert_eq!(
        by_event("stop").claim,
        Claim::State(StateClaim {
            state: AgentState::Idle,
            detail: None,
        })
    );
}

#[test]
fn bundled_manifest_declares_cursor_context_telemetry() {
    // Cursor's statusLine push channel: `cli-config.json`'s statusLine command
    // forwards its `context_window` payload to `tma event --kind context`, parsed by
    // `cursor-statusline-json`. `event` = a push shim, like Claude's statusline.
    let m = manifest();
    let ctx = m
        .telemetry
        .as_ref()
        .expect("cursor declares a [telemetry] block")
        .context
        .as_ref()
        .expect("cursor declares [telemetry.context]");
    assert_eq!(ctx.channel, tma_core::Channel::Event);
    assert_eq!(ctx.format, "cursor-statusline-json");
    assert!(
        m.covers_context(),
        "a declared channel makes cursor context-covered"
    );
}

// ---- working detected from the real streaming captures ---------------------------

#[test]
fn working_detected_at_wide_and_narrow() {
    // The active-turn footer (`ctrl+c to stop`) raises a working claim at both widths. Driven
    // live 2026-07-26.
    for name in ["cursor_working_w100.txt", "cursor_working_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "cursor");
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Working),
            "{name}: active-turn chrome must raise a working claim"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: a working screen must never read blocked"
        );
    }
}

// ---- blocked detected from the real approval-prompt captures ---------------------

#[test]
fn blocked_detected_at_wide_and_narrow() {
    // The shell-approval dialog (`Run this command?` + `Not in allowlist`) raises a
    // blocked/permission claim at both widths. This is the ONLY blocked signal cursor offers.
    for name in ["cursor_blocked_w100.txt", "cursor_blocked_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "cursor");
        let ev = evaluate(name);
        let blocked = ev
            .evidence
            .iter()
            .find_map(|e| match &e.claim {
                Claim::State(s) if s.state == AgentState::Blocked => Some(s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: approval chrome must raise a blocked claim"));
        assert_eq!(
            blocked.detail.as_ref().map(|d| d.as_str()),
            Some("permission")
        );
    }
}

// ---- the visible-screen clamp keeps scrollback chrome from false-blocking ----------

#[test]
fn prior_turn_approval_chrome_in_scrollback_does_not_false_block() {
    // A short split pane (12 visible rows) whose `capture-pane -S -50` reaches back into
    // scrollback. A PRIOR turn's approval dialog (`Run this command?` + `Not in allowlist`) sits
    // in the scrollback portion, above the visible screen; the visible screen itself is an idle
    // composer with none of the blocked anchors. Cursor's blocked rule is screen-authoritative,
    // so a whole-screen match here would be a false `blocked` (the sharp edge the clamp fixes). The
    // `visible` region clamps to `#{pane_height}` (12), so the scrollback dialog is out of scope.
    let scrollback = "\
Run this command?
Not in allowlist: rm -rf build
❯ 1. Yes  2. No
(prior turn, now scrolled above the visible screen)";
    let visible = {
        let mut rows = vec!["→ Plan, search, build anything"];
        rows.extend(vec!["idle composer line"; 11]);
        rows.join("\n")
    };
    let tail = format!("{scrollback}\n{visible}\n");

    let base = PaneSnapshot {
        pane_id: "%0".to_string(),
        pid_tree: vec![],
        title: "Cursor Agent".to_string(),
        tail_text: tail,
        tail_hash: 0,
        alternate_on: true,
        scroll_position: None,
        visible_height: Some(12),
        captured_at: 1_000,
    };

    // Clamped to the 12 visible rows: the scrollback approval dialog is invisible ⇒ no blocked.
    assert!(
        !has_state(&engine().evaluate(&base), AgentState::Blocked),
        "prior-turn approval chrome in scrollback must not raise blocked once clamped to the \
         visible screen"
    );

    // Control: without the clamp (height unknown ⇒ whole tail) the SAME capture DOES false-block,
    // proving the fixture actually exercises the scrollback leak the `visible` region closes.
    let unclamped = PaneSnapshot {
        visible_height: None,
        ..base.clone()
    };
    assert!(
        has_state(&engine().evaluate(&unclamped), AgentState::Blocked),
        "the leak is real: unclamped whole-screen evaluation matches the scrollback approval chrome"
    );
}

// ---- the real idle screen reads idle, and never working/blocked (safety) ---------

#[test]
fn idle_screen_never_reads_working_or_blocked_at_wide_and_narrow() {
    // Both idle shapes: the fresh session (`→ Plan, search, build anything`) and a real completed
    // turn (`→ Add a follow-up`, captured 2026.08.11 for the idle rule). Neither matches the
    // working or blocked rules (no `ctrl+c to stop`, no approval dialog); both match the idle
    // rule and nothing else, so the engine raises EXACTLY one claim, `idle` — critically it never
    // synthesizes `blocked` from the idle screen (the forbidden direction). Keeps the rules
    // honest.
    for name in [
        "cursor_idle_w100.txt",
        "cursor_idle_w60.txt",
        "cursor_idle_post_turn_w100.txt",
        "cursor_idle_post_turn_w60.txt",
    ] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "cursor");
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: idle chrome must not raise a blocked claim"
        );
        assert!(
            !has_state(&ev, AgentState::Working),
            "{name}: idle chrome must not raise a working claim"
        );
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: the composer box must raise an idle claim"
        );
        assert_eq!(
            ev.evidence.len(),
            1,
            "{name}: exactly one claim, the idle one"
        );
    }
}

// ---- the approval dialog reuses the arrow glyph and must not read idle -----------

#[test]
fn blocked_screen_raises_no_idle_claim() {
    // Cursor prefixes its approval options with the SAME glyph as the composer
    // (`→ Run (once) (y)`), which is exactly why the idle rule requires the surrounding
    // half-block frame as well: the dialog replaces the composer box outright, so the frame is
    // absent from both blocked captures.
    for name in ["cursor_blocked_w100.txt", "cursor_blocked_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Idle),
            "{name}: the approval option list must not read as the composer"
        );
    }
}

// ---- the pinned-`working` trap: a finished turn now lands ------------------------

#[test]
fn idle_screen_releases_a_pinned_working_stamp() {
    // The bug batch B closes, shown on the post-turn captures specifically: with no idle rule a
    // pane that finished a turn raised nothing, so every later cycle hit `hold previous` and the
    // row stayed `working` forever.
    for name in [
        "cursor_idle_post_turn_w100.txt",
        "cursor_idle_post_turn_w60.txt",
    ] {
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
    // Cursor keeps the composer box on screen for the whole turn — the working captures show
    // `→ Add a follow-up` inside the frame with `ctrl+c to stop` beside it — so a live turn
    // raises BOTH claims. That is by design (claude's `⏵⏵` precedent); the fold's slot order is
    // what resolves it. If this ever folds to idle, the ladder in `fold.rs` has been reordered.
    for name in ["cursor_working_w100.txt", "cursor_working_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: the composer box renders mid-turn too"
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

// ---- the post-turn captures also pin the title finding ---------------------------

#[test]
fn post_turn_idle_title_is_not_cursor_agent() {
    // The identity note in the manifest says cursor's OSC title does NOT revert after a turn.
    // These captures are the evidence: their title is the conversation summary, so the title
    // pattern must reject it and identity has to lean on the pid-anchored stickiness hold.
    for name in [
        "cursor_idle_post_turn_w100.txt",
        "cursor_idle_post_turn_w60.txt",
    ] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_ne!(fx.title, "Cursor Agent", "{name}: the title did not revert");
        assert!(
            !engine().title_matches(&fx.title),
            "{name}: a post-turn title must not satisfy the identity pattern"
        );
    }
}

// ---- idle has a rule but is deliberately not capture-visible ---------------------

#[test]
fn idle_has_a_rule_but_stays_outside_capture_visible() {
    let m = manifest();
    assert_eq!(
        m.capture.visible,
        [AgentState::Working, AgentState::Blocked]
    );
    assert!(
        m.rules
            .iter()
            .any(|r| r.state == AgentState::Idle && !r.skip_state_update),
        "cursor ships a positive idle rule"
    );
    // The composer box renders mid-turn too, so it is not evidence a turn ENDED and must never be
    // allowed to decay an idle hook claim.
    assert!(
        !m.capture.visible.contains(&AgentState::Idle),
        "idle has a rule but must stay outside [capture].visible"
    );
}
