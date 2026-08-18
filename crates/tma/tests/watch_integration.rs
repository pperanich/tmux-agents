//! Acceptance: `tma watch` jumps to the highlighted agent on Enter, plus the width-driven
//! preview (present when wide, absent when narrow). Like the picker, `watch` runs an alt-screen TUI in
//! a `home` pane needing an attached PTY client (via a Python pty-fork helper; skips without python3).
//! Unlike the picker, `watch` is persistent (no close on Enter), so this asserts the jump landed and
//! attention cleared, then lets `Drop` tear the server down. The preview tests set pane width with
//! `window-size manual` + `resize-window`; the discriminator is the work pane's permission prompt,
//! which renders only inside the preview, so its presence proves the split and its absence the MVP.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tma_test_support::{poll_until, wait_capture_contains, AttachOutcome, Scratch, POLL_CEILING};

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Run a git command in `dir`; `false` if git is unavailable or the command failed.
fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|st| st.success())
        .unwrap_or(false)
}

/// Shared setup for the preview tests: a scratch server with an attached `home` pane for `watch` and
/// a detached `work` session holding one blocked agent (stamps populated so the first frame lists it).
/// Returns `None` on an environment gap (no tmux/python3) so the caller skips, but panics on a real
/// attach regression rather than masking it as a skip. The `Scratch` keeps the PTY attached until drop.
fn setup_blocked_agent(label: &str) -> Option<Scratch> {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return None;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return None;
    }
    let mut s = Scratch::new(label);

    // `home`: a shell where the watcher will run (attached). `work`: a detached blocked agent.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "-x", "100", "-y", "30"])
        .status
        .success());
    let chrome = "\\n\\n\\n\\n\\n\\n\\n\\n\
        ╭──────────────────────────╮\\n\
        │ Do you want to proceed?  │\\n\
        │ ❯ 1. Yes                 │\\n\
        ╰──────────────────────────╯\\n";
    let work_cmd = format!("printf '{chrome}'; exec sleep 100000");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-x",
            "100",
            "-y",
            "30",
            &work_cmd
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, "work", "Do you want to proceed?", POLL_CEILING),
        "work pane chrome did not render"
    );

    let work_pid = s.display("work", "#{pane_pid}");
    let cc = basename(&s.display("work", "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &work_pid])
            .output()
            .unwrap()
            .stdout,
    ));
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version=\"0.1\"\n[identity]\nprocess_names=[\"{cc}\",\"{psc}\"]\n\
             [capture]\nvisible=[\"working\",\"idle\",\"blocked\"]\n\
             [[rules]]\nstate=\"blocked\"\ndetail=\"permission\"\npriority=100\n\
             region=\"tail_lines(50)\"\nmatch={{ contains=\"Do you want to proceed?\" }}\n"
        ),
    )
    .unwrap();

    // Populate stamps so the watcher's first frame already lists the blocked agent.
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "blocked");

    match s.attach_client("home") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return None;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    Some(s)
}

/// Launch the watcher in the `home` pane and hard-fail if the first frame never renders (a startup
/// regression, not an environment gap — the only genuine skips are in [`setup_blocked_agent`]).
fn launch_watch(s: &Scratch) {
    let launch = format!(
        "'{}' watch --socket-name '{}' --manifest-dir '{}'",
        s.bin(),
        s.socket,
        s.workdir.display()
    );
    s.tmux(&["send-keys", "-t", "home", &launch, "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "home", "agents (", POLL_CEILING),
        "watch must render its first frame within 4 s after launch (startup regression)"
    );
}

#[test]
fn watch_enter_jumps_to_highlighted_agent() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return;
    }
    let mut s = Scratch::new("watch");

    // `home`: a shell where the watcher will run (attached). `work`: a detached blocked agent.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "-x", "100", "-y", "30"])
        .status
        .success());
    let chrome = "\\n\\n\\n\\n\\n\\n\\n\\n\
        ╭──────────────────────────╮\\n\
        │ Do you want to proceed?  │\\n\
        │ ❯ 1. Yes                 │\\n\
        ╰──────────────────────────╯\\n";
    let work_cmd = format!("printf '{chrome}'; exec sleep 100000");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-x",
            "100",
            "-y",
            "30",
            &work_cmd
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, "work", "Do you want to proceed?", POLL_CEILING),
        "work pane chrome did not render"
    );

    let work_pane = s.display("work", "#{pane_id}");
    let work_pid = s.display("work", "#{pane_pid}");
    let cc = basename(&s.display("work", "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &work_pid])
            .output()
            .unwrap()
            .stdout,
    ));
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version=\"0.1\"\n[identity]\nprocess_names=[\"{cc}\",\"{psc}\"]\n\
             [capture]\nvisible=[\"working\",\"idle\",\"blocked\"]\n\
             [[rules]]\nstate=\"blocked\"\ndetail=\"permission\"\npriority=100\n\
             region=\"tail_lines(50)\"\nmatch={{ contains=\"Do you want to proceed?\" }}\n"
        ),
    )
    .unwrap();

    // Populate stamps so the watcher's first frame already lists the blocked agent.
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "blocked");

    match s.attach_client("home") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }

    let home_pane = s.display("home", "#{pane_id}");

    // Launch the watcher in the home pane.
    let launch = format!(
        "'{}' watch --socket-name '{}' --manifest-dir '{}'",
        s.bin(),
        s.socket,
        s.workdir.display()
    );
    s.tmux(&["send-keys", "-t", "home", &launch, "Enter"]);

    // The client attached and the launch keys were sent, so a render timeout here is a real startup
    // regression — hard-fail. The genuine skips (no tmux/python3/PTY client) are handled above.
    poll_until("watch to render its first frame after launch", || {
        let content = s.tmux(&["capture-pane", "-p", "-t", "home"]);
        String::from_utf8_lossy(&content.stdout).contains("agents (")
    });

    // The running watcher advertises its pid in `@tma_watch_pid` on its own pane.
    let advertised = s.display(&home_pane, "#{@tma_watch_pid}");
    assert!(
        advertised.parse::<i32>().map(|p| p > 0).unwrap_or(false),
        "watch must advertise a positive pid in @tma_watch_pid, got {advertised:?}"
    );

    // Enter jumps to the highlighted (sorted-first, blocked) agent; the watcher stays open.
    s.tmux(&["send-keys", "-t", "home", "Enter"]);

    poll_until(
        &format!("watch Enter to jump the client to the blocked agent {work_pane}"),
        || s.display("", "#{pane_id}") == work_pane,
    );
    // The attention flag for the jumped-to pane is cleared on Enter. The clear is a separate write
    // from the jump the poll above saw, so poll for it rather than reading straight after.
    poll_until("watch jump to clear the target's attention flag", || {
        s.display("work", "#{@agent_attention}").is_empty()
    });

    // Quitting the watcher unsets its `@tma_watch_pid` advertisement eagerly. Re-focus home first —
    // the jump above moved the client onto the work pane.
    s.tmux(&["send-keys", "-t", "home", "q"]);
    poll_until("quitting watch to unset @tma_watch_pid on its pane", || {
        s.display(&home_pane, "#{@tma_watch_pid}").is_empty()
    });
}

