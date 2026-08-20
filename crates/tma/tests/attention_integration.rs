//! Attention + episode wiring acceptance on a scratch tmux server: a blocked publish sets
//! `@agent_attention` and `clear-attention` removes it; the notification dedup survives a simulated
//! daemon restart (a cold `tma event` whose only record is the persisted `@agent_notified_at`) and
//! does NOT re-fire; a pid-change episode boundary clears both through `debug stamp --episode-reset`.
//! Runs on a scratch `tmux -L` server, killed on drop. Most cases invoke `clear-attention` directly
//! (a simulated hook), but several drive the REAL installed hooks end to end, which
//! is what the pane-argument regression below needs.

use std::io::Write;
use std::process::{Command, Stdio};

use common::{AttachOutcome, Scratch};
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
/// populates only on the notify_pane-style hooks. On the focus hooks tma installs it expands
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

/// The window-departure hook `install-hooks` writes, and the reason it is not `after-select-window`:
/// tmux runs THAT one even for a `select-window` onto the window you are already in, where
/// `window_last_flag` still names a window left long ago. `session-window-changed` fires only when a
/// session's current window really changed. Both regression tests below turn on that difference.
const WINDOW_HOOK: &str = "session-window-changed";

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

/// The name of the one attached client, for the `-c` of a `switch-client` / `detach-client`. Every
/// caller has just attached one and needs to act on it by name, since these servers grow a second
/// (control-mode) client in one test.
fn attached_client(s: &Scratch) -> String {
    let name = String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{client_name}"]).stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    assert!(!name.is_empty(), "need the attached client's name");
    name
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

/// The window-switch half: `session-window-changed` clears the active pane of the window you left,
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

    install_hook(&s, WINDOW_HOOK);
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

/// Selecting the window you are ALREADY in must leave the window you left alone.
///
/// This is the R-C failure, and it was not exotic: `tma jump --attention` onto a pane in your own
/// window, `prefix <N>` onto the current window, `choose-tree` onto it, any script running
/// `select-window -t :0`. tmux runs `after-select-window` for all of those, and `window_last_flag`
/// there still names whatever window you left this morning — so seen-on-leave cleared a done marker
/// on a pane you had not looked at since. The fix is the hook name: `session-window-changed` is a
/// notification of a real change and is simply never emitted for a no-op.
///
/// The liveness proof is the genuine switch at the end rather than a witness cleared by the no-op
/// itself: a hook that never fires is the CORRECT behaviour for the no-op, so nothing about it can
/// distinguish "installed and quiet" from "not installed at all". If the hook were dead, misnamed,
/// or no longer resolving a departure, the last assertion fails.
#[test]
fn a_no_op_window_selection_leaves_the_window_you_left_alone() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("noopwin");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["split-window", "-t", "s1:0", "exec sleep 100000"])
        .status
        .success());
    for _ in 0..2 {
        assert!(s
            .tmux(&["new-window", "-d", "-t", "s1:", "exec sleep 100000"])
            .status
            .success());
    }
    // `far` is the window-0 pane an agent finished in, on the second pane so a pass cannot be
    // explained by a format naming the window's first pane.
    let far = s.get("s1:0.1", "#{pane_id}");
    let near = s.get("s1:1.0", "#{pane_id}");

    // Establish the history the stale format reads: leave window 0 for window 1, before the hook
    // exists, so window 0 is the session's `last` window from here on.
    select_pane(&s, &far);
    assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());
    install_hook(&s, WINDOW_HOOK);

    // Two agents finish: one in the window you are sitting in, one in the window you left earlier.
    s.set_opt(&far, "@agent_attention", "1");
    s.set_opt(&near, "@agent_attention", "1");

    // Jump to the near one. You are already in its window, so this select-window changes nothing.
    assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());
    assert!(
        !tma_test_support::wait_until(std::time::Duration::from_millis(400), || s
            .pane_option(&far, "@agent_attention")
            .is_empty()),
        "selecting the window you are already in cleared {far}, a pane in the window you left \
         long ago and have not seen since — that is the done marker this tool exists to carry"
    );

    // Now leave window 1 for real. The departure clear must land on the pane it was showing, which
    // both proves the hook was live all along and re-asserts the seen-on-leave behaviour.
    assert!(s.tmux(&["select-window", "-t", "s1:2"]).status.success());
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&near, "@agent_attention")
            .is_empty()),
        "the hook never ran: {near} still reads {:?}, so the no-op assertion above proved nothing",
        s.pane_option(&near, "@agent_attention")
    );
    assert_eq!(
        s.pane_option(&far, "@agent_attention"),
        "1",
        "leaving window 1 must clear window 1, not the window before it"
    );
}

