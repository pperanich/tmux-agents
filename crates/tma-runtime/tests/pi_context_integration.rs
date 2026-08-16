//! pi context telemetry end to end on an isolated scratch `tmux -L` server: a
//! pi-`getContextUsage()`-shaped payload driven through the `tma event --kind context` intake stamps
//! `@agent_context_pct`, read back via `ls --json`; a reading with no usable window stamps no gauge
//! (fail-safe, not wrong), per the no-silent-window rule.

use std::process::Output;

use common::Scratch;
use tma_test_support as common;

const OWNER: &str = "ses_0789d5f61ffeW6yCmb3x7wLH1X";

/// A faux pi manifest: a `Boot` lifecycle registers the pane (recording the owner session), and a
/// `[telemetry.context]` block declares the `pi-context-json` parser. `process_names` matches the
/// pane's `sleep` so the `ls` identity walk keeps the registration.
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
format = "pi-context-json"
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("pi_context");
    s.write_manifest("pifaux.toml", FAUX_MANIFEST);
    s
}

/// Fire the shared `tma event` intake for this suite's agent (`pifaux`). See [`Scratch::event`].
fn event(s: &Scratch, kind: &str, pane: &str, payload: &str) -> Output {
    s.event("pifaux", kind, pane, payload)
}

/// The pi extension's `agent_settled` forward: `{ session_id, context_usage }` from
/// `ctx.getContextUsage()`.
fn usage(session: &str, tokens: u32, window: u32, percent: u32) -> String {
    format!(
        r#"{{"session_id":"{session}","context_usage":{{"tokens":{tokens},"contextWindow":{window},"percent":{percent}}}}}"#
    )
}

#[test]
fn pi_context_stamps_through_intake_and_reads_back_via_ls_json() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    let boot = event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));
    assert!(boot.status.success());
    assert_eq!(s.get(&pane, "#{@agent_session}"), OWNER);

    let out = event(&s, "context", &pane, &usage(OWNER, 124_000, 200_000, 62));
    assert!(out.status.success(), "context intake must exit 0");
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "62",
        "pi's precomputed percent is stamped"
    );

    let json = s.ls_json();
    assert!(
        json.contains("\"context\":62"),
        "ls --json exposes the pi gauge: {json}"
    );
}

#[test]
fn pi_unknown_window_stamps_no_gauge() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));

    // A reading with a token count but no usable window: the no-silent-window rule forbids guessing,
    // so no gauge is stamped (fail-safe, not wrong).
    let out = event(
        &s,
        "context",
        &pane,
        &format!(
            r#"{{"session_id":"{OWNER}","context_usage":{{"tokens":150000,"percent":null}}}}"#
        ),
    );
    assert!(out.status.success(), "context intake must exit 0 on a miss");
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "",
        "an unknown-window reading leaves the gauge absent"
    );
    let json = s.ls_json();
    assert!(
        json.contains("\"context\":null"),
        "ls --json shows no gauge for the uncovered reading: {json}"
    );
}
