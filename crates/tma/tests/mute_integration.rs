//! `tma mute` acceptance on a scratch `tmux -L` server (killed on drop). The mechanism is one pane
//! option (`@agent_mute_until`), so the assertions read it back directly; exit codes are the shared
//! contract (`0` applied, `3` nothing matched). Selector-resolved panes wear a hand-written claude
//! stamp with a widened freshness window, which is what keeps the cycle on its consumer path.

use std::process::{Command, Output};

use tma_test_support::{AttachOutcome, Scratch};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// Run `tma mute <args>` against the scratch server, its `agents/` manifest dir, and the workdir
/// config ([`hold_the_stamp`] writes it before any selector-resolved case).
fn mute(s: &Scratch, args: &[&str]) -> Output {
    Command::new(s.bin())
        .arg("mute")
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMA_CONFIG", s.config_path())
        .output()
        .expect("spawn tma mute")
}

/// Widen the stamp-freshness window: the pane is a shell wearing a hand-written claude stamp, and
/// only the consumer path keeps that fiction alive. Under parallel load the default 3 s can lapse
/// between the stamp and the invocation, which would let the producer path correctly unmask it.
fn hold_the_stamp(s: &Scratch) {
    s.write_config("[fold]\nfreshness_secs = 600\n");
}

/// Stamp `pane` as a fresh IDLE claude agent. `--state done` is then idle + `@agent_attention`, so
/// the marker alone decides the match — which is what the clear ordering turns on.
fn stamp_idle_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

/// `--state done` must match a pane whose marker `mute`'s OWN target-resolution cycle would
/// otherwise have retracted. `done` is idle + `@agent_attention`, and the ordered-input clear used
/// to run inside that cycle: a client parked on the pane plus one keystroke after the raise arms the
/// clear, the cycle strips the flag before the selector reads the rows, and the pane the operator
/// asked to silence resolves to nothing (exit 3) — the one pane most worth muting, missed.
#[test]
fn mute_does_not_retract_the_done_mark_it_is_selecting_on() {
    if !have_tmux() {
        return;
    }
    let mut s = Scratch::new("mute_seen");
    hold_the_stamp(&s);
    let pane = s.new_shell_pane();
    let session = s.display(&pane, "#{session_name}");
    stamp_idle_claude(&s, &pane);

    match s.attach_client(&session) {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    assert_eq!(
        s.displayed_pane(),
        pane,
        "the attached client must be displaying the agent pane"
    );

    // The completion goes up AFTER the attach, so nothing the attach itself did to
    // `client_activity` can arm the clear — only the keystroke below can.
    let raised = tma_runtime::now_ms();
    s.set_opt(&pane, "@agent_since", &raised.to_string());
    s.set_opt(&pane, "@agent_attention", "1");
    // `stamped_at` moves with `since`: a stamp whose `since` postdates it reads as a torn write,
    // which would take this shell-wearing-a-claude-stamp off the consumer path and unmask it.
    s.set_opt(&pane, "@agent_stamped_at", &raised.to_string());
    // The keystroke after it: this, and only this, is what arms the clear against this pane.
    s.type_client_input_past(raised);

    let out = mute(&s, &["--state", "done", "--for", "30s"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--state done must resolve the pane whose mark was standing when the cycle read it: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&pane),
        "the applied line names the muted pane: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !s.pane_option(&pane, "@agent_mute_until").is_empty(),
        "the mute deadline actually landed on the pane"
    );
    // Ordered, not skipped: the clear still runs, just after the selector read. If this ever reads
    // `1` someone has "fixed" the match by dropping the clear, and a marker on a pane its owner is
    // typing into would stand until something else happens to poll.
    assert_eq!(
        s.pane_option(&pane, "@agent_attention"),
        "",
        "the resolution cycle still retires the marker it read"
    );
}
