//! Attention + episode wiring acceptance on a scratch tmux server: a blocked publish sets
//! `@agent_attention` and `clear-attention` removes it; the notification dedup survives a simulated
//! daemon restart (a cold `tma event` whose only record is the persisted `@agent_notified_at`) and
//! does NOT re-fire; a pid-change episode boundary clears both through `debug stamp --episode-reset`.
//! Runs on a scratch `tmux -L` server, killed on drop. Most cases invoke `clear-attention` directly
//! (a simulated hook), but one drives the REAL installed `after-select-pane` hook end to end, which
//! is what the pane-argument regression below needs.

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

/// The installed `after-select-pane` hook must actually clear the pane it selects.
///
/// It did not, from the first release until this test: the hook passed `#{hook_pane}`, which tmux
/// populates only on the notify_pane-style hooks. On the `after-select-*` command hooks it expands
/// EMPTY, and `clear-attention ''` is a no-op — so the always-on pair cleared nothing for anyone,
/// while still looking installed and still firing its watch nudge. The old assertion only checked
/// that the hook TEXT contained `clear-attention`, which the broken shape satisfied.
///
/// Deliberately end-to-end through `set-hook` + `select-pane` rather than calling the subcommand:
/// the defect lived entirely in the format string, so any test that supplies the pane itself is
/// blind to it. No attached client is needed — verified on tmux 3.6a that `after-select-pane` fires
/// and `#{pane_id}` resolves on a detached server.
#[test]
fn select_pane_hook_clears_attention_on_the_selected_pane() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("hookclear");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "s1", "exec sleep 100000"])
        .status
        .success());
    let first = s.get("s1.0", "#{pane_id}");
    let second = s.get("s1.1", "#{pane_id}");
    assert_ne!(first, second, "need two distinct panes");

    // Park on `second` BEFORE the hook exists, so no in-flight clear can be confused for this one,
    // and so the only pane the coming hook can clear as a DEPARTURE is `second` — which carries no
    // flag. What clears `first` below can therefore only be the arrival clear.
    select_pane(&s, &second);
    install_hook(&s, "after-select-pane");

    s.set_opt(&first, "@agent_attention", "1");
    select_pane(&s, &first);

    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&first, "@agent_attention")
            .is_empty()),
        "selecting the pane must clear its attention flag, but it is still {:?}",
        s.pane_option(&first, "@agent_attention")
    );
}

// ---- seen-on-leave ------------------------------------------------------------------

/// The hook command as `install-hooks` writes it, minus the late-binding `-x` test (the test binary
/// is always executable and always the one we mean). The `TMA_HOOK_KIND` prefix is the part under
/// test: it is what tells `clear-attention` which departure format to resolve.
fn install_hook(s: &Scratch, hook: &str) {
    let cmd = format!(
        "run-shell \"TMA_HOOK_KIND={hook} '{0}' clear-attention '#{{pane_id}}' 2>/dev/null \
         || true\"",
        bin()
    );
    assert!(s.tmux(&["set-hook", "-g", hook, &cmd]).status.success());
}

/// Run `clear-attention` the way the hook's `sh` would, but from this process: `TMA_HOOK_KIND` in the
/// environment, the arrival pane in argv, and `TMUX_PANE` explicitly absent so tmux cannot infer the
/// query's target from the environment instead of from the argument we pass it.
fn clear_attention_as_hook(s: &Scratch, arrival: &str, hook: &str) {
    let out = Command::new(bin())
        .args(["clear-attention", arrival, "--socket-name", &s.socket])
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_HOOK_KIND", hook)
        .env_remove("TMUX_PANE")
        .output()
        .expect("spawn clear-attention");
    assert!(out.status.success());
}

fn select_pane(s: &Scratch, pane: &str) {
    assert!(s.tmux(&["select-pane", "-t", pane]).status.success());
}

