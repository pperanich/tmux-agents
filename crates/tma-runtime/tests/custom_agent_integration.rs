//! User-defined agents are a data extension point: a manifest with `[identity]` + `[[hooks.map]]`,
//! dropped into the loaded set, makes `tma event <name> <event>` map and stamp the pane daemonlessly
//! (user manifests ARE the event→state mapping layer).
//!
//! Both tests use a non-shipped agent name (`myagent`) to prove the extension for an agent tma never
//! bundled. We drive `tma event` directly with `--manifest-dir`, which loads exactly that dir as a
//! hermetic closed set, so one test can both map a custom agent and pin the unknown-agent contract
//! without touching the user's real `~/.config/tma/agents`.

use std::process::{Command, Output};

use common::Scratch;
use tma_test_support as common;

/// A minimal custom-agent manifest: identity + three `[[hooks.map]]` entries (a lifecycle register
/// and two state claims), the entire contract a third party writes. Deliberately no `[[rules]]`
/// screen block (hook-only) to show the floor; `[capture]` is present-but-empty (the schema requires
/// the table).
const MYAGENT_MANIFEST: &str = r#"min_engine_version = "0.1"

[identity]
process_names = ["myagent"]

[hooks]
covers = ["working", "idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "Run"
claim = { state = "working" }

[[hooks.map]]
event = "Wait"
claim = { state = "blocked", detail = "permission" }

[capture]
"#;

/// A shared [`Scratch`] with the single custom `myagent.toml` in its `agents/` subdir: the whole
/// closed manifest set these tests drive `tma event` against.
fn scratch() -> Scratch {
    let s = Scratch::new("custom_agent");
    s.write_manifest("myagent.toml", MYAGENT_MANIFEST);
    s
}

/// Fire `tma event --agent <agent> --kind <kind>` against the scratch server + custom manifest dir,
/// with `TMUX_PANE` set and the real config pinned out. `payload` (if any) is passed via a file
/// (`--payload <path>`, no stdin plumbing). Returns the `Output`.
fn event(s: &Scratch, agent: &str, kind: &str, pane: &str, payload: Option<&str>) -> Output {
    let mut cmd = Command::new(common::tma_bin());
    cmd.args(["event", "--agent", agent, "--kind", kind])
        .args(["--socket-name", &s.socket])
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMUX_PANE", pane)
        .env("TMA_CONFIG", common::empty_config_path());
    if let Some(body) = payload {
        let path = s.workdir.join(format!("payload_{kind}"));
        std::fs::write(&path, body).unwrap();
        cmd.arg("--payload").arg(&path);
    }
    cmd.output().expect("spawn tma event")
}

const SESSION: &str = "3f1c8d2e-9a44-4b17-9c0e-2b6a1d7e4f88";

fn payload(event: &str) -> String {
    format!(r#"{{"session_id":"{SESSION}","hook_event_name":"{event}"}}"#)
}

/// (a) The end-to-end story: a custom manifest makes `tma event` direct-stamp the mapped state,
/// daemonlessly (register → idle + session; mapped events → their states). Nothing is special-cased
/// in code; the manifest table alone drives every stamp.
#[test]
fn custom_agent_manifest_maps_events_to_stamps() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    // Boot (lifecycle start) → the pane is now a myagent pane at idle, owner session recorded.
    let out = event(&s, "myagent", "Boot", &pane, Some(&payload("Boot")));
    assert!(out.status.success(), "event must exit 0");
    assert_eq!(s.get(&pane, "#{@agent_state}"), "idle");
    assert_eq!(s.get(&pane, "#{@agent_name}"), "myagent");
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    assert_eq!(s.get(&pane, "#{@agent_session}"), SESSION);

    // Run (state = working) → working.
    event(&s, "myagent", "Run", &pane, Some(&payload("Run")));
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");

    // Wait (state = blocked, detail = permission) → blocked, detail carried through, attention
    // armed on the working→blocked transition, and the window summary rolled up.
    event(&s, "myagent", "Wait", &pane, Some(&payload("Wait")));
    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");
    assert_eq!(s.get(&pane, "#{@agent_detail}"), "permission");
    assert_eq!(s.get(&pane, "#{@agent_attention}"), "1");
    assert_eq!(s.get(&pane, "#{@agent_summary}"), "blocked:1");
}

/// A broken sibling manifest is skipped, not fatal: `myagent.toml` still maps and stamps while
/// `broken.toml` sits in the same directory. Before per-file tolerance one typo in the user's
/// agents dir made every `tma event` a silent no-op, which is the worst possible failure mode for a
/// hook (exit 0, nothing stamped, no clue why).
#[test]
fn a_broken_sibling_manifest_does_not_disarm_the_hook_path() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    s.write_manifest("broken.toml", "min_engine_version = \nnot = = toml\n");
    let pane = s.new_pane();

    let out = event(&s, "myagent", "Run", &pane, Some(&payload("Run")));
    assert!(out.status.success(), "event must exit 0");
    assert!(
        out.stderr.is_empty(),
        "the hook path stays quiet about a skipped manifest: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "the good manifest still maps the event"
    );
}

/// (b) The unknown-agent silent-success contract: an agent with no manifest is a clean no-op (exit
/// 0, no stamp, nothing on stderr). We pre-stamp the pane and confirm the event leaves it
/// byte-for-byte untouched, proving "no stamp", not merely "no crash".
#[test]
fn unknown_agent_is_silent_success_and_stamps_nothing() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    // Seed a distinctive stored state so any stray stamp would be visible as a change.
    assert!(s
        .tmux(&["set-option", "-p", "-t", &pane, "@agent_state", "working"])
        .status
        .success());

    // `ghost` has no manifest in the closed `--manifest-dir` set (only `myagent` does), so the
    // lookup misses and `run` returns SUCCESS without touching tmux.
    let out = event(&s, "ghost", "Run", &pane, Some(&payload("Run")));
    assert!(
        out.status.success(),
        "unknown agent must exit 0 (a hook never fails loudly)"
    );
    assert!(
        out.stderr.is_empty(),
        "unknown agent must be silent on stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    // No stamp: the seeded state is untouched, and none of the fold's companion options appeared.
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "the pre-seeded state must survive an unknown-agent event unchanged"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_name}"),
        "",
        "no @agent_name written for an unmapped agent"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "");
}
