//! Acceptance: what a SessionEnd-class event does to a pane depends on whether the AGENT is gone or
//! only one of its sessions is.
//!
//! The bug this pins: pi's workflow runs open and close sub-sessions inside the live pi process, and
//! each `session_shutdown` removed the pane's whole stamp. The next poll then had no prior stamp and
//! no matching screen rule, so it published the `unknown` floor and the pane sat there until the
//! next hook event — a pi pane reading `unknown` for twenty minutes while its agent worked. Claude's
//! SessionEnd on `/clear` is the same shape.
//!
//! A "live agent" here is a pane running `sleep`: [`Scratch::new_pane`] execs `sleep 100000`, so a
//! manifest naming it in `process_names` makes the pane's tree look to the walk exactly like a
//! running agent's. Its twin names a process nothing runs, which is the agent-exited case.

use common::Scratch;
use tma_test_support as common;

/// A custom manifest whose `process_names` name the pane's own process: the agent is still running
/// when its session ends.
const LIVE: &str = r#"min_engine_version = "0.1"

[identity]
process_names = ["sleep"]

[hooks]
covers = ["working", "idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "Run"
claim = { state = "working" }

[[hooks.map]]
event = "Quit"
claim = { lifecycle = "end" }

[capture]
"#;

/// The same manifest with a process name nothing in the pane runs: the agent really has exited.
const GONE: &str = r#"min_engine_version = "0.1"

[identity]
process_names = ["nosuchagent"]

[hooks]
covers = ["working", "idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "Run"
claim = { state = "working" }

[[hooks.map]]
event = "Quit"
claim = { lifecycle = "end" }

[capture]
"#;

const SESSION: &str = "0c9b6d4a-2f31-4d8e-9a55-71c0e3b8f412";

fn payload(session: &str) -> String {
    format!(r#"{{"session_id":"{session}"}}"#)
}

fn scratch() -> Scratch {
    let s = Scratch::new("deregister");
    s.write_manifest("liveagent.toml", LIVE);
    s.write_manifest("goneagent.toml", GONE);
    s
}

/// The carve-out: the session ends, the agent process is still there, so the state tuple stays and
/// only the session lane is cleared. The pane keeps reading `working` instead of falling to the
/// `unknown` floor, and the next event still lands (the ownership guard is not left frozen by a
/// subagent set with no owner).
#[test]
fn session_end_under_a_live_agent_keeps_the_state_tuple() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    assert!(s
        .event("liveagent", "Boot", &pane, &payload(SESSION))
        .status
        .success());
    assert_eq!(s.get(&pane, "#{@agent_state}"), "idle");
    assert_eq!(s.get(&pane, "#{@agent_session}"), SESSION);

    assert!(s
        .event("liveagent", "Run", &pane, &payload(SESSION))
        .status
        .success());
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");

    // A subagent set recorded by the ending session, and a permission id naming one of its prompts:
    // both must go with it. A set left standing would make `decide` ignore every later event.
    s.set_opt(&pane, "@agent_subagents", "sub-1");
    s.set_opt(&pane, "@agent_permission_request", "perm-1");

    assert!(s
        .event("liveagent", "Quit", &pane, &payload(SESSION))
        .status
        .success());
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "the agent is still running: its state tuple survives its session"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_name}"),
        "liveagent",
        "the pane is still an agent pane"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_session}"),
        "",
        "the ended session is deregistered"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_subagents}"),
        "",
        "the ended session's subagents go with it"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_permission_request}"),
        "",
        "so does the prompt only that session could answer"
    );

    // The pane is not frozen: the next session registers and stamps as usual.
    const NEXT: &str = "1d7c4e02-55aa-4b66-8f19-3c2d90ab7e61";
    assert!(s
        .event("liveagent", "Boot", &pane, &payload(NEXT))
        .status
        .success());
    assert_eq!(s.get(&pane, "#{@agent_session}"), NEXT);
    assert_eq!(s.get(&pane, "#{@agent_state}"), "idle");
}

/// The unchanged case: nothing the manifest would claim is left in the pane's tree, so the agent
/// exited and the whole stamp goes.
#[test]
fn session_end_with_the_agent_gone_removes_the_stamp() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    assert!(s
        .event("goneagent", "Boot", &pane, &payload(SESSION))
        .status
        .success());
    assert!(s
        .event("goneagent", "Run", &pane, &payload(SESSION))
        .status
        .success());
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");

    assert!(s
        .event("goneagent", "Quit", &pane, &payload(SESSION))
        .status
        .success());
    for key in [
        "@agent_state",
        "@agent_name",
        "@agent_session",
        "@agent_source",
    ] {
        assert_eq!(
            s.get(&pane, &format!("#{{{key}}}")),
            "",
            "{key} must be removed when the agent itself is gone"
        );
    }
}
