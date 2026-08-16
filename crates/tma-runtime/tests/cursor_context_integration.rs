//! Cursor context telemetry end to end on an isolated scratch `tmux -L` server: a
//! Cursor-statusLine-shaped payload driven through the `tma event --kind context` intake stamps
//! `@agent_context_pct`, read back via `ls --json`; a later payload that has lost its `context_window`
//! object leaves the stored gauge in place (IGNORE, not a clear), the fail-safe for the highest-churn
//! channel of the batch.

use std::process::Output;

use common::Scratch;
use tma_test_support as common;

const OWNER: &str = "3f1c8d2e-9a44-4b17-9c0e-2b6a1d7e4f88";

/// A faux cursor manifest: a `Boot` lifecycle registers the pane (recording the owner session), and a
/// `[telemetry.context]` block declares the `cursor-statusline-json` parser. `process_names` matches
/// the pane's `sleep` so the `ls` identity walk keeps the registration.
const FAUX_MANIFEST: &str = r#"min_engine_version = "0.1"

[identity]
process_names = ["sleep"]

[hooks]
covers = ["idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[capture]

[telemetry.context]
channel = "event"
format = "cursor-statusline-json"
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("cursor_context");
    s.write_manifest("cursorfaux.toml", FAUX_MANIFEST);
    s
}

/// Fire the shared `tma event` intake for this suite's agent (`cursorfaux`). See [`Scratch::event`].
fn event(s: &Scratch, kind: &str, pane: &str, payload: &str) -> Output {
    s.event("cursorfaux", kind, pane, payload)
}

/// A Cursor statusLine-shaped payload: a `context_window` object with `total_input_tokens` and
/// `context_window_size`, plus the owning `session_id`.
fn statusline(session: &str, tokens: u32, window: u32) -> String {
    format!(
        r#"{{"session_id":"{session}","context_window":{{"total_input_tokens":{tokens},"context_window_size":{window}}}}}"#
    )
}

#[test]
fn cursor_context_stamps_through_intake_and_reads_back_via_ls_json() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    let boot = event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));
    assert!(boot.status.success());
    assert_eq!(s.get(&pane, "#{@agent_session}"), OWNER);

    // 130000 / 200000 = 65%.
    let out = event(&s, "context", &pane, &statusline(OWNER, 130_000, 200_000));
    assert!(out.status.success(), "context intake must exit 0");
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "65",
        "the cursor gauge is computed from the token counts"
    );

    let json = s.ls_json();
    assert!(
        json.contains("\"context\":65"),
        "ls --json exposes the cursor gauge: {json}"
    );
}

#[test]
fn cursor_missing_context_window_leaves_the_gauge_untouched() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));

    // A real reading stamps 65, then a payload that has lost its `context_window` arrives.
    event(&s, "context", &pane, &statusline(OWNER, 130_000, 200_000));
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "65");

    let out = event(
        &s,
        "context",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}","model":"claude-4.5-sonnet"}}"#),
    );
    assert!(out.status.success(), "context intake must exit 0 on a miss");
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "65",
        "a missing context_window is ignored, not a clear — the live gauge stays"
    );
}
