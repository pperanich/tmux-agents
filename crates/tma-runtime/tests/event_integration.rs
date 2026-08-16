//! Acceptance: the `tma-hook` wrapper + `tma event` bridge, end to end on a scratch tmux server.
//!
//! We fire the real `tma-hook` wrapper as a subprocess with a Claude hook's environment (`TMUX_PANE`
//! at a scratch pane, `TMA_HOOK_SOCKET` pinning the scratch `-L` server), so no default-server pane
//! is touched. The scratch server is `-f /dev/null` isolated and killed on drop.

use std::path::PathBuf;
use std::process::Command;

use common::Scratch;
use tma_test_support as common;

/// Path to the `tma-hook` wrapper, copied into the scratch workdir and made executable. This is the
/// only suite that drives the shell wrapper end to end.
fn wrapper(s: &Scratch) -> PathBuf {
    // The `tma-hook` wrapper is a bin-crate asset; this test moved to `tma-runtime`,
    // so reach back into the bin crate's `assets/` (a `../` bump forced by the move).
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../tma/assets/tma-hook");
    let dst = s.workdir.join("tma-hook");
    std::fs::copy(src, &dst).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dst).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dst, perms).unwrap();
    }
    dst
}

/// Fire the wrapper as a hook would: `tma-hook <agent> <event>` with `payload` on stdin,
/// `TMUX_PANE` set, and the scratch socket pinned. Notify opt-in is toggled by `notify`.
fn fire(s: &Scratch, agent: &str, event: &str, pane: &str, payload: &str, notify: bool) {
    use std::io::Write;
    let mut child = Command::new(wrapper(s))
        .arg(agent)
        .arg(event)
        .env("TMUX_PANE", pane)
        .env("TMA_HOOK_SOCKET", &s.socket)
        .env("TMA_BIN", common::tma_bin())
        // The wrapper passes its env through to the inner `tma event`; pin the config to the
        // empty default so that process never reads the real `~/.config/tma/config.toml`.
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_NOTIFY_FROM_EVENT", if notify { "1" } else { "0" })
        // The wrapper forwards no manifest flag, so `tma event` uses the bundled claude
        // manifest — which carries the hook table this test exercises.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wrapper");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let status = child.wait().expect("wait wrapper");
    assert!(status.success(), "wrapper must always exit 0");
}

/// Fire like [`fire`] but wire an instrumented `TMA_NOTIFY_CMD` sink (a shell line appending to a
/// file per fire), so a test can assert whether the daemonless direct-fire actually fired.
fn fire_sink(s: &Scratch, agent: &str, event: &str, pane: &str, payload: &str, sink_cmd: &str) {
    use std::io::Write;
    let mut child = Command::new(wrapper(s))
        .arg(agent)
        .arg(event)
        .env("TMUX_PANE", pane)
        .env("TMA_HOOK_SOCKET", &s.socket)
        .env("TMA_BIN", common::tma_bin())
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_NOTIFY_FROM_EVENT", "1")
        .env("TMA_NOTIFY_CMD", sink_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wrapper");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let status = child.wait().expect("wait wrapper");
    assert!(status.success(), "wrapper must always exit 0");
}

/// Fire the wrapper the way Codex's `notify` does: the payload is the third argv
/// (`tma-hook codex notify <JSON>`), not stdin. Proves the wrapper's argv→stdin forwarding.
fn fire_argv(s: &Scratch, agent: &str, event: &str, pane: &str, payload_arg: &str) {
    let mut child = Command::new(wrapper(s))
        .arg(agent)
        .arg(event)
        .arg(payload_arg)
        .env("TMUX_PANE", pane)
        .env("TMA_HOOK_SOCKET", &s.socket)
        .env("TMA_BIN", common::tma_bin())
        .env("TMA_CONFIG", common::empty_config_path())
        // Deliberately feed unrelated bytes on stdin: the wrapper must ignore them when the
        // argv payload is present, so this must not leak into the stamp.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wrapper");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"STDIN-SHOULD-BE-IGNORED")
        .unwrap();
    let status = child.wait().expect("wait wrapper");
    assert!(status.success(), "wrapper must always exit 0");
}

const SESSION: &str = "65ced290-2a08-43de-aa80-d0b049d7ce30";

fn payload(event: &str, session: &str) -> String {
    format!(r#"{{"session_id":"{session}","hook_event_name":"{event}"}}"#)
}

fn notification_permission(session: &str) -> String {
    format!(
        r#"{{"session_id":"{session}","hook_event_name":"Notification","notification_type":"permission_prompt","message":"needs permission"}}"#
    )
}

#[test]
fn wrapper_bridges_hook_events_to_stamps_and_dedups() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    // A pane whose process name is `claude`, so the bundled manifest identity matches if the
    // fold ever runs — but `tma event` maps purely by the manifest hook table, so any pane
    // works. Use a plain sleep pane and address it by id.
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");
    assert!(pane.starts_with('%'), "got pane {pane:?}");

    // SessionStart → registered as an agent pane at idle, session recorded.
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane,
        &payload("SessionStart", SESSION),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "idle");
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    assert_eq!(s.get(&pane, "#{@agent_session}"), SESSION);
    assert_eq!(s.get(&pane, "#{@agent_name}"), "claude");

    // UserPromptSubmit → working.
    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", SESSION),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");
    let since_working = s.get(&pane, "#{@agent_since}");

    // Notification (permission) with notify opt-in → blocked, attention set, marker written.
    fire(
        &s,
        "claude",
        "Notification",
        &pane,
        &notification_permission(SESSION),
        true,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");
    assert_eq!(s.get(&pane, "#{@agent_detail}"), "permission");
    assert_eq!(s.get(&pane, "#{@agent_attention}"), "1");
    let notified_1 = s.get(&pane, "#{@agent_notified_at}");
    let since_blocked = s.get(&pane, "#{@agent_since}");
    // (working→blocked is a real transition, but both fire in the same wall-clock second in
    // this test, so `since` may share the epoch — we assert the dedup no-op below instead.)
    let _ = since_working;
    assert!(
        !notified_1.is_empty(),
        "notify marker written before firing"
    );

    // Window summary rolled up (this window has one blocked agent).
    assert_eq!(s.get(&pane, "#{@agent_summary}"), "blocked:1");

    // Dedup: a second identical Notification no-ops — no attention re-arm, no marker bump,
    // no `since` change (the episode continues).
    fire(
        &s,
        "claude",
        "Notification",
        &pane,
        &notification_permission(SESSION),
        true,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        notified_1,
        "no double notify-marker bump on a repeat blocked event"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_since}"),
        since_blocked,
        "since is write-once within the blocked episode"
    );

    // SessionEnd → all @agent_* options removed.
    fire(
        &s,
        "claude",
        "SessionEnd",
        &pane,
        &payload("SessionEnd", SESSION),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "", "state option removed");
    assert_eq!(s.get(&pane, "#{@agent_session}"), "");
}

