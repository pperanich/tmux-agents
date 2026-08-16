//! Context-gate integration: the bundled `compact` action (gated `state=["idle"], context_pct_min=75`)
//! becomes fireable once a pane carries a stamped high context, and refuses correctly otherwise. A
//! pane is stamped as an idle `claude` agent with a `@agent_context_pct`; a `claude.toml` carrying a
//! `[telemetry.context]` block is loaded so the agent covers the metric (else the gate would be the
//! permanent `no-coverage`, which the act suite already exercises on the empty manifest dir).
//!
//! Driven through the built binary's `tma act --list --json --pane` (the deck/surface path):
//! the verdict for `compact` is read from the JSON, not inferred.

use std::process::{Command, Output};

use tma_test_support::{empty_config_path, Scratch};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// A claude manifest that declares the context channel, so `covers_context()` is true and a context
/// bound distinguishes `gated` (metric absent/low) from the permanent `no-coverage`.
const CLAUDE_MANIFEST: &str = r#"min_engine_version = "0.1"
[identity]
process_names = ["claude"]
[capture]
visible = []
[telemetry.context]
channel = "event"
format = "claude-statusline-json"
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("ctx_gate");
    s.write_manifest("claude.toml", CLAUDE_MANIFEST);
    s
}

/// Stamp `pane` as an idle claude agent carrying `pct` context (a fresh stamp; `--list` never
/// re-verifies, but keep it realistic).
fn stamp_idle_claude(s: &Scratch, pane: &str, pct: Option<u32>) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_source", "hook");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_pid", "4242");
    match pct {
        Some(p) => {
            s.set_opt(pane, "@agent_context_pct", &p.to_string());
            s.set_opt(pane, "@agent_context_at", &now);
        }
        None => {
            // Leave the gauge absent.
        }
    }
}

fn act_list(s: &Scratch, pane: &str) -> Output {
    Command::new(s.bin())
        .args(["act", "--list", "--json", "--pane", pane])
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMA_CONFIG", empty_config_path())
        .env("XDG_CONFIG_HOME", &s.workdir) // empty user action dir: only bundled actions load
        .output()
        .expect("spawn tma act --list")
}

/// The `(fireable, reason)` verdict for one action in a `--list --json` document (compact, structural
/// scan of the flat JSON: the object between this action's `"name"` and the next action boundary).
fn verdict_for(json: &str, action: &str) -> (bool, String) {
    let needle = format!("\"name\":\"{action}\"");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("action {action:?} not listed in: {json}"));
    // The action's object ends at the next `}` that closes it; the `when`/`agents` sub-structures are
    // before `fireable`/`reason`, so scanning to the next `"name":` (or the array end) bounds it.
    let rest = &json[start..];
    let obj_end = rest[1..]
        .find("\"name\":")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    let obj = &rest[..obj_end];
    let fireable = obj.contains("\"fireable\":true");
    let reason = if let Some(i) = obj.find("\"reason\":\"") {
        let tail = &obj[i + "\"reason\":\"".len()..];
        tail[..tail.find('"').unwrap_or(0)].to_string()
    } else {
        String::new() // null reason (fireable)
    };
    (fireable, reason)
}

#[test]
fn compact_fireable_on_high_context_idle_pane() {
    if !have_tmux() {
        return;
    }
    let s = scratch();
    let pane = s.new_shell_pane();

    // Idle + 80% context (>= the bundled 75 threshold) + covered ⇒ compact fireable.
    stamp_idle_claude(&s, &pane, Some(80));
    let out = act_list(&s, &pane);
    assert_eq!(out.status.code(), Some(0));
    let json = String::from_utf8_lossy(&out.stdout);
    let (fireable, reason) = verdict_for(&json, "compact");
    assert!(
        fireable,
        "compact must be fireable on an idle pane with 80% context: {json}"
    );
    assert_eq!(reason, "", "a fireable action carries a null reason");
}

#[test]
fn compact_gated_below_threshold_and_when_metric_absent() {
    if !have_tmux() {
        return;
    }
    let s = scratch();
    let pane = s.new_shell_pane();

    // Idle + 50% context (< 75) ⇒ gated (covered, but the metric is out of range), not no-coverage.
    stamp_idle_claude(&s, &pane, Some(50));
    let (fireable, reason) = verdict_for(
        &String::from_utf8_lossy(&act_list(&s, &pane).stdout),
        "compact",
    );
    assert!(!fireable, "compact must refuse below the threshold");
    assert_eq!(reason, "gated", "covered-but-low is gated, not no-coverage");

    // The gauge absent (never observed) on a covered agent is still gated (not no-coverage).
    stamp_idle_claude(&s, &pane, None);
    let (fireable, reason) = verdict_for(
        &String::from_utf8_lossy(&act_list(&s, &pane).stdout),
        "compact",
    );
    assert!(!fireable);
    assert_eq!(reason, "gated", "covered-but-absent is gated");
}

#[test]
fn compact_gated_by_state_even_at_high_context() {
    if !have_tmux() {
        return;
    }
    let s = scratch();
    let pane = s.new_shell_pane();

    // Working + 90% context: the state key fails the ANDed gate ⇒ gated (compact is idle-only).
    stamp_idle_claude(&s, &pane, Some(90));
    s.set_opt(&pane, "@agent_state", "working");
    let (fireable, reason) = verdict_for(
        &String::from_utf8_lossy(&act_list(&s, &pane).stdout),
        "compact",
    );
    assert!(!fireable, "compact is idle-only; a working pane is gated");
    assert_eq!(reason, "gated");
}