/// Raise the flag on `pane`, run `nav`, and wait until `witness`'s flag comes off — proving the hook
/// ran to completion. Without that witness a "the flag survived" assertion passes whenever the hook
/// silently did nothing, which is the failure mode these tests exist to catch. Returns whether
/// `pane`'s flag survived.
fn survives_navigation(s: &Scratch, pane: &str, witness: &str, nav: impl FnOnce()) -> bool {
    s.set_opt(pane, "@agent_attention", "1");
    s.set_opt(witness, "@agent_attention", "1");
    nav();
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(witness, "@agent_attention")
            .is_empty()),
        "the hook never ran: the witness pane {witness} still reads {:?}, so this test would \
         have passed vacuously",
        s.pane_option(witness, "@agent_attention")
    );
    s.pane_option(pane, "@agent_attention") == "1"
}

/// Departing a pane clears it. The residue this closes: an agent finishes while you are watching it,
/// you move on, and the flag survives on the pane you were just looking at — counted by `tma status`
/// and offered by `prefix-j` for as long as you stay away.
///
/// This deliberately INVERTS the assertion that stood here before ("selecting a different pane must
/// not clear this one"). That guard was over-clearing protection; its real job is now held by
/// `an_unrelated_windows_pane_switch_leaves_the_flag_standing` below.
#[test]
fn departing_a_pane_clears_its_attention_flag() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("leavepane");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "s1:0", "exec sleep 100000"])
        .status
        .success());
    let watched = s.get("s1:0.0", "#{pane_id}");
    let elsewhere = s.get("s1:0.1", "#{pane_id}");
    assert_ne!(watched, elsewhere);
    // Sit on the watched pane before the hook exists, so the departure below is the first one.
    select_pane(&s, &watched);
    assert_eq!(
        s.get("s1:0.0", "#{pane_active}"),
        "1",
        "the watched pane must be the one we are sitting on"
    );

    install_hook(&s, "after-select-pane");
    s.set_opt(&watched, "@agent_attention", "1");
    select_pane(&s, &elsewhere);

    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "leaving the pane must clear its attention flag, but it is still {:?}",
        s.pane_option(&watched, "@agent_attention")
    );
}

/// The window-switch half: `after-select-window` clears the active pane of the window you left,
/// which is the pane you were actually looking at.
#[test]
fn departing_a_window_clears_the_pane_it_was_showing() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("leavewin");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "s1:0", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["new-window", "-d", "-t", "s1:", "exec sleep 100000"])
        .status
        .success());
    // Sit on the SECOND pane of window 0, so a passing test cannot be explained by the format
    // naming the window's first pane rather than its active one.
    let watched = s.get("s1:0.1", "#{pane_id}");
    select_pane(&s, &watched);

    install_hook(&s, "after-select-window");
    s.set_opt(&watched, "@agent_attention", "1");
    assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());

    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "leaving the window must clear the pane it was showing, but it is still {:?}",
        s.pane_option(&watched, "@agent_attention")
    );
}

/// The over-clearing guard, moved rather than dropped: navigating between two panes of an unrelated
/// window must leave a flag standing everywhere else. A departure clear that resolved both formats
/// on every hook would fail here, because the other window's active pane is `window_last_flag`'s
/// answer on a plain pane switch.
#[test]
fn an_unrelated_windows_pane_switch_leaves_the_flag_standing() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("unrelated");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["new-window", "-d", "-t", "s1:", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "s1:1", "exec sleep 100000"])
        .status
        .success());
    let bystander = s.get("s1:0.0", "#{pane_id}");
    let here = s.get("s1:1.0", "#{pane_id}");
    let there = s.get("s1:1.1", "#{pane_id}");

    // Move into window 1 BEFORE the hook exists, so nothing has to be waited out.
    assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());
    select_pane(&s, &here);
    install_hook(&s, "after-select-pane");

    let survived = survives_navigation(&s, &bystander, &here, || select_pane(&s, &there));
    assert!(
        survived,
        "a pane switch in another window must not clear {bystander}: an agent finishing in a \
         window you are not in is exactly the signal this flag carries"
    );
}