/// An `after-select-window` hook string left on a server by an older install must be harmless: this
/// binary no longer recognizes that hook name, so it can only clear the pane you ARRIVED at, which
/// is the behaviour that shipped before seen-on-leave existed. Belt to the retirement sweep's
/// braces — `install-hooks` removes the entry, but a server whose user has not re-run it (or whose
/// hooks were restored from an old config) keeps firing the string against the new binary.
///
/// The arrival clear is this test's witness: it proves the hook fired at all, which is exactly what
/// makes the surviving flag on `far` meaningful.
#[test]
fn the_retired_window_hook_can_only_clear_the_pane_you_arrived_at() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("retiredwin");
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
    let far = s.get("s1:0.1", "#{pane_id}");
    let near = s.get("s1:1.0", "#{pane_id}");

    select_pane(&s, &far);
    assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());
    install_hook(&s, "after-select-window");

    // The no-op fires this hook (that is the tmux behaviour under test); the arrival pane's flag
    // coming off is what proves it ran.
    let survived = survives_navigation(&s, &far, &near, || {
        assert!(s.tmux(&["select-window", "-t", "s1:1"]).status.success());
    });
    assert!(
        survived,
        "a retired hook string still on the server cleared {far}: `after-select-window` fires on a \
         no-op selection with a stale `window_last_flag`, so it must resolve no departure at all"
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

/// Leaving a whole SESSION is the one departure scope tma deliberately does not clear, and this is
/// one of the three guards on that decision (the others are `pane_focus_out_is_not_a_hook_tma_installs`
/// and the two `pane-focus-out` characterisation tests below). `client-session-changed` fires even
/// for `switch-client -t <the session you are already on>`, where `client_last_session` still names
/// a session left however long ago — the same shape as the retired `after-select-window`. So the
/// name maps to no departure, and a hook string someone wires onto it by hand can only clear the
/// pane it arrived at.
///
/// Driven through the real hook with a real PTY client, because that is what makes the test
/// non-vacuous: the client genuinely switched from `s1` to `s2`, so `client_last_session` really
/// names `s1` and a future session arm would really resolve and clear `watched`.
#[test]
fn a_hand_wired_session_hook_can_only_clear_the_pane_you_arrived_at() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new("sesshook");
    for name in ["s1", "s2"] {
        assert!(s
            .tmux(&["new-session", "-d", "-s", name, "exec sleep 100000"])
            .status
            .success());
    }
    let watched = s.get("s1:0.0", "#{pane_id}");
    let arrival = s.get("s2:0.0", "#{pane_id}");

    // An attach is ITSELF a `client-session-changed`. Installing the hook BEFORE the attach turns
    // that into a free liveness sentinel: the flag below must come off the pane the attach arrives
    // at, which is positive proof the hook is wired and firing before the part under test begins.
    // Installing afterwards would prove nothing until the switch, and may also race the attach's
    // own firing through tmux's notification queue.
    install_hook(&s, "client-session-changed");
    s.set_opt(&watched, "@agent_attention", "1");
    match s.attach_client("s1") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "precondition: attaching fires the hook and clears the pane it arrives at, and nothing \
         below is meaningful until that firing has been consumed"
    );
    let client = attached_client(&s);

    // `arrival` is the witness: the arrival clear must take its flag down, or a hook that silently
    // did nothing would pass this test.
    let survived = survives_navigation(&s, &watched, &arrival, || {
        assert!(s
            .tmux(&["switch-client", "-c", &client, "-t", "s2"])
            .status
            .success());
    });
    assert!(
        survived,
        "a hook wired onto `client-session-changed` cleared {watched}, the pane the departed \
         session was showing. That is a session departure, and tma does not resolve one: the same \
         hook fires with a STALE `client_last_session` on a no-op `switch-client -t <current \
         session>`, which is what tma's own `Tmux::focus` issues on every same-session jump. Read \
         `DepartureKind::from_hook_name` and ARCHITECTURE.md before making this pass"
    );
}

// ---- why `pane-focus-out` is not the answer to the session gap ------------------------

