//! Acceptance (Gemini CLI 0.46.0): the bundled manifest, tested against redacted real captures
//! driven live in an isolated-HOME scratch tmux server. Gated on the `fixtures` feature.
//!
//! Gemini has a passive title identity: its OSC title encodes state as `<glyph> <phrase> (<cwd>)`,
//! distinct per state (idle `◇  Ready`, working `✦  Working…`, blocked `✋  Action Required`), so
//! `process_names = ["node"]` narrowed by title_patterns is safe. It also fires a `Notification`
//! hook ("ToolPermission") before an approval prompt is answered, so blocked is hook-covered. Tests
//! pin the shape, the working/blocked screen rules, and the negatives.
#![cfg(feature = "fixtures")]

use std::path::{Path, PathBuf};

use tma_core::evidence::{Claim, Lifecycle, StateClaim};
use tma_core::fixture::Fixture;
use tma_core::manifest::CoverToken;
use tma_core::snapshot::PaneSnapshot;
use tma_core::stamp::StampedState;
use tma_core::{
    verdict, AgentState, Evaluation, FoldConfig, Manifest, Provenance, RuleEngine, SnapshotFacts,
    Verdict,
};

const GEMINI_TOML: &str = include_str!("../manifests/gemini.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(GEMINI_TOML, "gemini.toml").expect("bundled manifest parses")
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
fn identity_is_title_narrowed_node() {
    let m = manifest();
    // A live gemini pane's comm is `node` on both read paths, so process_names is `["node"]`
    // NARROWED by title_patterns — `node` alone would false-match every Node app, the title
    // patterns make it safe. The three patterns are the real observed state titles.
    assert_eq!(m.identity.process_names, ["node"]);
    assert_eq!(
        m.identity.title_patterns,
        ["^◇  Ready ", "^✦  Working…", "^✋  Action Required "]
    );
}

#[test]
fn title_patterns_match_every_observed_state_title_and_reject_a_bare_node_pane() {
    let eng = engine();
    assert!(eng.has_title_patterns());
    // The three real 0.46.0 titles (isolated HOME, cwd basename `work`). gemini sets a distinct,
    // matching title in EVERY state, so the pattern set covers all of idle/working/blocked — the
    // title is not merely a startup/idle catch here (that is cursor's shape).
    assert!(eng.title_matches("◇  Ready (work)"), "idle");
    assert!(eng.title_matches("✦  Working… (work)"), "working");
    assert!(
        eng.title_matches("✋  Action Required (work)"),
        "blocked/approval"
    );
    // A different cwd still matches (the trailing `(<dir>)` is not part of the anchor).
    assert!(eng.title_matches("◇  Ready (my-project)"));
    // A plain node pane (a dev server, a REPL, tmux's default hostname title) must NOT match, so
    // process_names = ["node"] stays safe: the title is the whole narrowing.
    assert!(!eng.title_matches("node"));
    assert!(!eng.title_matches("my-app  Ready to serve on :3000"));
    assert!(!eng.title_matches("pp-ml1"));
}

#[test]
fn hooks_cover_working_idle_blocked_lifecycle() {
    let m = manifest();
    let hooks = m.hooks.as_ref().expect("gemini has a [hooks] block");
    // Blocked joins the covered set (the Notification/ToolPermission approval hook), so the
    // coverage now matches claude/codex — every state carried by a hook.
    assert_eq!(
        hooks.covers,
        [
            CoverToken::State(AgentState::Working),
            CoverToken::State(AgentState::Idle),
            CoverToken::State(AgentState::Blocked),
            CoverToken::Lifecycle,
        ]
    );

    // The mapped event set: the six native events plus Notification. BeforeModel/
    // AfterModel stay deliberately unmapped (multi-fire, race the final AfterAgent idle).
    let mut events: Vec<&str> = hooks.map.iter().map(|h| h.event.as_str()).collect();
    events.sort_unstable();
    assert_eq!(
        events,
        [
            "AfterAgent",
            "AfterTool",
            "BeforeAgent",
            "BeforeTool",
            "Notification",
            "SessionEnd",
            "SessionStart",
        ]
    );

    let by_event = |ev: &str| {
        hooks
            .map
            .iter()
            .find(|m| m.event == ev)
            .unwrap_or_else(|| panic!("map entry for {ev}"))
    };
    assert_eq!(
        by_event("SessionStart").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::Start
        }
    );
    assert_eq!(
        by_event("SessionEnd").claim,
        Claim::Lifecycle {
            lifecycle: Lifecycle::End
        }
    );
    let working = Claim::State(StateClaim {
        state: AgentState::Working,
        detail: None,
    });
    for ev in ["BeforeAgent", "BeforeTool", "AfterTool"] {
        assert_eq!(by_event(ev).claim, working, "{ev} ⇒ working");
        assert_eq!(by_event(ev).matcher, None, "{ev} maps unconditionally");
    }
    assert_eq!(
        by_event("AfterAgent").claim,
        Claim::State(StateClaim {
            state: AgentState::Idle,
            detail: None,
        })
    );
    // Notification ⇒ blocked/permission, gated on the ToolPermission notification_type so a future
    // non-permission notification can never false-block.
    let notif = by_event("Notification");
    assert_eq!(notif.matcher.as_deref(), Some("ToolPermission"));
    assert_eq!(
        notif.claim,
        Claim::State(StateClaim {
            state: AgentState::Blocked,
            detail: Some(tma_core::state::Detail::new("permission")),
        })
    );
}

