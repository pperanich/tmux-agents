//! `tma act --menu` execution acceptance: the wiring from the CLI menu mode through
//! `tma_runtime::ui::display_menu` down to the real `tmux display-menu` command. The pure entry
//! builder is unit-tested in `tma-ui`; this covers what only a live server can — that the menu
//! actually renders.
//!
//! The catch: `tmux display-menu` needs an ATTACHED client (a detached scratch server answers "no
//! current client"), and a one-shot `display-menu` then BLOCKS until its menu is closed. So the
//! positive test attaches a PTY client that keeps pressing `q` (via [`Scratch::attach_menu_client`]),
//! which dismisses the overlay as soon as it opens; the `tma act --menu` process then exits 0. The
//! detached negative control proves that exit 0 is specifically the attached client rendering the
//! menu, not some other success path. As in the jump suite, the PTY attach needs `python3`; absent
//! it, the attach-based test skips rather than failing.

use std::process::{Command, Output};

use tma_test_support::{empty_config_path, AttachOutcome, Scratch};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// A detached session `name` running an idle `sleep` (no output to perturb the attached client),
/// returning its pane id.
fn new_agent_session(s: &Scratch, name: &str) -> String {
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get(name, "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    pane
}

/// Stamp `pane` as a fresh `blocked/permission` claude agent, so `approve`/`deny` are fireable and
/// the menu has entries (mirrors the stamp in `act_integration.rs`).
fn stamp_blocked_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

/// Run `tma act --menu` against the scratch server with the user action dir pinned empty
/// (`XDG_CONFIG_HOME` at the workdir) so only the bundled actions load. Mirrors `act_integration.rs`.
fn act_menu(s: &Scratch, pane: &str) -> Output {
    Command::new(s.bin())
        .args(["act", "--menu", "--pane", pane])
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMA_CONFIG", empty_config_path())
        .env("XDG_CONFIG_HOME", &s.workdir)
        .output()
        .expect("spawn tma act --menu")
}

/// With an attached client, `tma act --menu` renders the `display-menu` and exits 0. The
/// self-dismissing PTY client closes the overlay within its lifetime, so the otherwise-blocking
/// `display-menu` returns. Exit 0 with empty stderr proves the menu reached and succeeded at the
/// real `display-menu` (the no-fireable path would print "no actions are fireable"; a render failure
/// would print "cannot show the action menu"). The detached negative control below proves the exit 0
/// is the attached client rendering, not another success path.
#[test]
fn menu_renders_with_attached_client() {
    if !have_tmux() {
        return;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return;
    }
    let mut s = Scratch::new("act_menu");
    let pane = new_agent_session(&s, "home");
    stamp_blocked_claude(&s, &pane);

    match s.attach_menu_client("home") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }

    let out = act_menu(&s, &pane);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the menu renders on an attached client (exit 0); stderr: {stderr}"
    );
    assert!(
        !stderr.contains("no actions are fireable"),
        "the fireable set was non-empty, so it reached display-menu; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("cannot show the action menu"),
        "display-menu succeeded on the attached client; stderr: {stderr}"
    );
    // The dismiss key `q` cancels the menu; it never quick-selects an entry, so no action fired and
    // the single-flight lock is never taken.
    assert!(
        s.pane_option(&pane, "@agent_action").is_empty(),
        "dismissing the menu fires no action, so no lock is left behind"
    );
}

/// The negative control: with NO attached client, the real `tmux display-menu` answers "no current
/// client", so `tma act --menu` fails (exit 1) with its own "cannot show the action menu" note. This
/// is what makes the exit 0 above meaningful — it is specifically the attached client rendering the
/// menu, not construction alone. No client, so no `python3` is needed.
#[test]
fn menu_without_attached_client_fails() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_menu_noclient");
    let pane = new_agent_session(&s, "home");
    stamp_blocked_claude(&s, &pane);

    let out = act_menu(&s, &pane);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a fireable menu with no attached client fails; stderr: {stderr}"
    );
    assert!(
        stderr.contains("cannot show the action menu"),
        "the failure names the menu-render step: {stderr}"
    );
}

/// The no-fireable path: when nothing is fireable on the pane, `tma act --menu` refuses before
/// touching `display-menu` — exit 0 with the "no actions are fireable" note, no client required. An
/// idle claude pane fires nothing (approve/deny need blocked, interrupt needs working, compact needs
/// idle+context telemetry the stamp does not carry).
#[test]
fn menu_with_no_fireable_actions_refuses_cleanly() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_menu_empty");
    let pane = new_agent_session(&s, "home");
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(&pane, "@agent_name", "claude");
    s.set_opt(&pane, "@agent_state", "idle");
    s.set_opt(&pane, "@agent_detail", "none");
    s.set_opt(&pane, "@agent_stamped_at", &now);
    s.set_opt(&pane, "@agent_source", "capture");
    s.set_opt(&pane, "@agent_pid", "4242");

    let out = act_menu(&s, &pane);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "an empty fireable set is a clean refusal, not a failure; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no actions are fireable"),
        "it names the empty-menu refusal: {stderr}"
    );
}