/// A daemonless event that loses arbitration (a strictly-newer hook claim is stored) must commit
/// none of its companions and must not fire. Staged deterministically: pre-seed a stored claim whose
/// `@agent_evidence_at` sits far in the future, so any real-clock event is strictly older and the
/// `HookArbitrate` guard suppresses its whole chain.
#[test]
fn losing_arbitration_event_neither_stamps_nor_fires() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Pre-seed a strictly-newer stored hook claim (working, evidence far in the future), with no
    // notification marker yet — so `decide` sees an un-notified, notifiable blocked transition and
    // would fire, were it not for arbitration.
    let future = "9999999999999";
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_evidence_at", future);
    s.set_opt(&pane, "@agent_since", "1000");
    s.set_opt(&pane, "@agent_stamped_at", future);
    s.set_opt(&pane, "@agent_pid", "4242");
    s.set_opt(&pane, "@agent_name", "claude");

    let sink = s.workdir.join("sink");
    let sink_cmd = format!("printf 'fire\\n' >> {}", sink.display());
    // A blocked/permission event with notify opt-in: it loses arbitration (older than the stored
    // future evidence), so the guarded chain and the read-back-gated fire both no-op.
    fire_sink(
        &s,
        "claude",
        "Notification",
        &pane,
        &notification_permission(SESSION),
        &sink_cmd,
    );

    // The newer stored claim survived untouched — the losing event clobbered nothing.
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "the strictly-newer hook claim wins arbitration; state held"
    );
    assert_eq!(s.get(&pane, "#{@agent_evidence_at}"), future);
    assert_eq!(s.get(&pane, "#{@agent_since}"), "1000");
    // The notify marker was NOT committed to the losing event's time (guarded companion held).
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        "",
        "a losing event must not commit the write-before-fire marker"
    );
    // And no notification fired (read-back gate: the marker did not commit).
    assert!(
        !sink.exists() || std::fs::read_to_string(&sink).unwrap().trim().is_empty(),
        "a losing event must not fire (at-most-once)"
    );
}

