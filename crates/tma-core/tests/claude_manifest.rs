//! Acceptance: the bundled Claude Code manifest, tested against redacted real captures
//! (every bundled rule has ≥1 fixture test; evidence-authored).
//!
//! Gated on the `fixtures` feature (the fixture loader). `mise run test` runs with
//! `--all-features`, so this always runs in CI-equivalent runs.
#![cfg(feature = "fixtures")]

use std::path::{Path, PathBuf};

use tma_core::evidence::Claim;
use tma_core::fixture::Fixture;
use tma_core::snapshot::PaneSnapshot;
use tma_core::stamp::StampedState;
use tma_core::{
    verdict, AgentState, Evaluation, FoldConfig, Manifest, Provenance, RuleEngine, SnapshotFacts,
    Source, Verdict,
};

const CLAUDE_TOML: &str = include_str!("../manifests/claude.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(CLAUDE_TOML, "claude.toml").expect("bundled manifest parses")
}

fn engine() -> RuleEngine {
    RuleEngine::build(&manifest()).expect("bundled manifest regexes compile")
}

/// Run the full detector (engine + fold) on a fixture, as a live producer would — this is
/// what actually reaches the store, so the "never idle while blocked" guarantees are
/// verified here rather than at the raw-evidence level (titles legitimately keep `✳` during
/// a permission prompt; blocker chrome overrides it in the fold).
fn fold_verdict(name: &str, prev: Option<StampedState>) -> Verdict {
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
        snap.captured_at + 10,
    )
}

