//! Acceptance (OpenCode): the bundled OpenCode manifest, tested against redacted
//! real captures of OpenCode 1.17.15 (every bundled rule has a fixture test;
//! evidence-authored, driven live in a scratch tmux server).
//!
//! Gated on the `fixtures` feature (the fixture loader). `cargo test -p tma-core
//! --all-features` runs it.
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

const OPENCODE_TOML: &str = include_str!("../manifests/opencode.toml");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn manifest() -> Manifest {
    Manifest::parse(OPENCODE_TOML, "opencode.toml").expect("bundled manifest parses")
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

fn has_state(ev: &Evaluation, state: AgentState) -> bool {
    ev.evidence
        .iter()
        .any(|e| matches!(&e.claim, Claim::State(s) if s.state == state))
}

/// Rule index in `opencode.toml`, in declaration order.
mod rule {
    pub(crate) const BLOCKED_PERMISSION: usize = 0;
}

fn matched(ev: &Evaluation, index: usize) -> bool {
    ev.reports.iter().any(|r| r.index == index && r.matched)
}

// ---- rule #0: blocked, anchored on the permission-dialog chrome --------------------
//
// One rule covers every OpenCode tool-permission dialog: they share the `Permission
// required` header and the `Allow once` / `Reject` button row. Verified tool-invariant
// against a bash-permission prompt (at w100 and w60) and a file-edit-permission prompt
// (w60), all driven live. The tool-specific subtitle (`# Shell command`, `→ Edit …`) is
// deliberately NOT matched.

fn assert_blocked(name: &str) {
    let ev = evaluate(name);
    assert!(
        matched(&ev, rule::BLOCKED_PERMISSION),
        "{name}: blocked rule must match"
    );
    let blocked = ev
        .evidence
        .iter()
        .find(|e| matches!(&e.claim, Claim::State(s) if s.state == AgentState::Blocked))
        .unwrap_or_else(|| panic!("{name}: blocked evidence"));
    assert_eq!(blocked.source, Source::ScreenRule, "{name}: body chrome");
    if let Claim::State(s) = &blocked.claim {
        assert_eq!(s.detail.as_ref().map(|d| d.as_str()), Some("permission"));
    }
    // The fold's verdict is blocked. OpenCode's OSC title carries no state, so no idle-title
    // evidence competes — the screen rule stands alone.
    let v = fold_verdict(name, None);
    assert_eq!(v.state, AgentState::Blocked, "{name}: verdict blocked");
    assert_eq!(v.detail.as_ref().map(|d| d.as_str()), Some("permission"));
}

#[test]
fn blocked_permission_detected_at_wide_and_narrow() {
    for name in [
        "opencode_blocked_permission_w100.txt",
        "opencode_blocked_permission_w60.txt",
    ] {
        assert_blocked(name);
    }
}

#[test]
fn blocked_edit_permission_is_the_same_rule() {
    // The file-edit permission dialog (`→ Edit notes.txt` subtitle, a diff body) shares the
    // `Permission required` + `Allow once` / `Reject` chrome, so the one blocked rule catches
    // it too — proving the anchor is tool-invariant, not bash-specific.
    assert_blocked("opencode_blocked_edit_w60.txt");
}

// ---- blocked overrides a stale hook working claim (carve-out) -----------------------

#[test]
fn blocked_chrome_overrides_stale_working_hook() {
    // The daemonless value of the capture fallback: a permission prompt stops output, so a
    // pane can sit `working` (last hook) while the screen shows the dialog. `blocked` is
    // capture-visible, so the fold's carve-out lets the fresh blocker chrome override the
    // stale hook claim. captured_at is +10 ahead of the stale hook's evidence_at.
    let name = "opencode_blocked_permission_w100.txt";
    let fx = Fixture::load(&fixtures_dir().join(name)).unwrap();
    let stale_working = StampedState {
        state: AgentState::Working,
        detail: None,
        source: Provenance::Hook,
        evidence_at: fx.captured_at.saturating_sub(5),
        since: fx.captured_at.saturating_sub(5),
        stamped_at: fx.captured_at.saturating_sub(5),
        attention: false,
        notified_at: None,
        hash: None,
        pid: 1,
        session: None,
        subagents: vec![],
    };
    let v = fold_verdict(name, Some(stale_working));
    assert_eq!(
        v.state,
        AgentState::Blocked,
        "blocker chrome wins the carve-out"
    );
}

// ---- negatives: idle / working screens must NOT read as blocked ---------------------

#[test]
fn idle_and_working_screens_do_not_false_block() {
    // Real idle and mid-turn (working) captures. Neither carries the permission-dialog chrome,
    // so the blocked rule must stay silent — blocked-as-a-false-alarm is the defect the
    // three-token anchor guards against. Working/idle themselves are hook-covered, not
    // screen-detected (see the manifest `[capture].visible = ["blocked"]`), so the fold holds
    // the prior state under these captures rather than restating one from the screen.
    for name in [
        "opencode_idle_w100.txt",
        "opencode_idle_w60.txt",
        "opencode_working_w100.txt",
    ] {
        let ev = evaluate(name);
        assert!(
            !matched(&ev, rule::BLOCKED_PERMISSION),
            "{name}: blocked rule must not match a non-dialog screen"
        );
        assert!(
            !has_state(&ev, AgentState::Blocked),
            "{name}: no blocked evidence"
        );
        // A prior idle hook stamp is held, never flipped to blocked by these screens.
        let v = fold_verdict(name, Some(idle_prior(ev_now(name))));
        assert_ne!(v.state, AgentState::Blocked, "{name}: never blocked");
    }
}

fn ev_now(name: &str) -> u64 {
    Fixture::load(&fixtures_dir().join(name))
        .unwrap()
        .captured_at
}

// ---- the manifest as a whole -------------------------------------------------------

#[test]
fn bundled_manifest_declares_expected_hooks_and_coverage() {
    let m = manifest();
    // Homebrew reports the resolved binary comm `opencode.exe`; `opencode` covers other installs.
    assert_eq!(m.identity.process_names, ["opencode.exe", "opencode"]);
    // Only `blocked` is capture-visible — working/idle ride hooks, and the OSC title is static.
    assert_eq!(m.capture.visible, [AgentState::Blocked]);

    let hooks = m.hooks.as_ref().expect("opencode is hook-capable");
    let events: Vec<&str> = hooks.map.iter().map(|h| h.event.as_str()).collect();
    for e in [
        "session-start",
        "user-prompt-submit",
        "stop",
        "permission-required",
    ] {
        assert!(events.contains(&e), "hook map covers {e}");
    }
    // No session-end mapping — OpenCode emits no end event on TUI close; deregistration rides
    // the pid-change / pane-close path, so it must NOT masquerade as a hook lifecycle.
    assert!(
        !events
            .iter()
            .any(|e| e.contains("end") || e.contains("End")),
        "no session-end hook (deregistration is pid-driven)"
    );
    // The permission-required entry maps unconditionally (no matcher): OpenCode has no
    // idle-reminder on that channel, so every fire is a real permission stop.
    let perm = hooks
        .map
        .iter()
        .find(|h| h.event == "permission-required")
        .unwrap();
    assert!(
        perm.matcher.is_none(),
        "permission-required is unconditional"
    );
}
