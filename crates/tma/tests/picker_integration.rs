//! The picker jumps to the highlighted agent when driven via `send-keys`, in a pane and in a popup.
//!
//! The picker runs its own alt-screen TUI, so an attached PTY client is required both for it to read
//! keys and for the Enter-jump's `switch-client`. We attach one via a Python pty-fork helper (as in
//! the jump test). If `python3` is unavailable, or the terminal drive proves flaky in an environment,
//! the test skips — a manual smoke script is documented in the crate README / task report. Scratch
//! `tmux -L tma_test_<unique>` (`-f /dev/null`), killed on drop; never the default server.
//!
//! The popup case is the one users actually press (`prefix a`), and it cannot be driven the same way:
//! a popup's pane is hidden, so neither `send-keys` nor `capture-pane` reaches it. It is driven by
//! typing at the client's real terminal instead, which is also what makes it exercise the binding's
//! key table for real.

use std::process::Command;

use tma_test_support::{poll_until, wait_capture_contains, AttachOutcome, Scratch, POLL_CEILING};

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// The permission prompt a blocked pane prints, matched by the manifest's blocked rule.
const BLOCKED_CHROME: &str = "\\n\\n\\n\\n\\n\\n\\n\\n\
     ╭──────────────────────────╮\\n\
     │ Do you want to proceed?  │\\n\
     │ ❯ 1. Yes                 │\\n\
     ╰──────────────────────────╯\\n";

/// The line an idle pane prints, matched by the manifest's idle rule. Only the two-agent setup
/// prints it, so the blocked-only setup is unaffected by the rule's presence.
const IDLE_CHROME: &str = "agent ready for input";

/// A pane running `sleep` behind `chrome`, waited on until the chrome has rendered.
fn agent_session(s: &Scratch, name: &str, chrome: &str, marker: &str) {
    let cmd = format!("printf '{chrome}'; exec sleep 100000");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "100",
            "-y",
            "30",
            &cmd
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, name, marker, POLL_CEILING),
        "{name} pane chrome did not render"
    );
}

/// Write the scratch manifest: the identity is taken from `sample`'s running process (tmux and `ps`
/// spell it differently on some platforms, so both go in), plus a blocked and an idle rule keyed on
/// the chrome the panes print.
fn write_manifest(s: &Scratch, sample: &str) {
    let pane_pid = s.display(sample, "#{pane_pid}");
    let cc = basename(&s.display(sample, "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pane_pid])
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
             region=\"tail_lines(50)\"\nmatch={{ contains=\"Do you want to proceed?\" }}\n\
             [[rules]]\nstate=\"idle\"\npriority=50\n\
             region=\"tail_lines(50)\"\nmatch={{ contains=\"{IDLE_CHROME}\" }}\n"
        ),
    )
    .unwrap();
}

/// Shared setup: a scratch server with an attached `home` pane to open the picker from and a detached
/// `work` session holding one blocked agent (stamps populated, so the picker's first frame lists it).
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

    // `home`: a shell where the picker will run (attached). `work`: a detached blocked agent.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "-x", "100", "-y", "30"])
        .status
        .success());
    agent_session(&s, "work", BLOCKED_CHROME, "Do you want to proceed?");
    write_manifest(&s, "work");

    // Populate stamps so the picker's first frame already lists the blocked agent.
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "blocked");

    if !attach_home(&mut s) {
        return None;
    }
    Some(s)
}

/// True once the suite's PTY client is attached to `home`; `false` only for the one genuine
/// environment gap (python3 absent), so a real attach regression panics instead of skipping.
fn attach_home(s: &mut Scratch) -> bool {
    match s.attach_client("home") {
        AttachOutcome::Attached => true,
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            false
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
}

/// The self-exclusion setup: the attached `home` pane is itself a *blocked* agent and the detached
/// `work` pane an *idle* one. Blocked sorts first, so a picker that listed `home` would put it under
/// the cursor and Enter would never move the client, which is what makes the jump assertion prove
/// the exclusion rather than the sort order.
fn setup_two_agents(label: &str) -> Option<Scratch> {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return None;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return None;
    }
    let mut s = Scratch::new(label);

    agent_session(&s, "home", BLOCKED_CHROME, "Do you want to proceed?");
    agent_session(&s, "work", IDLE_CHROME, IDLE_CHROME);
    write_manifest(&s, "work");

    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("home", "#{@agent_state}"), "blocked");
    assert_eq!(s.display("work", "#{@agent_state}"), "idle");

    if !attach_home(&mut s) {
        return None;
    }
    Some(s)
}

/// The picker's command line against this scratch server: the shipped popup binding runs a bare
/// `tma`, so the isolation flags are all this adds.
fn picker_command(s: &Scratch) -> String {
    format!(
        "'{}' --socket-name '{}' --manifest-dir '{}'",
        s.bin(),
        s.socket,
        s.workdir.display()
    )
}

/// True while a picker is running against this scratch server. A popup's process lives in a hidden
/// pane that no tmux listing returns, so matching its command line is the only handle on it; the
/// one-shot `tma ls` of the setup has long exited, and the `tmux -L <socket>` server itself carries
/// no `socket-name` argument, so a match is the picker.
fn picker_running(s: &Scratch) -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("socket-name {}", s.socket)])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[test]
fn picker_enter_jumps_to_highlighted_agent() {
    let Some(s) = setup_blocked_agent("picker") else {
        return;
    };
    let work_pane = s.display("work", "#{pane_id}");

    // Launch the picker in the home pane.
    let launch = picker_command(&s);
    s.tmux(&["send-keys", "-t", "home", &launch, "Enter"]);

    // The client attached and the launch keys were sent, so a render timeout here is a real startup
    // regression — hard-fail. The genuine skips (no tmux/python3/PTY client) are handled above.
    poll_until("the picker to render its first frame", || {
        let content = s.tmux(&["capture-pane", "-p", "-t", "home"]);
        String::from_utf8_lossy(&content.stdout).contains("agents (")
    });

    // Enter jumps to the highlighted (sorted-first, blocked) agent.
    s.tmux(&["send-keys", "-t", "home", "Enter"]);

    poll_until(
        &format!("picker Enter to jump the client to the blocked agent {work_pane}"),
        || s.display("", "#{pane_id}") == work_pane,
    );
    // The attention flag for the jumped-to pane is cleared on Enter.
    assert_eq!(
        s.display("work", "#{@agent_attention}"),
        "",
        "picker jump clears the target's attention flag"
    );
}

