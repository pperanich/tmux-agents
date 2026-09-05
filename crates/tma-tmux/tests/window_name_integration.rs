//! `[daemon] window_names` against a real tmux server: the window read, the rename chain, and the
//! restore that has to put the user's window back exactly as it was.
//!
//! The scratch `tmux -L tma_test_<unique>` server (started `-f /dev/null`) is killed on drop; the
//! default server, with the user's live agents on it, is never touched.
//!
//! What earns a live server here is `automatic-rename`: `rename-window` turns it off as a side
//! effect, and whether it was set at WINDOW scope or inherited is invisible to a unit test.

use tma_core::stamp::{opt, AUTORENAME_UNSET};
use tma_test_support as common;
use tma_tmux::tmux::Tmux;
use tma_tmux::window_name::{rename_commands, restore_commands, SavedOriginal};

use common::Scratch;

/// A scratch server with one detached session whose single window is named `shell`.
fn scratch(tag: &str) -> (Scratch, Tmux, String, String) {
    let s = Scratch::new(tag);
    let pane = s.new_pane();
    assert!(s
        .tmux(&["rename-window", "-t", "s1", "shell"])
        .status
        .success());
    let window = s.get("s1", "#{window_id}");
    assert!(window.starts_with('@'), "got window {window:?}");
    // The rename above pinned `automatic-rename off` at window scope, which is tmux's own side
    // effect and exactly what this suite is about. Put the window back to inheriting so each test
    // decides its own starting point.
    assert!(s
        .tmux(&["set-option", "-w", "-t", &window, "-u", "automatic-rename"])
        .status
        .success());
    let tmux = Tmux::new(Some(s.socket.clone()));
    (s, tmux, window, pane)
}

fn window_option(s: &Scratch, window: &str, key: &str) -> String {
    let out = s.tmux(&["show-options", "-wqv", "-t", window, key]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_window_read_carries_the_agent_fields_and_the_bookkeeping() {
    if !common::tmux_available() {
        return;
    }
    let (s, tmux, window, pane) = scratch("winname_read");
    s.set_opt(&pane, opt::NAME, "claude");
    s.set_opt(&pane, opt::STATE, "blocked");
    s.set_opt(&pane, opt::DETAIL, "permission");

    let rows = tmux.list_window_pane_rows().expect("read window rows");
    let row = rows
        .iter()
        .find(|r| r.window_id == window)
        .expect("the window is in the read");
    assert_eq!(row.window_name, "shell");
    assert_eq!(row.agent.as_deref(), Some("claude"));
    assert_eq!(row.state.as_deref(), Some("blocked"));
    assert_eq!(row.detail.as_deref(), Some("permission"));
    assert!(row.cwd.is_some(), "the pane reports a working directory");
    // Nothing renamed yet: every bookkeeping option reads as absent.
    assert_eq!(row.name_orig, None);
    assert_eq!(row.autorename_orig, None);
    assert_eq!(row.name_last, None);
}

#[test]
fn a_rename_round_trips_an_inherited_automatic_rename() {
    if !common::tmux_available() {
        return;
    }
    let (s, tmux, window, _pane) = scratch("winname_inherit");
    // Inherited, not set at window scope: the state the restore has to reproduce by unsetting.
    assert_eq!(window_option(&s, &window, "automatic-rename"), "");
    assert_eq!(
        tmux.window_automatic_rename(&window).expect("read"),
        None,
        "an inherited option reads as absent at window scope"
    );

    tmux.apply(&rename_commands(
        &window,
        "tma:blocked",
        Some(SavedOriginal {
            name: "shell".into(),
            automatic_rename: None,
        }),
    ))
    .expect("rename");

    assert_eq!(s.get(&window, "#{window_name}"), "tma:blocked");
    assert_eq!(window_option(&s, &window, opt::WINDOW_NAME_ORIG), "shell");
    assert_eq!(
        window_option(&s, &window, opt::WINDOW_AUTORENAME_ORIG),
        AUTORENAME_UNSET
    );
    assert_eq!(
        window_option(&s, &window, opt::WINDOW_NAME_LAST),
        "tma:blocked"
    );
    // tmux's own side effect: renaming a window pins `automatic-rename` off at window scope.
    assert_eq!(window_option(&s, &window, "automatic-rename"), "off");

    // A second rename writes the name and the marker only, leaving the saved original alone.
    tmux.apply(&rename_commands(&window, "tma:idle", None))
        .expect("second rename");
    assert_eq!(s.get(&window, "#{window_name}"), "tma:idle");
    assert_eq!(window_option(&s, &window, opt::WINDOW_NAME_ORIG), "shell");

    tmux.apply(&restore_commands(&window, "shell", Some(AUTORENAME_UNSET)))
        .expect("restore");

    assert_eq!(s.get(&window, "#{window_name}"), "shell");
    assert_eq!(
        window_option(&s, &window, "automatic-rename"),
        "",
        "an inherited option is restored by unsetting it, not by pinning a value"
    );
    assert_eq!(s.get(&window, "#{automatic-rename}"), "1");
    for key in [
        opt::WINDOW_NAME_ORIG,
        opt::WINDOW_AUTORENAME_ORIG,
        opt::WINDOW_NAME_LAST,
    ] {
        assert_eq!(window_option(&s, &window, key), "", "{key} is dropped");
    }
}

#[test]
fn a_rename_round_trips_an_explicit_automatic_rename() {
    if !common::tmux_available() {
        return;
    }
    let (s, tmux, window, _pane) = scratch("winname_explicit");
    assert!(s
        .tmux(&["set-option", "-w", "-t", &window, "automatic-rename", "off"])
        .status
        .success());

    let saved = tmux.window_automatic_rename(&window).expect("read");
    assert_eq!(saved.as_deref(), Some("off"));
    tmux.apply(&rename_commands(
        &window,
        "tma:working",
        Some(SavedOriginal {
            name: "shell".into(),
            automatic_rename: saved,
        }),
    ))
    .expect("rename");
    assert_eq!(
        window_option(&s, &window, opt::WINDOW_AUTORENAME_ORIG),
        "off"
    );

    tmux.apply(&restore_commands(&window, "shell", Some("off")))
        .expect("restore");
    assert_eq!(s.get(&window, "#{window_name}"), "shell");
    assert_eq!(
        window_option(&s, &window, "automatic-rename"),
        "off",
        "an explicitly-set value is put back as it was"
    );
}

#[test]
fn a_name_with_shell_metacharacters_survives_the_chain() {
    if !common::tmux_available() {
        return;
    }
    let (s, tmux, window, _pane) = scratch("winname_quoting");
    // The chain is spawned without a shell, so `;` and `$` are plain argv bytes. A `;` would
    // otherwise split the chained invocation into two commands.
    let name = "a;b $x";
    tmux.apply(&rename_commands(
        &window,
        name,
        Some(SavedOriginal {
            name: "shell".into(),
            automatic_rename: None,
        }),
    ))
    .expect("rename");
    assert_eq!(s.get(&window, "#{window_name}"), name);
    assert_eq!(window_option(&s, &window, opt::WINDOW_NAME_LAST), name);
}