/// Positive control for [`losing_arbitration_event_neither_stamps_nor_fires`]: the same shape but
/// with the pre-seeded evidence in the PAST, so the real-clock event is newer, wins, and the guarded
/// chain commits + fires. Without this the negative test could pass vacuously.
#[test]
fn winning_arbitration_event_stamps_and_fires() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Same seed as the negative test, but with the stored evidence strictly in the PAST (`past`
    // instead of the far-future `9999999999999`), and still no notification marker — so the
    // incoming real-clock Notification is newer, wins arbitration, and both stamps and fires.
    let past = "1000";
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_evidence_at", past);
    s.set_opt(&pane, "@agent_since", past);
    s.set_opt(&pane, "@agent_stamped_at", past);
    s.set_opt(&pane, "@agent_pid", "4242");
    s.set_opt(&pane, "@agent_name", "claude");

    let sink = s.workdir.join("sink");
    let sink_cmd = format!("printf 'fire\\n' >> {}", sink.display());
    fire_sink(
        &s,
        "claude",
        "Notification",
        &pane,
        &notification_permission(SESSION),
        &sink_cmd,
    );

    // The winning event stamped the blocked transition.
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "blocked",
        "the strictly-newer real-clock event wins arbitration and stamps blocked"
    );
    assert_eq!(s.get(&pane, "#{@agent_detail}"), "permission");
    assert_eq!(s.get(&pane, "#{@agent_attention}"), "1");
    // The write-before-fire marker committed (the guarded companion ran).
    assert!(
        !s.get(&pane, "#{@agent_notified_at}").is_empty(),
        "a winning event commits the write-before-fire marker"
    );
    // And the notification fired (read-back gate: the marker committed).
    assert_eq!(
        std::fs::read_to_string(&sink).unwrap().trim(),
        "fire",
        "a winning notifiable event fires exactly once"
    );
}

#[test]
fn subagent_events_are_bookkeeping_only() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000"
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Register + go blocked (owner session).
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane,
        &payload("SessionStart", SESSION),
        false,
    );
    fire(
        &s,
        "claude",
        "Notification",
        &pane,
        &notification_permission(SESSION),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");

    // A subagent starts (foreign session), then fires a working event: the parent stays
    // blocked (subagent guard), and @agent_subagents tracks the child.
    let sub = "aaaa-subagent-session";
    fire(
        &s,
        "claude",
        "SubagentStart",
        &pane,
        &payload("SubagentStart", sub),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_subagents}"), sub);
    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", sub),
        false,
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "blocked",
        "a subagent's working event must not clobber the parent"
    );

    // Subagent stops: bookkeeping cleared.
    fire(
        &s,
        "claude",
        "SubagentStop",
        &pane,
        &payload("SubagentStop", sub),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_subagents}"), "");

    // Now the owner's own working event is honored again.
    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", SESSION),
        false,
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");
}

/// Codex delivers its `notify` JSON as a trailing argv argument, not stdin. This asserts that
/// argv-delivered payload reaches `tma event` (via the wrapper's argv→stdin forward), maps through
/// the bundled codex manifest (`agent-turn-complete` ⇒ idle), and stamps the pane, ignoring stdin.
#[test]
fn codex_notify_argv_payload_stamps_idle() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // A real agent-turn-complete notification (live capture; keys thread-id/turn-id, not
    // session_id, with client "codex-tui"). Delivered as argv, not stdin.
    let notify = r#"{"type":"agent-turn-complete","thread-id":"019f99c3-7c57-7963-98e9-f496a7978257","turn-id":"019f99c4-38c9-7f63-901a-d9910886b99a","cwd":"/tmp","client":"codex-tui","input-messages":["hi"],"last-assistant-message":"done"}"#;
    fire_argv(&s, "codex", "notify", &pane, notify);
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "idle",
        "agent-turn-complete ⇒ idle"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    assert_eq!(s.get(&pane, "#{@agent_name}"), "codex");
    // Codex's notify carries no session_id, so the owner is never recorded (subagent guard inert).
    assert_eq!(s.get(&pane, "#{@agent_session}"), "");

    // A non-turn-complete notify must not change state (matcher miss ⇒ unmapped ⇒ ignore). Re-stamp
    // working via a bare set, then confirm the stray notify leaves it untouched.
    assert!(s
        .tmux(&["set", "-pt", &pane, "@agent_state", "working"])
        .status
        .success());
    fire_argv(
        &s,
        "codex",
        "notify",
        &pane,
        r#"{"type":"some-other-notification"}"#,
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "a non-turn-complete notify must not restamp"
    );
}

