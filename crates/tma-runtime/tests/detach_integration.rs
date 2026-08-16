//! Detached actions end to end through the real `tma` binary, so the broker's `current_exe()`
//! resolves to the tma binary and the hidden `supervise` mode actually runs. A user action manifest
//! with `detach = true` is dropped into an `XDG_CONFIG_HOME/tma/actions` dir; a pane is stamped as a
//! fresh agent so the (gate-less) exec action fires. The three properties pinned:
//!
//! - two invocations serialize (the second exits 5 while the child lives, then the lock clears);
//! - a short `detach_timeout_ms` kills the child and clears the lock at the deadline;
//! - the completion notification fires with the pinned payload (via `TMA_NOTIFY_CMD`).

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use tma_test_support::{self as common, Scratch};

use tma_tmux::lock::LockValue;

/// Stamp `pane` as a fresh `idle` claude agent so an ungated exec action applies and fires.
fn stamp_idle_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

/// Write a detached exec action `stem` (agents = ["claude"]) into `xdg/tma/actions`.
fn write_action(xdg: &Path, stem: &str, command: &str, detach_timeout_ms: u64) {
    let dir = xdg.join("tma/actions");
    std::fs::create_dir_all(&dir).unwrap();
    let body = format!(
        "min_engine_version = \"0.1\"\nname = \"{stem}\"\nlabel = \"L\"\nkind = \"exec\"\n\
         agents = [\"claude\"]\ndetach = true\ndetach_timeout_ms = {detach_timeout_ms}\n\
         command = \"{command}\"\n"
    );
    std::fs::write(dir.join(format!("{stem}.toml")), body).unwrap();
}

/// Run `tma <args>` against the scratch server with the user-actions dir pinned via `XDG_CONFIG_HOME`
/// and an optional `TMA_NOTIFY_CMD` completion observer.
fn tma_act(s: &Scratch, xdg: &Path, notify_cmd: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new(common::tma_bin());
    cmd.args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", common::empty_config_path())
        .env("XDG_CONFIG_HOME", xdg);
    if let Some(nc) = notify_cmd {
        cmd.env("TMA_NOTIFY_CMD", nc);
    }
    cmd.output().expect("spawn tma act")
}

fn lock_present(s: &Scratch, pane: &str) -> bool {
    LockValue::parse(&s.pane_option(pane, "@agent_action")).is_some()
}

/// Poll up to `deadline` for the completion `marker` to both exist and carry `needle`, returning its
/// full contents. `TMA_NOTIFY_CMD` (`cat >> marker`) creates the file on open, then writes the JSON,
/// so gating on existence alone can read the file mid-write and see it empty; gating on the payload's
/// own text closes that window. The deadline is pure test patience: it returns in milliseconds on the
/// happy path and only stretches when the machine is saturated under parallel `cargo test` load.
fn await_completion(marker: &Path, needle: &str, deadline: Duration) -> String {
    let mut payload = String::new();
    let arrived = common::wait_until(deadline, || {
        payload = std::fs::read_to_string(marker).unwrap_or_default();
        payload.contains(needle)
    });
    assert!(
        arrived,
        "completion notification did not carry {needle:?} within {deadline:?}; got: {payload:?}"
    );
    payload
}

