//! Attention + episode wiring acceptance on a scratch tmux server: a blocked publish sets
//! `@agent_attention` and `clear-attention` removes it; the notification dedup survives a simulated
//! daemon restart (a cold `tma event` whose only record is the persisted `@agent_notified_at`) and
//! does NOT re-fire; a pid-change episode boundary clears both through `debug stamp --episode-reset`.
//! Runs on a scratch `tmux -L` server, killed on drop. `clear-attention` is invoked directly (the
//! simulated select-pane hook), since `#{hook_pane}` is empty without an attached client.

use std::io::Write;
use std::process::{Command, Stdio};

use common::Scratch;
use tma_test_support as common;

/// The `tma` binary for tests inside the `tma` package.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tma")
}

/// A fresh `tma event` process (cold: no in-process state), pane pinned via `TMUX_PANE`. Suite-specific
/// (claude agent, stdin payload, cold-restart semantics), so a free helper over the shared [`Scratch`].
fn event(s: &Scratch, event: &str, pane: &str, payload: &str, notify: bool) {
    let mut child = Command::new(bin())
        .args([
            "event",
            "--agent",
            "claude",
            "--kind",
            event,
            "--payload",
            "-",
        ])
        .args(["--socket-name", &s.socket])
        .env("TMUX_PANE", pane)
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_NOTIFY_FROM_EVENT", if notify { "1" } else { "0" })
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// Simulate the select-pane hook firing the clear-attention command.
fn clear_attention(s: &Scratch, pane: &str) {
    let out = Command::new(bin())
        .args(["clear-attention", pane, "--socket-name", &s.socket])
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn clear-attention");
    assert!(out.status.success());
}

const SESSION: &str = "65ced290-2a08-43de-aa80-d0b049d7ce30";

fn payload(event: &str) -> String {
    format!(r#"{{"session_id":"{SESSION}","hook_event_name":"{event}"}}"#)
}

fn notification() -> String {
    format!(
        r#"{{"session_id":"{SESSION}","hook_event_name":"Notification","notification_type":"permission_prompt"}}"#
    )
}

#[test]
fn attention_clears_and_notify_dedups_across_cold_restart() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("attn");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Register, work, then block with notify opt-in.
    event(&s, "SessionStart", &pane, &payload("SessionStart"), false);
    event(
        &s,
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit"),
        false,
    );
    event(&s, "Notification", &pane, &notification(), true);

    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");
    assert_eq!(
        s.get(&pane, "#{@agent_attention}"),
        "1",
        "blocked sets attention"
    );
    let notified_1 = s.get(&pane, "#{@agent_notified_at}");
    assert!(!notified_1.is_empty(), "notify marker written");

    // Simulated after-select-pane hook: attention cleared.
    clear_attention(&s, &pane);
    assert_eq!(
        s.get(&pane, "#{@agent_attention}"),
        "",
        "select-pane hook clears attention"
    );

    // "Daemon restart": re-run the producer cold on the SAME blocked episode. The only dedup record
    // is the persisted marker, so no re-fire, no marker bump, and attention stays clear.
    event(&s, "Notification", &pane, &notification(), true);
    assert_eq!(s.get(&pane, "#{@agent_state}"), "blocked");
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        notified_1,
        "no re-fire across a cold producer restart"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_attention}"),
        "",
        "a continuing episode does not re-arm attention"
    );
}

#[test]
fn episode_reset_clears_attention_and_notified() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("attn");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    let pane = s.get("s1", "#{pane_id}");

    // Seed a blocked stamp with attention + a notify marker for pid 111.
    for (k, v) in [
        ("@agent_state", "blocked"),
        ("@agent_source", "hook"),
        ("@agent_attention", "1"),
        ("@agent_notified_at", "1000"),
        ("@agent_since", "1000"),
        ("@agent_pid", "111"),
    ] {
        assert!(s
            .tmux(&["set-option", "-p", "-t", &pane, k, v])
            .status
            .success());
    }

    // A pid-change episode boundary (pid 222): the write path must clear attention + marker.
    let out = Command::new(bin())
        .args(["debug", "stamp", &pane, "--socket-name", &s.socket])
        .args([
            "--mode", "publish", "--state", "working", "--source", "capture",
        ])
        .args(["--pid", "222", "--evidence-at", "2000", "--since", "2000"])
        .args(["--stamped-at", "2000", "--episode-reset"])
        .output()
        .expect("spawn debug stamp");
    assert!(
        out.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");
    assert_eq!(
        s.get(&pane, "#{@agent_attention}"),
        "",
        "episode reset clears attention"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        "",
        "episode reset clears the notify marker"
    );
}