/// A registration-class hook stamps `@agent_model` from the payload's top-level `model`
/// field, it coexists with the Codex rollout tail's identical stamp (both plain last-write-wins
/// writes, so the same value never oscillates), and ONLY registration-class events touch it — a later
/// turn event carrying a different `model` must not restamp.
#[test]
fn registration_stamps_model_and_coexists_with_tail() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Codex SessionStart carries `model` (its real hooks.json shape) ⇒ stamped on registration.
    let start = format!(
        r#"{{"session_id":"{SESSION}","hook_event_name":"SessionStart","source":"startup","model":"gpt-5.6-terra"}}"#
    );
    fire(&s, "codex", "SessionStart", &pane, &start, false);
    assert_eq!(s.get(&pane, "#{@agent_state}"), "idle");
    assert_eq!(
        s.get(&pane, "#{@agent_model}"),
        "gpt-5.6-terra",
        "registration stamps the model from the payload"
    );

    // The rollout tail writes the SAME model via a plain set (exactly `poll_context_tails`' write);
    // hook and tail writing the same value must not fight — the read-back is stable.
    assert!(s
        .tmux(&["set", "-pt", &pane, "@agent_model", "gpt-5.6-terra"])
        .status
        .success());

    // A later turn event also carries `model`, but only registration-class events restamp: a
    // UserPromptSubmit with a different model leaves @agent_model where the tail and register left it.
    let turn = format!(
        r#"{{"session_id":"{SESSION}","hook_event_name":"UserPromptSubmit","model":"gpt-9-imaginary","prompt":"hi"}}"#
    );
    fire(&s, "codex", "UserPromptSubmit", &pane, &turn, false);
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");
    assert_eq!(
        s.get(&pane, "#{@agent_model}"),
        "gpt-5.6-terra",
        "only registration-class events stamp the model; the tail and hook agree"
    );

    // Re-registration with the same value stays stable (no oscillation).
    fire(&s, "codex", "SessionStart", &pane, &start, false);
    assert_eq!(s.get(&pane, "#{@agent_model}"), "gpt-5.6-terra");

    // Ownership guard: while a subagent is live, a foreign session's SessionStart is ignored — and
    // its model must not stamp either (a nested agent inherits $TMUX_PANE and fires real hooks).
    let sub = "aaaa-subagent-session";
    fire(
        &s,
        "codex",
        "SubagentStart",
        &pane,
        &format!(r#"{{"session_id":"{sub}","hook_event_name":"SubagentStart"}}"#),
        false,
    );
    let foreign = format!(
        r#"{{"session_id":"{sub}","hook_event_name":"SessionStart","source":"startup","model":"haiku-nested"}}"#
    );
    fire(&s, "codex", "SessionStart", &pane, &foreign, false);
    assert_eq!(
        s.get(&pane, "#{@agent_model}"),
        "gpt-5.6-terra",
        "an ignored foreign registration must not stamp its model"
    );
}

/// A malformed `config.toml` must NOT kill the hook path: the wrapper swallows a nonzero exit, so a
/// fail-fast would silently disable all hook state tracking. Instead `event` degrades to defaults and
/// warns on stderr naming the file + key. Fired directly so the warning is observable; every other
/// subcommand still fails fast (proven in config_integration.rs).
#[test]
fn event_degrades_on_malformed_config_and_still_stamps() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("event");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");
    assert!(pane.starts_with('%'), "got pane {pane:?}");

    // A garbage config: a type error (freshness_secs wants an integer) — toml names key + span.
    let bad_config = s.workdir.join("config.toml");
    std::fs::write(&bad_config, "[fold]\nfreshness_secs = \"three\"\n").unwrap();

    use std::io::Write;
    let mut child = Command::new(common::tma_bin())
        .args([
            "event",
            "--agent",
            "claude",
            "--kind",
            "SessionStart",
            "--payload",
            "-",
        ])
        .args(["--socket-name", &s.socket])
        .args(["--config", bad_config.to_str().unwrap()])
        .env("TMUX_PANE", &pane)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload("SessionStart", SESSION).as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait tma event");

    // The stamp still lands: state tracking survived the bad config on the hook path.
    assert!(
        out.status.success(),
        "event must not fail on a malformed config"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "idle",
        "SessionStart stamped idle despite the malformed config"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");

    // ...and it warned, naming the config file + the offending key.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("config.toml") && stderr.contains("freshness_secs"),
        "the degrade warning names the config file + error: {stderr:?}"
    );
}

/// The wrapper resolves `tma` fresh on every fire and exits 0 silently when it cannot find one
/// (rebuild in flight, uninstalled), so a hook never surfaces a failure. This makes the binary
/// unresolvable via all three lookup paths and asserts a silent exit 0.
#[test]
fn wrapper_exits_zero_silently_when_binary_missing() {
    use std::io::Write;
    let s = Scratch::new("event");
    let wrapper = wrapper(&s); // copied into the workdir with no sibling `tma` next to it

    let mut child = Command::new(&wrapper)
        .arg("claude")
        .arg("SessionStart")
        // No $TMA_BIN, and a PATH with no `tma` on it (dirname/pwd still resolve from /bin):
        // all three resolution paths fail, so the wrapper must silently succeed.
        .env_remove("TMA_BIN")
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn wrapper");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload("SessionStart", SESSION).as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait wrapper");

    assert!(
        out.status.success(),
        "wrapper must exit 0 when the binary is absent"
    );
    assert!(
        out.stdout.is_empty(),
        "wrapper must be silent on stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "wrapper must be silent on stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