#[test]
fn screen_rules_ship_for_working_and_blocked() {
    let m = manifest();
    // The audit drove working + blocked screens live, so both are capture-visible with real
    // rules. idle now HAS a screen rule too (the composer box's bottom edge), but deliberately
    // stays OUT of `visible`: its chrome overlaps working, so it is not evidence a turn ENDED and
    // must never be allowed to decay an idle hook claim. The rule exists only to give the fold a
    // positive idle claim once the working chrome leaves.
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
        "gemini ships working/blocked screen rules from real captures"
    );
}

// ---- working detected from the real streaming captures ---------------------------

#[test]
fn working_detected_at_wide_and_narrow() {
    // The model-thinking footer (`esc to cancel`) raises a working claim at both widths. Driven
    // live 2026-07-26 in an isolated HOME.
    for name in ["gemini_working_w100.txt", "gemini_working_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "gemini");
        let ev = evaluate(name);
        assert!(
            has_state(&ev, AgentState::Working),
            "{name}: thinking-footer chrome must raise a working claim"
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
    // The shell-approval dialog (`Allow execution of [Shell]?` + `Allow once`) raises a
    // blocked/permission claim at both widths. gemini also hook-covers this (Notification), so the
    // screen rule is the fallback for non-hook-wired panes.
    for name in ["gemini_blocked_w100.txt", "gemini_blocked_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "gemini");
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
        assert!(
            !has_state(&ev, AgentState::Working),
            "{name}: a blocked screen must never read working"
        );
    }
}

// ---- the real idle screen reads idle, and never working/blocked (safety) ---------

#[test]
fn idle_screen_never_reads_working_or_blocked_at_wide_and_narrow() {
    // The real idle composer matches NONE of the working/blocked rules (no `esc to cancel` footer,
    // no approval dialog). It now matches the idle rule, so the engine raises EXACTLY one claim,
    // `idle` — critically it never synthesizes `blocked` from the idle screen (the forbidden
    // direction). This negative regression proves the anchors are state-unique.
    for name in ["gemini_idle_w100.txt", "gemini_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "gemini");
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

// ---- the approval dialog reuses the composer box and must not read idle ----------

#[test]
fn blocked_screen_raises_no_idle_claim() {
    // gemini echoes every prior user message into the transcript inside an IDENTICAL composer box:
    // same `▄`/`▀` half-block frame, same `> ` arrow. So both leaves of a frame-shaped anchor are
    // present on a blocked screen, and the idle rule is safe because of WHERE it looks, not what
    // it looks for. The approval dialog replaces the composer and the status footer outright, so
    // the bottom eight rows carry no box edge at all.
    for name in ["gemini_blocked_w100.txt", "gemini_blocked_w60.txt"] {
        let ev = evaluate(name);
        assert!(
            !has_state(&ev, AgentState::Idle),
            "{name}: the approval dialog must not read as the live composer"
        );
    }

    // Control, and the reason the window must not be widened to `visible`: the SAME leaf scanning
    // the whole visible screen matches BOTH blocked captures, off the transcript echoes. Review
    // gate R-B proposed exactly that unwindowed shape; it would ship a false `idle` on an approval
    // prompt, the forbidden direction under REQUIREMENTS D2.
    let unwindowed = RuleEngine::build(
        &Manifest::parse(
            "min_engine_version = \"0.1\"\n[identity]\nprocess_names=[\"node\"]\n\
             [capture]\nvisible=[\"idle\"]\n\
             [[rules]]\nstate=\"idle\"\nregion=\"visible\"\nmatch={ line_regex='^\\s*▀{10,}\\s*$' }\n",
            "control.toml",
        )
        .expect("control manifest parses"),
    )
    .expect("control manifest compiles");
    for name in ["gemini_blocked_w100.txt", "gemini_blocked_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert!(
            has_state(&unwindowed.evaluate(&snapshot(&fx)), AgentState::Idle),
            "{name}: the leak is real, an unwindowed frame rule matches the transcript echo"
        );
    }
}

// ---- a composer holding a draft still reads idle ---------------------------------

#[test]
fn idle_rule_survives_a_non_empty_composer() {
    // Review gate R-B's functional finding. The first idle anchor was the composer PLACEHOLDER
    // (`Type your message or @path/to/file`), which gemini draws only while the box is EMPTY, so a
    // pane holding a draft lost the anchor and dropped back into the pinned-`working` trap the
    // rule exists to close. The shipped anchor is the box's `▀` bottom edge inside `tail_lines(8)`,
    // which is draft-independent: the box grows UPWARD, so its bottom edge keeps its distance from
    // the status footer no matter how many rows the draft wraps to.
    //
    // The splice turns a real capture into the screen gemini draws with a draft in the box: the
    // placeholder row becomes two draft rows, and one row comes off the top because the terminal
    // scrolls rather than growing. Only the composer content changes.
    for name in ["gemini_idle_w100.txt", "gemini_idle_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        let mut lines: Vec<&str> = fx.capture.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains("Type your message or @path/to/file"))
            .unwrap_or_else(|| panic!("{name}: fixture must carry the empty-composer placeholder"));
        lines[at] = " > refactor the fold ladder so blocked outranks working, then run";
        lines.insert(at + 1, "   the whole suite and report the count");
        lines.remove(0);
        let spliced = format!("{}\n", lines.join("\n"));
        assert!(
            !spliced.contains("Type your message or @path/to/file"),
            "{name}: the splice must really remove the placeholder, or it proves nothing"
        );

        let snap = PaneSnapshot {
            tail_text: spliced,
            ..snapshot(&fx)
        };
        let ev = engine().evaluate(&snap);
        assert!(
            has_state(&ev, AgentState::Idle),
            "{name}: a composer holding a draft must still raise an idle claim"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: a draft must never read blocked"
        );
        assert!(
            !has_state(&ev, AgentState::Working),
            "{name}: a draft must never read working"
        );
    }
}

// ---- the pinned-`working` trap: a finished turn now lands ------------------------

#[test]
fn idle_screen_releases_a_pinned_working_stamp() {
    // The bug batch B closes: with no idle rule the idle screen raised nothing, so every cycle
    // after a turn hit `hold previous` and the pane stayed `working` forever. Now the composer
    // claim lands — once past the working→idle dwell.
    for name in ["gemini_idle_w100.txt", "gemini_idle_w60.txt"] {
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
    // gemini keeps the composer box on screen underneath the thinking footer, so the working
    // screen raises BOTH claims. That is by design (claude's `⏵⏵` precedent); the fold's slot order
    // is what resolves it. If this ever folds to idle, the ladder in `fold.rs` has been reordered.
    for name in ["gemini_working_w100.txt", "gemini_working_w60.txt"] {
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

// ---- the visible-screen clamp keeps scrollback chrome from false-blocking ----------

#[test]
fn prior_turn_approval_chrome_in_scrollback_does_not_false_block() {
    // A short split pane (10 visible rows) whose `capture-pane -S` reaches back into scrollback. A
    // PRIOR turn's approval dialog (`Allow execution of [Shell]?` + `Allow once`) sits in the
    // scrollback portion, above the visible screen; the visible screen itself is an idle composer
    // with none of the blocked anchors. gemini's blocked screen rule scans `Region::Visible`, so
    // the scrollback dialog is out of scope and cannot false-block (the sharp edge).
    let scrollback = "\
✦ This command removes the directory /tmp/x recursively.
│ Allow execution of [Shell]?
│ ● 1. Allow once
(prior turn, now scrolled above the visible screen)";
    let visible = {
        let mut rows = vec!["> Type your message or @path/to/file"];
        rows.extend(vec!["idle composer line"; 9]);
        rows.join("\n")
    };
    let tail = format!("{scrollback}\n{visible}\n");

    let base = PaneSnapshot {
        pane_id: "%0".to_string(),
        pid_tree: vec![],
        title: "Gemini".to_string(),
        tail_text: tail,
        tail_hash: 0,
        alternate_on: true,
        scroll_position: None,
        visible_height: Some(10),
        captured_at: 1_000,
    };

    // Clamped to the 10 visible rows: the scrollback approval dialog is invisible ⇒ no blocked.
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