/// A wide watch pane splits the body and shows a live preview beside the list. `resize-window -x 90`
/// forces the pane above the 76-column threshold (the pane PTY takes the window width, verified on
/// tmux 3.6a); the blocked agent's permission prompt renders only in the preview, proving the split.
#[test]
fn watch_wide_shows_preview() {
    let Some(s) = setup_blocked_agent("watch-wide") else {
        return;
    };
    assert!(s
        .tmux(&["set-option", "-g", "window-size", "manual"])
        .status
        .success());
    assert!(s
        .tmux(&["resize-window", "-t", "home", "-x", "90", "-y", "30"])
        .status
        .success());

    launch_watch(&s);

    assert!(
        wait_capture_contains(&s.socket, "home", "Do you want to proceed?", POLL_CEILING),
        "wide watch must render the highlighted agent's live preview beside the list"
    );
}

/// A narrow watch pane stays the single-list MVP — no preview. `resize-window -x 60` holds it below
/// the 76-column threshold; the permission prompt is the preview's only unique content, so its
/// absence proves no split. A short wait lets a wrongly-fired refresh-tick capture surface first.
#[test]
fn watch_narrow_hides_preview() {
    let Some(s) = setup_blocked_agent("watch-narrow") else {
        return;
    };
    assert!(s
        .tmux(&["set-option", "-g", "window-size", "manual"])
        .status
        .success());
    assert!(s
        .tmux(&["resize-window", "-t", "home", "-x", "60", "-y", "30"])
        .status
        .success());

    launch_watch(&s);

    // A negative window: the preview must NOT appear, so there is nothing to poll for — give a
    // wrongly-fired refresh tick time to render one first.
    std::thread::sleep(Duration::from_secs(2));
    let content = s.tmux(&["capture-pane", "-p", "-t", "home"]);
    let text = String::from_utf8_lossy(&content.stdout);
    assert!(
        text.contains("agents ("),
        "narrow watch must still render the agent list"
    );
    assert!(
        !text.contains("Do you want to proceed?"),
        "narrow watch must not render a preview (single-list MVP below the width threshold)"
    );
}