#[test]
fn two_detached_invocations_serialize_then_the_lock_clears() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let _gate = common::DaemonTestGuard::acquire();
    let s = Scratch::new_daemon("detach_serialize");
    let pane = s.new_shell_pane();
    stamp_idle_claude(&s, &pane);
    let xdg = s.workdir.join("xdg");
    // A child that lives ~2 s, well under the 30 s detach deadline: the lock is held while it runs.
    write_action(&xdg, "sleeper", "sleep 2", 30_000);

    // First invocation spawns the supervisor and returns `spawned` (exit 0).
    let first = tma_act(
        &s,
        &xdg,
        None,
        &["act", "sleeper", "--pane", &pane, "--json"],
    );
    assert_eq!(first.status.code(), Some(0), "first detach spawns (exit 0)");
    let out = String::from_utf8_lossy(&first.stdout);
    assert!(
        out.contains("\"outcome\":\"spawned\""),
        "outcome spawned: {out}"
    );

    // The supervisor holds the lock; a live holder pid should be stamped shortly.
    assert!(
        common::wait_until(common::POLL_CEILING, || lock_present(&s, &pane)),
        "the supervisor holds the single-flight lock while the child runs"
    );

    // A second invocation while the child lives refuses with exit 5 (locked).
    let second = tma_act(
        &s,
        &xdg,
        None,
        &["act", "sleeper", "--pane", &pane, "--json"],
    );
    assert_eq!(second.status.code(), Some(5), "a held lock refuses exit 5");
    let out2 = String::from_utf8_lossy(&second.stdout);
    assert!(
        out2.contains("\"reason\":\"locked\""),
        "reason locked: {out2}"
    );

    // When the child exits (~2 s), the supervisor clears the lock.
    assert!(
        common::wait_until(common::POLL_CEILING, || !lock_present(&s, &pane)),
        "the lock clears once the detached child exits"
    );
}

#[test]
fn detach_deadline_kills_the_child_and_clears_the_lock() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let _gate = common::DaemonTestGuard::acquire();
    let s = Scratch::new_daemon("detach_deadline");
    let pane = s.new_shell_pane();
    stamp_idle_claude(&s, &pane);
    let xdg = s.workdir.join("xdg");
    // A long-running child with a short (700 ms) deadline: the supervisor kills it well before it ends.
    write_action(&xdg, "hang", "sleep 60", 700);
    let marker = s.workdir.join("completion.json");
    // The completion command appends its JSON stdin payload to a file so we can inspect it.
    let notify = format!("cat >> {}", marker.display());

    let r = tma_act(
        &s,
        &xdg,
        Some(&notify),
        &["act", "hang", "--pane", &pane, "--json"],
    );
    assert_eq!(r.status.code(), Some(0), "detach spawns (exit 0)");

    // The deadline (700 ms) fires and the lock clears far sooner than the child's own 60 s sleep.
    assert!(
        common::wait_until(common::POLL_CEILING, || !lock_present(&s, &pane)),
        "the deadline kill clears the lock"
    );

    // The completion notification fired with the pinned payload; a deadline kill is `timeout` with a
    // null exit_code, and the pane is still alive so the locator is present. Gate on the payload
    // text so the read never races `cat`'s create-then-write; the deadline is generous test patience.
    let payload = await_completion(&marker, "\"outcome\":\"timeout\"", common::POLL_CEILING);
    assert!(payload.contains("\"schema\":1"), "payload: {payload}");
    assert!(
        payload.contains("\"action\":\"hang\""),
        "payload: {payload}"
    );
    assert!(
        payload.contains("\"agent\":\"claude\""),
        "payload: {payload}"
    );
    assert!(payload.contains("\"exit_code\":null"), "payload: {payload}");
    assert!(
        payload.contains(&format!("\"pane\":\"{pane}\"")),
        "payload: {payload}"
    );
}

#[test]
fn detached_completion_reports_the_child_exit_code() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let _gate = common::DaemonTestGuard::acquire();
    let s = Scratch::new_daemon("detach_exit");
    let pane = s.new_shell_pane();
    stamp_idle_claude(&s, &pane);
    let xdg = s.workdir.join("xdg");
    write_action(&xdg, "quick", "exit 7", 30_000);
    let marker = s.workdir.join("completion.json");
    let notify = format!("cat >> {}", marker.display());

    let r = tma_act(
        &s,
        &xdg,
        Some(&notify),
        &["act", "quick", "--pane", &pane, "--json"],
    );
    assert_eq!(r.status.code(), Some(0), "detach spawns (exit 0)");

    // A normal exit passes the child's own code through as `exited`. Gate on the payload
    // text so the read never races `cat`'s create-then-write; the deadline is generous test patience.
    let payload = await_completion(&marker, "\"outcome\":\"exited\"", common::POLL_CEILING);
    assert!(payload.contains("\"exit_code\":7"), "payload: {payload}");

    // The lock is released after the child finished.
    assert!(
        common::wait_until(common::POLL_CEILING, || !lock_present(&s, &pane)),
        "the lock clears after the detached child exits"
    );
}