/// Bind `prefix a` to the shipped popup form of `command` on this scratch server (`-f /dev/null`
/// leaves it with tmux's defaults, which carry no `a` binding).
fn bind_popup(s: &Scratch, command: &str) {
    assert!(s
        .tmux(&[
            "bind-key",
            "a",
            "display-popup",
            "-E",
            "-w",
            "80%",
            "-h",
            "60%",
            command,
        ])
        .status
        .success());
}

/// Open the picker popup for one round: press the binding, wait until the picker is demonstrably
/// alive, then Enter until the client moves.
///
/// A popup gives a test no surface to read: its pane is hidden, so `capture-pane` cannot see the
/// first frame the way the in-pane test does. Dropping the agent's stamp first supplies the missing
/// readiness signal — only the picker's own refresh cycle can put it back, so its return proves the
/// picker is up and folding events, not merely spawned.
fn popup_jump(s: &Scratch, work_pane: &str, label: &str) {
    assert!(s
        .tmux(&["set-option", "-pu", "-t", "work", "@agent_state"])
        .status
        .success());
    // `C-b a`: the prefix key then the binding, typed at the client's real terminal.
    s.send_client_keys("\x02a");
    poll_until(
        &format!("{label}: the popup picker to run a refresh cycle"),
        || s.display("work", "#{@agent_state}") == "blocked",
    );
    poll_until(
        &format!("{label}: popup Enter to jump the client to the blocked agent {work_pane}"),
        || {
            s.send_client_keys("\r");
            s.display("", "#{pane_id}") == work_pane
        },
    );
}

/// The popup the `prefix a` binding opens: pressed at the client's terminal, jumped with Enter at the
/// client's terminal, since a popup takes neither `send-keys` nor `capture-pane`.
///
/// Both binding forms must jump. tmux does not format-expand a `display-popup` shell-command, so the
/// `--client "#{client_name}"` older installs carry arrives as those literal bytes; the picker has to
/// discard it and resolve the invoking client from inside the popup, which is what the shipped
/// binding (no `--client` at all) relies on too.
#[test]
fn picker_enter_jumps_from_the_popup_binding() {
    let Some(s) = setup_blocked_agent("picker_popup") else {
        return;
    };
    let work_pane = s.display("work", "#{pane_id}");
    let client = s.display("", "#{client_name}");

    // The shipped binding's command (BINDINGS key `a` in crates/tma/src/install_keys.rs) and the one
    // installs made before it dropped `--client` still run.
    let shipped = picker_command(&s);
    let legacy = format!("{shipped} --client \"#{{client_name}}\"");

    for (label, command) in [("pre-fix binding", legacy), ("shipped binding", shipped)] {
        s.tmux(&["switch-client", "-c", &client, "-t", "home"]);
        poll_until(&format!("{label}: the client to be back on home"), || {
            s.display("", "#{session_name}") == "home"
        });
        bind_popup(&s, &command);
        let had_attention = s.display("work", "#{@agent_attention}") == "1";

        popup_jump(&s, &work_pane, label);
        // Enter jumps and closes the picker, and with it the popup.
        poll_until(&format!("{label}: the popup to close"), || {
            !picker_running(&s)
        });
        if had_attention {
            assert_eq!(
                s.display("work", "#{@agent_attention}"),
                "",
                "{label}: the popup jump clears the target's attention flag"
            );
        }
    }
}

/// The picker hides the pane it was opened from: jumping to where you already are does nothing.
///
/// Both panes are agents and the invoking one (`home`) is the blocked one, so it would sort first
/// and hold the cursor if it were listed, Enter then never moves the client, and this test fails.
/// The exclusion is display-side only: the stamps dropped before the binding are the popup's
/// readiness signal (a popup's pane is hidden, so there is no frame to capture) and their return
/// shows the refresh cycle still covering `home` while the picker hides it.
///
/// The degenerate case (the only agent is the invoking pane, so the list is empty) is a frame
/// assertion a popup cannot serve; `RowFilter` covers it as a unit test in `crates/tma-ui`.
#[test]
fn picker_popup_hides_the_invoking_pane() {
    let Some(s) = setup_two_agents("picker_self") else {
        return;
    };
    let home_pane = s.display("home", "#{pane_id}");
    let work_pane = s.display("work", "#{pane_id}");
    bind_popup(&s, &picker_command(&s));

    for target in ["home", "work"] {
        assert!(s
            .tmux(&["set-option", "-pu", "-t", target, "@agent_state"])
            .status
            .success());
    }
    s.send_client_keys("\x02a");
    poll_until(
        "the popup picker to restamp both panes, the hidden one included",
        || {
            s.display("home", "#{@agent_state}") == "blocked"
                && s.display("work", "#{@agent_state}") == "idle"
        },
    );

    poll_until(
        &format!("popup Enter to jump the client to {work_pane}, the pane it was not opened from ({home_pane})"),
        || {
            s.send_client_keys("\r");
            s.display("", "#{pane_id}") == work_pane
        },
    );
}