/// A wide watch pane groups its rows by repo: the `▸ <repo>` header line renders above the group, and
/// pressing `g` flattens the list so the header disappears. The work agent runs in a scratch git repo
/// so the refresh-path annotation resolves a repo name; grouping renders only in the wide arms (here
/// the table, forced with `--table` and a 100-column pane). Skips on the same env gaps as the other
/// watch tests plus a missing git or a host that does not report `#{pane_current_path}`.
#[test]
fn watch_wide_groups_by_repo_and_g_flattens() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not installed");
        return;
    }
    let mut s = Scratch::new("watch-group");

    // A scratch git repo under the workdir (torn down with the Scratch); the work agent runs in it.
    let repo = s.workdir.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    if !git(&repo, &["init", "-q", "-b", "main"]) {
        eprintln!("skipping: git init failed (unavailable or too old for -b)");
        return;
    }
    assert!(git(&repo, &["config", "user.email", "t@t"]));
    assert!(git(&repo, &["config", "user.name", "t"]));
    assert!(git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]));

    // `home`: the wide attached pane the watcher runs in. `work`: a detached blocked agent in the repo.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "-x", "100", "-y", "30"])
        .status
        .success());
    let chrome = "\\n\\n\\n\\n\\n\\n\\n\\n\
        ╭──────────────────────────╮\\n\
        │ Do you want to proceed?  │\\n\
        │ ❯ 1. Yes                 │\\n\
        ╰──────────────────────────╯\\n";
    let work_cmd = format!("printf '{chrome}'; exec sleep 100000");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-c",
            repo.to_str().unwrap(),
            "-x",
            "100",
            "-y",
            "30",
            &work_cmd,
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, "work", "Do you want to proceed?", POLL_CEILING),
        "work pane chrome did not render"
    );

    let work_pane = s.display("work", "#{pane_id}");
    let work_pid = s.display("work", "#{pane_pid}");
    let cc = basename(&s.display("work", "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &work_pid])
            .output()
            .unwrap()
            .stdout,
    ));
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version=\"0.1\"\n[identity]\nprocess_names=[\"{cc}\",\"{psc}\"]\n\
             [capture]\nvisible=[\"working\",\"idle\",\"blocked\"]\n\
             [[rules]]\nstate=\"blocked\"\ndetail=\"permission\"\npriority=100\n\
             region=\"tail_lines(50)\"\nmatch={{ contains=\"Do you want to proceed?\" }}\n"
        ),
    )
    .unwrap();
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "blocked");

    // The resolver reads `#{pane_current_path}`; without it the annotation has nothing to work from
    // and the grouping assertion would be vacuous, so skip.
    if !s
        .display(&work_pane, "#{pane_current_path}")
        .contains("proj")
    {
        eprintln!("skipping: host does not report #{{pane_current_path}} for the pane");
        return;
    }

    // Force the pane wide (grouping renders only in the wide arms) before the client's size can win.
    assert!(s
        .tmux(&["set-option", "-g", "window-size", "manual"])
        .status
        .success());
    assert!(s
        .tmux(&["resize-window", "-t", "home", "-x", "100", "-y", "30"])
        .status
        .success());

    match s.attach_client("home") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }

    // Launch the watcher straight into the table (a wide arm that groups).
    let launch = format!(
        "'{}' watch --table --socket-name '{}' --manifest-dir '{}'",
        s.bin(),
        s.socket,
        s.workdir.display()
    );
    s.tmux(&["send-keys", "-t", "home", &launch, "Enter"]);

    // The seed and the refresh both annotate, so the `▸ proj` group header is there from the first
    // frame; the poll tolerates a slower host rather than pinning which frame it arrived on.
    assert!(
        wait_capture_contains(&s.socket, "home", "▸ proj", POLL_CEILING),
        "wide grouped watch must render a `▸ proj` repo header"
    );

    // `g` flattens the list: the header disappears (a flat labeled list has no group lines).
    s.tmux(&["send-keys", "-t", "home", "g"]);
    let mut flattened = false;
    let deadline = Instant::now() + POLL_CEILING;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let content = s.tmux(&["capture-pane", "-p", "-t", "home"]);
        if !String::from_utf8_lossy(&content.stdout).contains("▸ proj") {
            flattened = true;
            break;
        }
    }
    assert!(
        flattened,
        "pressing g must flatten the grouped list, removing the `▸ proj` header"
    );
}

/// The sender in isolation: `tma clear-attention` walks panes for `@tma_watch_pid` and SIGUSR1s each
/// pid. We advertise a `sleep` child's pid and assert `clear-attention` signals it — SIGUSR1 terminates
/// the child, so its death by signal is the observable proof. No PTY drive needed.
#[test]
fn clear_attention_nudges_advertised_watch_pid() {
    use std::os::unix::process::ExitStatusExt;

    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("watch");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "-x", "80", "-y", "24"])
        .status
        .success());
    let pane = s.display("home", "#{pane_id}");

    // A stand-in watcher process: a plain sleep, SIGUSR1 terminates it by default.
    let mut child = Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn sleep child");
    let child_pid = child.id();

    // Advertise the child's pid the way a real `tma watch` would (pane-scoped option).
    assert!(s
        .tmux(&[
            "set-option",
            "-p",
            "-t",
            &pane,
            "@tma_watch_pid",
            &child_pid.to_string(),
        ])
        .status
        .success());

    // Run the hook body. It clears attention (a no-op here) then nudges the advertised pid.
    assert!(s.tma(&["clear-attention", &pane]).status.success());

    // The child should be terminated by SIGUSR1 within a short window.
    let mut status = None;
    let deadline = Instant::now() + POLL_CEILING;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(st)) => {
                status = Some(st);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    match status {
        Some(st) => assert!(
            st.signal().is_some(),
            "watch pid should be terminated by the SIGUSR1 nudge, exited normally: {st:?}"
        ),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("clear-attention did not signal the advertised @tma_watch_pid");
        }
    }
}
