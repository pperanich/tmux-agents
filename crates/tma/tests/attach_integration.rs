//! `tma attach --pane` acceptance on a scratch server (A-270).
//!
//! The handover itself replaces the process with an interactive tmux client, which a headless test
//! cannot observe. `--print` is the seam: it performs the same window/pane selects and then prints
//! the argv it would have exec'd instead of exec'ing it, so both halves are assertable. The scratch
//! `tmux -L tma_test_<unique>` server (`-f /dev/null`) is killed on drop, never the default one.

use std::process::{Command, Output, Stdio};

use tma_test_support::{empty_config_path, Scratch};

/// `tma <args>` against the scratch server with an explicitly empty stdin. The tty refusal is the
/// point of one of these tests, so the child's stdin is wired here rather than left to a default.
fn tma_no_tty(s: &Scratch, args: &[&str]) -> Output {
    Command::new(s.bin())
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", empty_config_path())
        // A tma running inside the developer's own tmux would take the jump path instead.
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn tma")
}

/// A `work` session with two windows, the second split in two. Returns the pane id of the second
/// pane of window 1, which is neither the session's current window nor its window's active pane,
/// so a passing assertion means both selects really ran.
fn two_window_session(s: &Scratch) -> String {
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    assert!(s
        .tmux(&["new-window", "-t", "work:", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "work:1", "exec sleep 100000"])
        .status
        .success());
    let target = s.display("work:1", "#{pane_id}");
    // Park the session back on window 0 with window 1's first pane active, so the target is two
    // selects away.
    assert!(s.tmux(&["select-pane", "-t", "work:1.0"]).status.success());
    assert!(s.tmux(&["select-window", "-t", "work:0"]).status.success());
    assert_eq!(
        s.display("work", "#{window_index}"),
        "0",
        "parked on window 0"
    );
    assert!(target.starts_with('%'), "got pane {target:?}");
    target
}

/// A-270. The pane's window and pane are selected on its session, and the argv that would take over
/// this terminal is printed: the resolved tmux binary, this invocation's socket selector, and the
/// pane's own session as the attach target.
#[test]
fn attach_print_selects_the_target_and_prints_the_attach_argv() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("attach_print");
    let pane = two_window_session(&s);

    let out = tma_no_tty(&s, &["attach", "--pane", &pane, "--print"]);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success(),
        "attach --print failed: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The handover half: an argv, not a shell line tma would have had to quote its way out of.
    assert!(
        stdout.ends_with("attach-session -t work"),
        "the attach names the pane's session: {stdout:?}"
    );
    assert!(
        stdout.contains(&format!("-L {}", s.socket)),
        "the attach carries this invocation's socket selector: {stdout:?}"
    );
    assert!(
        stdout.starts_with('/'),
        "the argv leads with the resolved tmux binary: {stdout:?}"
    );

    // The focus half, read back off the server: the session is on window 1 and that window's
    // active pane is the target.
    assert_eq!(
        s.display("work", "#{window_index}"),
        "1",
        "the pane's window was selected"
    );
    assert_eq!(
        s.display("work", "#{pane_id}"),
        pane,
        "the pane itself was selected"
    );
}

/// The exec hands tmux this process's terminal. Without one there is nothing to hand over, so the
/// command refuses (exit 2) rather than letting tmux fail with its own terse line after the selects
/// have already moved somebody's focus.
#[test]
fn attach_refuses_when_stdin_is_not_a_terminal() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("attach_notty");
    let pane = two_window_session(&s);

    let out = tma_no_tty(&s, &["attach", "--pane", &pane]);
    assert_eq!(out.status.code(), Some(2), "no tty is a usage refusal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a terminal"),
        "the refusal says why: {stderr}"
    );
    // The refusal comes before anything moves: the session is still parked where it was.
    assert_eq!(s.display("work", "#{window_index}"), "0");
}

/// An unknown pane is the shared `vanished` refusal (exit 3), not an attach to whatever session
/// tmux would have picked instead.
#[test]
fn attach_refuses_a_pane_that_is_not_there() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("attach_gone");
    let _pane = two_window_session(&s);

    let out = tma_no_tty(&s, &["attach", "--pane", "%99999", "--print"]);
    assert_eq!(out.status.code(), Some(3), "an unknown pane is exit 3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vanished"),
        "the refusal names it: {stderr}"
    );
    assert!(out.stdout.is_empty(), "nothing was printed to exec");
}