/// `pane-focus-out` is the hook the session-departure gap keeps almost being closed with, and on a
/// default server it looks perfect: on `focus-events off` it fires on a genuine session switch and
/// on none of the three no-ops, naming the departed pane straight in `#{pane_id}`. This test pins
/// the first of the two measured reasons tma still does not install it.
///
/// A clean `detach-client` fires the same edge. Detaching is the "leave it running and come back
/// tomorrow" flow the done mark exists for, and wiring the hook would take the mark down on the way
/// out — while a client that is KILLED rather than detached leaves it standing, so the behaviour
/// would also differ between closing your terminal and losing your ssh connection. Nothing at hook
/// time separates a detach from a session switch: both are one `server_client_set_session` call.
///
/// The assertion is positive (the flag must COME OFF), so it cannot pass vacuously: a hook that
/// never fired would leave the flag up and fail here.
#[test]
fn a_hand_wired_focus_out_hook_clears_a_pane_you_only_detached_from() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new("focusoutdetach");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    let watched = s.get("s1:0.0", "#{pane_id}");
    match s.attach_client("s1") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    let client = attached_client(&s);
    // Installed AFTER the attach, unlike the `client-session-changed` guard above, and for the
    // opposite reason: an attach fires `pane-focus-in`, and a hook already on the server when the
    // client arrives leaves a `clear-attention` in flight that can land after the flag goes up and
    // take the credit the detach is supposed to earn. Installing here means the detach is the only
    // thing that can have fired.
    install_hook(&s, "pane-focus-out");
    s.set_opt(&watched, "@agent_attention", "1");
    assert_eq!(
        s.pane_option(&watched, "@agent_attention"),
        "1",
        "precondition: the flag must still be up going into the detach"
    );
    assert!(s.tmux(&["detach-client", "-t", &client]).status.success());
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "a `pane-focus-out` hook did NOT clear {watched} on detach. If tmux stopped firing that \
         edge on `server_client_set_session(c, NULL)`, one of the two reasons the session gap \
         stays open has gone away and the decision is worth reopening — see ARCHITECTURE.md"
    );
}

/// The second measured reason, and the decisive one: tmux fires `pane-focus-out` only when NO
/// attached client still has that window current. A control-mode client counts as an attached,
/// focused viewer (E2), and tma's daemon parks exactly one on every monitored session — so the
/// session-departure clear would be silently inert for daemon users while the pane and window
/// clears kept working. A departure rule whose existence depends on whether the daemon is running
/// is not a rule that can be written down.
///
/// Liveness is proved the C6 way rather than with a witness pane: the correct behaviour under the
/// control client is that no hook runs at all, so nothing about that half can distinguish "wired
/// and suppressed" from "not wired". The second switch, after the control client is gone, is the
/// proof — it must clear.
#[test]
fn a_control_mode_client_suppresses_the_session_departure_focus_out() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new("focusoutctrl");
    for name in ["s1", "s2"] {
        assert!(s
            .tmux(&["new-session", "-d", "-s", name, "exec sleep 100000"])
            .status
            .success());
    }
    let watched = s.get("s1:0.0", "#{pane_id}");
    match s.attach_client("s1") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    let client = attached_client(&s);
    // After the attach, for the reason spelled out in the detach test above.
    install_hook(&s, "pane-focus-out");

    // The daemon's shape: `tmux -C attach-session -t <session>`, no tty. stdin stays a live pipe
    // (control mode reads commands from it and exits on EOF); stdout goes to null rather than a
    // pipe nobody drains, so a chatty server cannot wedge the client on a full buffer.
    let mut control = Command::new("tmux")
        .args(["-L", &s.socket, "-C", "attach-session", "-t", "s1"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn control-mode client");
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || {
            s.get("s1:0.0", "#{session_attached}") == "2"
        }),
        "precondition: the control client must count as a second attached viewer of s1"
    );

    s.set_opt(&watched, "@agent_attention", "1");
    assert!(s
        .tmux(&["switch-client", "-c", &client, "-t", "s2"])
        .status
        .success());
    assert!(
        !tma_test_support::wait_until(std::time::Duration::from_millis(600), || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "tmux fired `pane-focus-out` for {watched} while a control client was still viewing s1. \
         That is the opposite of what E2 and `window_pane_update_focus` say, and it would make \
         `pane-focus-out` a live candidate for the session gap again — reopen the decision"
    );

    // Liveness: the same switch, with nothing else viewing s1, must clear.
    assert!(s
        .tmux(&["switch-client", "-c", &client, "-t", "s1"])
        .status
        .success());
    let _ = control.kill();
    let _ = control.wait();
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || {
            s.get("s1:0.0", "#{session_attached}") == "1"
        }),
        "the control client must be gone before the liveness half"
    );
    s.set_opt(&watched, "@agent_attention", "1");
    assert!(s
        .tmux(&["switch-client", "-c", &client, "-t", "s2"])
        .status
        .success());
    assert!(
        tma_test_support::wait_until(tma_test_support::POLL_CEILING, || s
            .pane_option(&watched, "@agent_attention")
            .is_empty()),
        "the hook never fired even with s1 unwatched, so the suppression assertion above proved \
         nothing: this test would have passed vacuously"
    );
}