/// The departure look-up must answer for the arrival pane's OWN session. The formats resolve through
/// `display-message -t <arrival>`, and that target is not decoration: an untargeted
/// `display-message` answers for whichever session tmux picks as "best", which on 3.6a is neither
/// stable nor the session the hook fired in (probed both ways — first-created won once,
/// last-created won another time, under otherwise similar setups).
///
/// Driven by direct invocation rather than through a hook, and with `TMUX_PANE` removed, because
/// tmux's own environment would otherwise pin the query for us and hide a missing `-t`. Both
/// directions are asserted: whichever session tmux would guess, one of the two arms sees the wrong
/// pane cleared.
#[test]
fn the_departure_lookup_stays_inside_the_arrival_panes_session() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("crosssess");
    for name in ["s1", "s2"] {
        assert!(s
            .tmux(&["new-session", "-d", "-s", name, "exec sleep 100000"])
            .status
            .success());
        assert!(s
            .tmux(&[
                "split-window",
                "-t",
                &format!("{name}:0"),
                "exec sleep 100000"
            ])
            .status
            .success());
    }
    // In each session: sit on pane 0, having last been on pane 1. Pane 1 is that session's
    // `pane_last`, i.e. exactly what a departure look-up scoped to it would name.
    let arrivals = [s.get("s1:0.0", "#{pane_id}"), s.get("s2:0.0", "#{pane_id}")];
    let departed = [s.get("s1:0.1", "#{pane_id}"), s.get("s2:0.1", "#{pane_id}")];
    for (arrival, last) in arrivals.iter().zip(&departed) {
        select_pane(&s, last);
        select_pane(&s, arrival);
    }

    for i in 0..2 {
        let (mine, theirs) = (i, 1 - i);
        s.set_opt(&departed[mine], "@agent_attention", "1");
        s.set_opt(&departed[theirs], "@agent_attention", "1");
        clear_attention_as_hook(&s, &arrivals[mine], "after-select-pane");
        assert_eq!(
            s.pane_option(&departed[mine], "@agent_attention"),
            "",
            "arriving at {} must clear its own session's departed pane",
            arrivals[mine]
        );
        assert_eq!(
            s.pane_option(&departed[theirs], "@agent_attention"),
            "1",
            "arriving at {} cleared {} in the OTHER session — the departure query is answering \
             for the wrong session",
            arrivals[mine],
            departed[theirs]
        );
        s.set_opt(&departed[theirs], "@agent_attention", "");
    }
}

/// Walk-away, the tool's headline use case: a flag raised AFTER you left a pane must survive, and a
/// flag raised on the pane you are sitting on must survive indefinitely. Both hold structurally —
/// staying put means no hook fires — but nothing asserted it before, and the seen-on-leave clear is
/// the change that could break it.
#[test]
fn a_flag_raised_after_the_departure_survives() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("walkaway");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    for _ in 0..2 {
        assert!(s
            .tmux(&["split-window", "-t", "s1:0", "exec sleep 100000"])
            .status
            .success());
    }
    let left = s.get("s1:0.0", "#{pane_id}");
    let middle = s.get("s1:0.1", "#{pane_id}");
    let right = s.get("s1:0.2", "#{pane_id}");

    select_pane(&s, &left);
    install_hook(&s, "after-select-pane");

    // Leave `left` for `middle`. Its flag comes off — that is `departing_a_pane...` again, asserted
    // here only to prove the hook is live before the interesting part.
    s.set_opt(&left, "@agent_attention", "1");
    select_pane(&s, &middle);
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&left, "@agent_attention")
            .is_empty()),
        "precondition: the departure clear must be working"
    );

    // Now the agent in the pane you walked away from finishes. Moving on again departs `middle`,
    // not `left`, so `left`'s new flag has to stand.
    let survived = survives_navigation(&s, &left, &middle, || select_pane(&s, &right));
    assert!(
        survived,
        "a flag raised after you left {left} must survive later navigation elsewhere — this is \
         the walk-away signal the whole tool exists to carry"
    );

    // And the purest form: raised on the pane you are sitting on, with no navigation at all.
    s.set_opt(&right, "@agent_attention", "1");
    assert_eq!(
        s.get("s1:0.2", "#{pane_active}"),
        "1",
        "precondition: sitting on the pane"
    );
    assert!(
        !tma_test_support::wait_until(std::time::Duration::from_millis(300), || s
            .pane_option(&right, "@agent_attention")
            .is_empty()),
        "sitting on a pane must never clear it: focus alone is not 'seen'"
    );
}