fn idle_prior(now: u64) -> StampedState {
    StampedState {
        state: AgentState::Idle,
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

/// Rule index in `claude.toml`, in declaration order — the map the fixture tests use.
/// New rules are APPENDED, never inserted: these indices are hardcoded across the suite.
mod rule {
    pub(crate) const BLOCKED_PERMISSION: usize = 0;
    pub(crate) const HISTORY_OVERLAY: usize = 1;
    pub(crate) const WORKING_TITLE: usize = 2;
    pub(crate) const IDLE_TITLE: usize = 3;
    pub(crate) const IDLE_MODE_LINE: usize = 4;
    pub(crate) const BLOCKED_PLAN: usize = 5;
    pub(crate) const BLOCKED_TRUST: usize = 6;
}

fn matched(ev: &Evaluation, index: usize) -> bool {
    ev.reports.iter().any(|r| r.index == index && r.matched)
}

// ---- rule #0: blocked, anchored on the `❯ 1. Yes` selection chrome -----------------
//
// One rule covers every blocking dialog Claude shows, because they share the same
// invariant: the selection cursor parked on the affirmative option. Verified against
// four real captures — the write-permission prompt and the first-run trust dialog, each
// at two pane widths. Free-text question forms are intentionally NOT matched;
// `no_false_blocked_from_conversation_text` guards that.

/// Assert a blocked fixture is detected as blocked both at the rule level (body chrome) and at the
/// fold level, resolving to `detail`. Every blocked dialog still matches the generic `❯ 1. Yes`
/// rule; `detail` is what the priority contest between it and the variant rules settles on.
fn assert_blocked_detail(name: &str, detail: &str) {
    let ev = evaluate(name);
    assert!(
        matched(&ev, rule::BLOCKED_PERMISSION),
        "{name}: generic blocked rule still matches the option-1 line"
    );
    let blocked = ev
        .evidence
        .iter()
        .find(|e| matches!(&e.claim, Claim::State(s) if s.state == AgentState::Blocked))
        .unwrap_or_else(|| panic!("{name}: blocked evidence"));
    assert_eq!(blocked.source, Source::ScreenRule, "{name}: body chrome");
    if let Claim::State(s) = &blocked.claim {
        assert_eq!(
            s.detail.as_ref().map(|d| d.as_str()),
            Some(detail),
            "{name}: evidence detail"
        );
    }
    // The fold's verdict must be blocked even though the OSC title can keep `✳`
    // (idle-title evidence coexists; blocker chrome overrides it).
    let v = fold_verdict(name, None);
    assert_eq!(v.state, AgentState::Blocked, "{name}: verdict blocked");
    assert_eq!(
        v.detail.as_ref().map(|d| d.as_str()),
        Some(detail),
        "{name}: verdict detail"
    );
}

/// A-502. The permission dialog is the case the split exists to preserve: it must keep folding to
/// `permission` at every captured width, including the second, independently-captured Bash-tool
/// dialog from the gap-fill round (claude 2.1.236).
#[test]
fn blocked_permission_detected_at_wide_and_narrow() {
    for name in [
        "claude_blocked_permission_w100.txt",
        "claude_blocked_permission_w60.txt",
        "claude_blocked_permission_bash_w60.txt",
    ] {
        assert_blocked_detail(name, "permission");
        let ev = evaluate(name);
        assert!(
            !matched(&ev, rule::BLOCKED_PLAN) && !matched(&ev, rule::BLOCKED_TRUST),
            "{name}: neither variant rule steals the permission dialog"
        );
    }
}

/// A-501. **This test changed with the plan/trust split, and the old assertion was the bug.**
/// It previously asserted `detail == "permission"` on the first-run trust gate, which is what made
/// `tma act approve` fire `1` — "Yes, I trust this folder", a whole-folder grant — at it.
/// Detection still rides the `❯ 1. Yes, I trust this folder` selection chrome, not the question
/// text (which renders above the tail window and wraps at narrow widths); only the stamped detail
/// moved.
#[test]
fn blocked_trust_dialog_detected_at_wide_and_narrow() {
    for name in [
        "claude_blocked_trust_w200.txt",
        "claude_blocked_trust_w100.txt",
        "claude_blocked_trust_w60.txt",
    ] {
        assert_blocked_detail(name, "trust");
        assert!(
            matched(&evaluate(name), rule::BLOCKED_TRUST),
            "{name}: trust variant rule matched"
        );
    }
}

/// A-500. The plan-approval dialog, whose option 1 is `Yes, and use auto mode` — it does not
/// approve the action in front of the user, it switches the session into auto-approving everything
/// that follows. Captured at the narrowest and widest widths in the corpus; the option-1 line is
/// identical at both, so the match is structural rather than a wrap artefact.
#[test]
fn blocked_plan_dialog_detected_at_wide_and_narrow() {
    for name in [
        "claude_blocked_plan_w200.txt",
        "claude_blocked_plan_w60.txt",
    ] {
        assert_blocked_detail(name, "plan");
        assert!(
            matched(&evaluate(name), rule::BLOCKED_PLAN),
            "{name}: plan variant rule matched"
        );
    }
}

/// A-503. The variant rules win by PRIORITY, not by declaration order: both they and the generic
/// permission rule match the same line, and 110 > 100 is the only thing that decides the detail.
/// Equalise the priorities and the engine falls back to lowest-index-wins, which is the generic
/// rule — the pre-fix behaviour.
#[test]
fn plan_and_trust_outrank_the_generic_permission_rule() {
    let m = manifest();
    assert_eq!(
        m.rules.len(),
        7,
        "the rule-index map in `mod rule` is positional; a rule was inserted or removed"
    );
    let generic = &m.rules[rule::BLOCKED_PERMISSION];
    assert_eq!(generic.priority, 100);
    assert_eq!(
        generic.detail.as_ref().map(|d| d.as_str()),
        Some("permission")
    );
    for (index, detail) in [(rule::BLOCKED_PLAN, "plan"), (rule::BLOCKED_TRUST, "trust")] {
        let variant = &m.rules[index];
        assert_eq!(variant.state, AgentState::Blocked);
        assert_eq!(variant.detail.as_ref().map(|d| d.as_str()), Some(detail));
        assert!(
            variant.priority > generic.priority,
            "{detail} at {} must outrank permission at {}",
            variant.priority,
            generic.priority
        );
        // Same region, so the discriminator is always inside the window the generic rule read.
        assert_eq!(variant.region, generic.region, "{detail}: same region");
    }
}

/// The plan dialog carries the same title-coexistence property the permission fixtures pin: a live
/// claude pane keeps `✳` in its OSC title while a dialog is up, and the fold must still land
/// `blocked/plan`. The gap-fill corpus recorded capture bodies only, so the title here is authored
/// onto the real capture rather than replayed from it.
#[test]
fn plan_verdict_survives_an_idle_marker_in_the_title() {
    let fx = Fixture::load(&fixtures_dir().join("claude_blocked_plan_w60.txt")).unwrap();
    let snap = PaneSnapshot {
        title: "✳ Add hello to probe.txt".to_string(),
        ..snapshot(&fx)
    };
    let ev = engine().evaluate(&snap);
    assert!(has_state(&ev, AgentState::Idle), "the ✳ title claims idle");
    let facts = SnapshotFacts {
        pid: 1,
        foreground_is_agent: true,
        scrolled: false,
        history_view: ev.history_view,
    };
    let v = verdict(
        None,
        &facts,
        &ev.evidence,
        &manifest(),
        &FoldConfig::default(),
        snap.captured_at + 10,
    );
    assert_eq!(
        v.state,
        AgentState::Blocked,
        "blocker chrome overrides idle"
    );
    assert_eq!(v.detail.as_ref().map(|d| d.as_str()), Some("plan"));
}

/// A-504 / A-505 end to end, at the only level that proves the fix: a **real capture** is folded to
/// a verdict and that verdict is fed to the shipped `approve` gate. This is the whole chain the bug
/// travelled down — screen → rule → fold → gate → `send-keys "1"` — with only the final tmux write
/// stubbed out.
///
/// `approve_refuses_at_the_plan_and_trust_dialogs` in `bundled_actions.rs` asserts the gate half
/// with a hand-written `detail`, so it stays green even if the manifest regresses. This one does
/// not: revert the two variant rules and the plan capture folds to `permission` again and the gate
/// returns `Fireable`.
#[test]
fn approve_and_deny_refuse_at_a_real_plan_or_trust_capture() {
    use tma_core::action::{ContextKeys, GateInput, GateOutcome, RefusalReason};
    use tma_core::ActionManifest;

    let approve = ActionManifest::parse(
        include_str!("../actions/approve.toml"),
        "approve",
        "approve.toml",
    )
    .unwrap();
    let deny =
        ActionManifest::parse(include_str!("../actions/deny.toml"), "deny", "deny.toml").unwrap();

    for name in [
        "claude_blocked_plan_w60.txt",
        "claude_blocked_plan_w200.txt",
        "claude_blocked_trust_w60.txt",
        "claude_blocked_trust_w200.txt",
    ] {
        let v = fold_verdict(name, None);
        assert_eq!(v.state, AgentState::Blocked, "{name}: folds to blocked");
        let detail = v.detail.as_ref().map(|d| d.as_str());
        let input = GateInput {
            agent: "claude",
            state: v.state,
            detail,
            context_pct: None,
            context_covered: false,
            context_keys: ContextKeys::default(),
        };
        assert_eq!(
            approve.evaluate_gate(&input),
            GateOutcome::Refused(RefusalReason::Gated),
            "{name}: approve resolved at detail {detail:?} — it would send `1`"
        );
        // `deny.toml` is gated identically, so its Escape stops firing here too. On the trust gate
        // that Escape was a correct, safe cancel: a deliberate, documented regression in
        // convenience, with per-detail deny rules scheduled separately.
        assert_eq!(
            deny.evaluate_gate(&input),
            GateOutcome::Refused(RefusalReason::Gated),
            "{name}: deny is gated identically to approve"
        );
    }

    // The control: the dialog approve exists for still resolves, from a real capture.
    let v = fold_verdict("claude_blocked_permission_bash_w60.txt", None);
    assert_eq!(
        approve.evaluate_gate(&GateInput {
            agent: "claude",
            state: v.state,
            detail: v.detail.as_ref().map(|d| d.as_str()),
            context_pct: None,
            context_covered: false,
            context_keys: ContextKeys::default(),
        }),
        GateOutcome::Fireable,
        "the permission dialog must still be approvable"
    );
}

// ---- rule #1: history overlay (skip_state_update) ----------------------------------

#[test]
fn model_picker_is_history_view() {
    let ev = evaluate("claude_history_model_picker.txt");
    assert!(matched(&ev, rule::HISTORY_OVERLAY), "history rule matched");
    assert!(ev.history_view, "history_view flag set (freeze)");
    // An overlay freezes the prior state — a prior `idle` is held, not restated.
    let now = 1_000_000;
    let v = fold_verdict("claude_history_model_picker.txt", Some(idle_prior(now)));
    assert_eq!(v.state, AgentState::Idle, "prior held under history view");
    assert_eq!(v.writes.action, tma_core::WriteAction::Hold);
}

#[test]
fn transcript_viewer_is_history_view_at_wide_and_narrow() {
    // The detailed-transcript overlay (ctrl+o), captured live at two widths. Its footer
    // `Showing detailed transcript` is the same HISTORY_OVERLAY rule's third alternative, so
    // the on-screen scrollback (which can contain past blocker/idle chrome) freezes the prior
    // state rather than restating it.
    for name in ["claude_transcript_w100.txt", "claude_transcript_w60.txt"] {
        let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
        assert_eq!(fx.agent, "claude");
        let ev = evaluate(name);
        assert!(matched(&ev, rule::HISTORY_OVERLAY), "{name}: history rule");
        assert!(ev.history_view, "{name}: history_view flag set (freeze)");
        // A prior blocked stamp is HELD under the overlay, never flipped, and the overlay
        // never itself raises blocked (the forbidden direction).
        let now = 1_000_000;
        let v = fold_verdict(name, Some(idle_prior(now)));
        assert_eq!(
            v.state,
            AgentState::Idle,
            "{name}: prior held under overlay"
        );
        assert_ne!(v.state, AgentState::Blocked, "{name}: overlay never blocks");
    }
}

// ---- rule #2: working (braille title) ----------------------------------------------

#[test]
fn working_detected_from_braille_title() {
    let ev = evaluate("claude_working_title.txt");
    assert!(
        matched(&ev, rule::WORKING_TITLE),
        "working title rule matched"
    );
    let working = ev
        .evidence
        .iter()
        .find(|e| matches!(&e.claim, Claim::State(s) if s.state == AgentState::Working))
        .expect("working evidence");
    assert_eq!(working.source, Source::Title);
}

// ---- rules #3 + #4: idle (title marker + mode-line fallback) ------------------------

#[test]
fn idle_detected_from_title_and_mode_line() {
    // These two carry the `⏸` (manual mode) form of the mode line.
    for name in ["claude_idle_completion.txt", "claude_idle_w60.txt"] {
        let ev = evaluate(name);
        assert!(matched(&ev, rule::IDLE_TITLE), "{name}: idle title rule");
        assert!(
            matched(&ev, rule::IDLE_MODE_LINE),
            "{name}: idle mode-line rule"
        );
        assert!(has_state(&ev, AgentState::Idle), "{name}: idle evidence");
    }
}

#[test]
fn idle_detected_from_bypass_mode_line() {
    // Backs the `⏵⏵` arm of the idle mode-line rule (the other arm, `⏸`, is covered
    // above). A real capture of an idle pane in bypass-permissions mode: the mode line
    // reads `⏵⏵ bypass permissions on (shift+tab to cycle)`. It must read idle, and its
    // `✻ Cooked for 4m 0s` completion line must not be misread as a working spinner.
    let name = "claude_idle_bypass_modeline.txt";
    let ev = evaluate(name);
    assert!(matched(&ev, rule::IDLE_TITLE), "{name}: idle title rule");
    assert!(
        matched(&ev, rule::IDLE_MODE_LINE),
        "{name}: idle mode-line rule (⏵⏵)"
    );
    assert!(has_state(&ev, AgentState::Idle), "{name}: idle evidence");
    assert!(
        !has_state(&ev, AgentState::Working),
        "{name}: completion line is not working"
    );
    assert!(
        !matched(&ev, rule::BLOCKED_PERMISSION),
        "{name}: idle prompt (`❯ Use the …`) is not a selection menu"
    );
}

// ---- negative: the "Sautéed…" completion graveyard ---------------------------------

#[test]
fn past_tense_completion_line_is_not_a_working_spinner() {
    // The idle fixture carries `✻ Cooked for 5s` / `✻ Churned for 3m 30s` completion
    // lines. ta's false-positive graveyard: these are NOT spinners. Detection is
    // title-braille only, so no working claim may appear.
    let fx = Fixture::load(&fixtures_dir().join("claude_idle_completion.txt")).unwrap();
    assert!(
        fx.capture.contains("Cooked for") || fx.capture.contains("Churned for"),
        "fixture actually contains a completion line"
    );
    let ev = engine().evaluate(&snapshot(&fx));
    assert!(
        !has_state(&ev, AgentState::Working),
        "completion line is not working"
    );
    assert!(
        !matched(&ev, rule::WORKING_TITLE),
        "braille-title rule stays silent"
    );
}

// ---- negative: free-text question / prose "1. Yes" must NOT false-block ----

#[test]
fn no_false_blocked_from_conversation_text() {
    // A realistic idle screen whose last turn happens to contain the exact traps the old
    // loose blocked arm matched: a "Do you want to …?" question in prose, a numbered
    // "1. Yes …" list line WITHOUT the selection cursor, and a past-tense "✻ Sautéed …"
    // completion line (the false-positive graveyard). None is the invariant
    // `❯ 1. Yes` selection chrome, so blocked must not fire — blocked-as-a-false-alarm is
    // exactly the defect this rule was tightened to remove. Tail is synthetic
    // authored-from-evidence content: styling is irrelevant, so it is written as the
    // post-strip text the engine matches on.
    let tail = "\
⏺ I compared the two approaches for the retry helper.

  Do you want to keep the shared helper, or should I inline it? Both work; inlining
  removes a file but duplicates the backoff math across two call sites.

  1. Yes, inline it — fewer files
  2. No, keep the shared helper

✻ Sautéed for 5m 34s

❯ let's keep the shared helper for now
────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";
    let snap = PaneSnapshot {
        pane_id: "%0".to_string(),
        pid_tree: vec![],
        title: "✳ Compare retry-helper approaches".to_string(),
        tail_text: tail.to_string(),
        tail_hash: 0,
        alternate_on: true,
        scroll_position: None,
        visible_height: None,
        captured_at: 2_000_000,
    };
    let ev = engine().evaluate(&snap);
    assert!(
        !matched(&ev, rule::BLOCKED_PERMISSION),
        "conversation text (question / prose `1. Yes` / `Sautéed`) must not blocked"
    );
    assert!(
        !has_state(&ev, AgentState::Blocked),
        "no blocked evidence from conversation text"
    );
    // It is a live idle screen, and must still read as idle (mode line + title).
    assert!(
        matched(&ev, rule::IDLE_MODE_LINE),
        "still idle via mode line"
    );
    assert!(has_state(&ev, AgentState::Idle), "idle evidence present");
}

// ---- the manifest as a whole -------------------------------------------------------

#[test]
fn bundled_manifest_declares_expected_hooks_and_coverage() {
    let m = Manifest::parse(CLAUDE_TOML, "claude.toml").unwrap();
    assert_eq!(m.identity.process_names, ["claude"]);
    assert_eq!(
        m.capture.visible,
        [AgentState::Working, AgentState::Idle, AgentState::Blocked]
    );
    let hooks = m.hooks.as_ref().expect("claude is hook-capable");
    let events: Vec<&str> = hooks.map.iter().map(|h| h.event.as_str()).collect();
    for e in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Notification",
        "Stop",
    ] {
        assert!(events.contains(&e), "hook map covers {e}");
    }
    // SubagentStop is intentionally not a claim (DAEMON.md: bookkeeping only).
    assert!(
        !events.contains(&"SubagentStop"),
        "SubagentStop is not mapped"
    );
    // The Notification permission matcher.
    let notif = hooks
        .map
        .iter()
        .find(|h| h.event == "Notification")
        .unwrap();
    assert_eq!(
        notif.matcher.as_deref(),
        Some("permission_prompt|elicitation_dialog")
    );
}
