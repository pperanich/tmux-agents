//! `tma jump --blocked` / `--back` acceptance across sessions on a scratch server.
//!
//! `switch-client` needs an *attached* client, which a headless test process lacks. We
//! attach one via a small Python PTY-fork helper (a real pseudo-terminal client), the
//! documented "script the attach" step. If `python3` is unavailable the test
//! skips rather than failing spuriously. The scratch `tmux -L tma_test_<unique>` server
//! (`-f /dev/null`) is killed on drop — never the default server.

use std::process::Command;

use tma_test_support::{wait_capture_contains, wait_until, AttachOutcome, Scratch, POLL_CEILING};

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Mirror `jump::origin_key`: `@tma_origin_<sanitized>_<hash>`, the sanitized name (non-alphanumerics
/// to `_`) plus an 8-hex FNV-1a hash of the raw name (disambiguating names that sanitize identically).
/// Kept in sync by hand so the test can assert the stored key.
fn origin_key(client: &str) -> String {
    let sanitized: String = client
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let mut hash: u32 = 0x811c_9dc5;
    for b in client.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("@tma_origin_{sanitized}_{hash:08x}")
}

/// The permission-prompt chrome a blocked-agent pane prints (then sleeps), matched by the
/// authored manifest's blocked rule.
const BLOCKED_CHROME: &str = "\\n\\n\\n\\n\\n\\n\\n\\n\
    ╭──────────────────────────╮\\n\
    │ Do you want to proceed?  │\\n\
    │ ❯ 1. Yes                 │\\n\
    ╰──────────────────────────╯\\n";

/// Spawn a detached session `name` running `cmd`.
fn spawn_session(s: &Scratch, name: &str, cmd: &str) {
    assert!(s
        .tmux(&["new-session", "-d", "-s", name, "-x", "80", "-y", "24", cmd])
        .status
        .success());
}

/// Spawn a detached blocked-agent session `name` (prints the permission chrome, then sleeps) and
/// wait for the chrome to render (which happens-before the `exec sleep`, so the pane's process is
/// settled too). Returns its pane id.
fn spawn_blocked(s: &Scratch, name: &str) -> String {
    spawn_session(
        s,
        name,
        &format!("printf '{BLOCKED_CHROME}'; exec sleep 100000"),
    );
    assert!(
        wait_capture_contains(&s.socket, name, "Do you want to proceed?", POLL_CEILING),
        "{name} pane's blocked chrome did not render"
    );
    s.display(name, "#{pane_id}")
}

/// Author a manifest whose identity + blocked rule match `ref_session`'s real process names. Every
/// session in these tests runs the same `sleep`, so one manifest matches them all.
fn write_blocked_manifest(s: &Scratch, ref_session: &str) {
    let pid = s.display(ref_session, "#{pane_pid}");
    let cc = basename(&s.display(ref_session, "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pid])
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
}

/// Build the `home` (interactive) and `work` (detached, blocked agent) sessions and author a
/// manifest whose blocked rule matches the `work` pane. Returns `(home_pane, work_pane)`.
fn setup_home_work(s: &Scratch) -> (String, String) {
    spawn_session(s, "home", "exec sleep 100000");
    let work_pane = spawn_blocked(s, "work");
    let home_pane = s.display("home", "#{pane_id}");
    write_blocked_manifest(s, "work");
    (home_pane, work_pane)
}

/// Build `home` (interactive) plus `n` detached blocked-agent sessions `s1..=sn`, all matched by
/// one manifest. Returns `(home_pane, [s1_pane, s2_pane, ...])`.
fn setup_home_and_blockers(s: &Scratch, n: usize) -> (String, Vec<String>) {
    spawn_session(s, "home", "exec sleep 100000");
    let panes: Vec<String> = (1..=n)
        .map(|i| spawn_blocked(s, &format!("s{i}")))
        .collect();
    let home_pane = s.display("home", "#{pane_id}");
    write_blocked_manifest(s, "s1");
    (home_pane, panes)
}

fn preflight() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    if !tma_test_support::python3_available() {
        eprintln!("skipping: python3 unavailable for the PTY attach");
        return false;
    }
    true
}

#[test]
fn jump_to_blocked_in_detached_session_then_back() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jump");
    let (home_pane, work_pane) = setup_home_work(&s);

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
    assert_eq!(
        s.display("", "#{pane_id}"),
        home_pane,
        "start attached on home"
    );

    // Jump to the blocked agent in the detached `work` session.
    let out = s.tma(&["jump", "--blocked"]);
    assert!(
        out.status.success(),
        "jump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == work_pane),
        "jump --blocked lands on the blocked agent in the detached session; active pane is {:?}",
        s.display("", "#{pane_id}")
    );

    // Return to the origin.
    let out = s.tma(&["jump", "--back"]);
    assert!(
        out.status.success(),
        "jump --back failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == home_pane),
        "jump --back returns to the origin pane; active pane is {:?}",
        s.display("", "#{pane_id}")
    );
}

/// The jump origin trail: with `--client <name>`, the origin is resolved from and keyed by *that*
/// client, and the jump switches it. A single attached client proves the client-targeted path (the
/// bug lived in targetless resolution); scripting two live clients is flaky, so that stays manual.
#[test]
fn jump_with_explicit_client_keys_origin_by_that_client() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumpc");
    let (home_pane, work_pane) = setup_home_work(&s);

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
    let client = String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{client_name}"]).stdout)
        .trim()
        .to_string();
    assert!(!client.is_empty(), "an attached client is required");
    let home_locator = s.display("home", "#{session_name}:#{window_index}.#{pane_index}");

    // Jump with the explicit acting client; it must switch that client to the blocked agent.
    let out = s.tma(&["jump", "--blocked", "--client", &client]);
    assert!(
        out.status.success(),
        "jump --client failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == work_pane),
        "jump --client switches the passed client to the blocked agent; active pane is {:?}",
        s.display("", "#{pane_id}")
    );

    // The origin must be stored under the passed client's sanitized key.
    let key = origin_key(&client);
    let stored = String::from_utf8_lossy(&s.tmux(&["show-options", "-sqv", &key]).stdout)
        .trim()
        .to_string();
    assert_eq!(
        stored, home_locator,
        "origin recorded under the acting client's key {key}"
    );

    // `--back` with the same client returns it to the recorded origin.
    let out = s.tma(&["jump", "--back", "--client", &client]);
    assert!(out.status.success(), "jump --back --client failed");
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == home_pane),
        "jump --back --client returns the passed client to its origin; active pane is {:?}",
        s.display("", "#{pane_id}")
    );
}

/// The return trail: a forward jump pushes the origin, `--home` returns to the pre-triage origin
/// (the bottom of the trail) and clears it, so a second `--home` has nothing to do. Exercises the
/// real server-option stack read/write/clear path end to end. A single attached client is enough.
#[test]
fn jump_home_returns_to_origin_then_clears_the_trail() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumphome");
    let (home_pane, work_pane) = setup_home_work(&s);

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
    let client = String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{client_name}"]).stdout)
        .trim()
        .to_string();
    assert!(!client.is_empty(), "an attached client is required");
    let key = origin_key(&client);

    // Forward jump: land on the blocked agent, pushing `home` onto the trail.
    let out = s.tma(&["jump", "--blocked", "--client", &client]);
    assert!(out.status.success(), "jump --blocked --client failed");
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == work_pane),
        "forward jump lands on the blocked agent; active pane is {:?}",
        s.display("", "#{pane_id}")
    );

    // `--home` returns to the pre-triage origin and clears the trail.
    let out = s.tma(&["jump", "--home", "--client", &client]);
    assert!(out.status.success(), "jump --home --client failed");
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == home_pane),
        "jump --home returns to the pre-triage origin; active pane is {:?}",
        s.display("", "#{pane_id}")
    );
    let stored = String::from_utf8_lossy(&s.tmux(&["show-options", "-sqv", &key]).stdout)
        .trim()
        .to_string();
    assert!(
        stored.is_empty(),
        "--home clears the trail; stored {key} was {stored:?}"
    );

    // A second `--home` has an empty trail: success, no move, a stderr note.
    let out = s.tma(&["jump", "--home", "--client", &client]);
    assert!(
        out.status.success(),
        "jump --home on an empty trail is not an error"
    );
    assert_eq!(
        s.display("", "#{pane_id}"),
        home_pane,
        "an empty-trail --home leaves the client where it is"
    );
}

/// Attach a PTY client to `session` and return its client name, or `None` when python3 is
/// unavailable (a legitimate skip). A python3-ran-but-no-client outcome panics (a regression).
fn attach_or_skip(s: &mut Scratch, session: &str) -> Option<String> {
    match s.attach_client(session) {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return None;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
    let client = String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{client_name}"]).stdout)
        .trim()
        .to_string();
    assert!(!client.is_empty(), "an attached client is required");
    Some(client)
}

/// A 2+ deep trail round-trips through the newline-joined server option: three forward `--next`
/// jumps build the stack (home → s1 → s2 → s3, pushing home, s1, s2), then `--back` twice retraces
/// the origins in reverse (s3 → s2 → s1). Exercises the real multi-entry stack read/write path.
#[test]
fn multi_jump_trail_backs_in_reverse_order() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumptrail");
    let (_home_pane, blockers) = setup_home_and_blockers(&s, 3);
    let Some(client) = attach_or_skip(&mut s, "home") else {
        return;
    };

    // Three forward `--next` jumps land on s1, s2, s3 in turn.
    for want in &blockers {
        let out = s.tma(&["jump", "--next", "--client", &client]);
        assert!(
            out.status.success(),
            "jump --next failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(POLL_CEILING, || &s.display("", "#{pane_id}") == want),
            "jump --next lands on {want}; active pane is {:?}",
            s.display("", "#{pane_id}")
        );
    }

    // `--back` twice retraces the pushed origins in reverse: s2, then s1.
    for want in blockers.iter().rev().skip(1) {
        let out = s.tma(&["jump", "--back", "--client", &client]);
        assert!(
            out.status.success(),
            "jump --back failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(POLL_CEILING, || &s.display("", "#{pane_id}") == want),
            "jump --back retraces to {want}; active pane is {:?}",
            s.display("", "#{pane_id}")
        );
    }
}

/// A fresh multi-deep trail then `--home` lands at the first (oldest) recorded origin — the bottom
/// of the stack — and clears the trail, proving `--home` reaches past a 2+ deep stack.
#[test]
fn multi_jump_home_lands_at_first_origin() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumptrailhome");
    let (home_pane, blockers) = setup_home_and_blockers(&s, 3);
    let Some(client) = attach_or_skip(&mut s, "home") else {
        return;
    };
    let key = origin_key(&client);

    // Build a 3-deep trail: three forward `--next` jumps (home → s1 → s2 → s3).
    for want in &blockers {
        let out = s.tma(&["jump", "--next", "--client", &client]);
        assert!(
            out.status.success(),
            "jump --next failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(POLL_CEILING, || &s.display("", "#{pane_id}") == want),
            "jump --next lands on {want}; active pane is {:?}",
            s.display("", "#{pane_id}")
        );
    }

    // `--home` returns to the oldest origin (home, the bottom of the stack) and clears the trail.
    let out = s.tma(&["jump", "--home", "--client", &client]);
    assert!(
        out.status.success(),
        "jump --home failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == home_pane),
        "jump --home lands at the first recorded origin; active pane is {:?}",
        s.display("", "#{pane_id}")
    );
    let stored = String::from_utf8_lossy(&s.tmux(&["show-options", "-sqv", &key]).stdout)
        .trim()
        .to_string();
    assert!(
        stored.is_empty(),
        "--home clears the trail; stored {key} was {stored:?}"
    );
}

/// `--pane` is the explicit target the menu entries fire: it focuses that pane wherever it lives,
/// records the origin like every other forward jump (so `--back` returns), and clears the
/// destination's attention flag the way the picker's Enter does.
#[test]
fn jump_to_a_named_pane_records_the_origin_and_clears_attention() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumppane");
    let (home_pane, work_pane) = setup_home_work(&s);
    s.set_opt(&work_pane, "@agent_attention", "1");
    let Some(client) = attach_or_skip(&mut s, "home") else {
        return;
    };

    let out = s.tma(&["jump", "--pane", &work_pane, "--client", &client]);
    assert!(
        out.status.success(),
        "jump --pane failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == work_pane),
        "jump --pane lands on the named pane; active pane is {:?}",
        s.display("", "#{pane_id}")
    );
    assert!(
        wait_until(POLL_CEILING, || s
            .pane_option(&work_pane, "@agent_attention")
            .is_empty()),
        "focusing the pane reviews it, so the attention flag is cleared"
    );

    // The origin went on the trail, so `--back` returns.
    let out = s.tma(&["jump", "--back", "--client", &client]);
    assert!(out.status.success());
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == home_pane),
        "--back returns to where the --pane jump started"
    );
}

/// A jump landing in the window you are already in must not run `select-window` at all.
///
/// tmux runs `after-select-window` even when the selection changes nothing, and at that moment
/// `window_last_flag` names whatever window was left however long ago — so a hook reading it (tma's
/// own, before this, and anyone else's) acts on a window the user never left. Half of all jumps land
/// in the current window: `--attention` to the near one of two, `--back` after a same-window jump,
/// the picker with one window in play.
///
/// The probe hook is a bare `run-shell` appending a byte, not tma's own hook: what is under test is
/// that the tmux command is not issued, independent of what any hook would then do about it. The
/// cross-window jump at the end is the liveness proof — without it, a probe hook that was never
/// installed would look exactly like a jump that correctly skipped the selection.
#[test]
fn jump_within_the_current_window_does_not_reselect_it() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumpsamewin");
    spawn_session(&s, "home", "exec sleep 100000");
    // A blocked agent beside you in window 0, and another one over in window 1.
    let printf = format!("printf '{BLOCKED_CHROME}'; exec sleep 100000");
    assert!(s
        .tmux(&["split-window", "-t", "home:0", &printf])
        .status
        .success());
    assert!(s
        .tmux(&["new-window", "-d", "-t", "home:", &printf])
        .status
        .success());
    for target in ["home:0.1", "home:1.0"] {
        assert!(
            wait_capture_contains(&s.socket, target, "Do you want to proceed?", POLL_CEILING),
            "{target}'s blocked chrome did not render"
        );
    }
    let near = s.display("home:0.1", "#{pane_id}");
    let far = s.display("home:1.0", "#{pane_id}");
    write_blocked_manifest(&s, "home");

    let Some(client) = attach_or_skip(&mut s, "home") else {
        return;
    };
    assert!(s.tmux(&["select-window", "-t", "home:0"]).status.success());
    assert!(s.tmux(&["select-pane", "-t", "home:0.0"]).status.success());

    // A bare probe: one byte per `after-select-window`, whoever caused it.
    let log = s.workdir.join("select-window.log");
    assert!(s
        .tmux(&[
            "set-hook",
            "-g",
            "after-select-window",
            &format!("run-shell \"printf x >> '{}'\"", log.display()),
        ])
        .status
        .success());

    let out = s.tma(&["jump", "--pane", &near, "--client", &client]);
    assert!(
        out.status.success(),
        "jump --pane failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(POLL_CEILING, || s.display("", "#{pane_id}") == near),
        "the jump still has to land: active pane is {:?}",
        s.display("", "#{pane_id}")
    );
    assert!(
        !wait_until(std::time::Duration::from_millis(400), || log.exists()),
        "the jump re-selected the window it was already in; every hook tmux runs for that \
         selection reads a `window_last_flag` naming a window the user never left"
    );

    // Liveness: a jump that really does change window issues the selection, so the probe fires.
    let out = s.tma(&["jump", "--pane", &far, "--client", &client]);
    assert!(out.status.success());
    assert!(
        wait_until(POLL_CEILING, || log.exists()),
        "the probe hook never fired even for a real window change, so the assertion above \
         proved nothing"
    );
}

/// A pane with no agent on it is a clean miss: exit 0 with a note naming the pane, and nothing moves.
#[test]
fn jump_to_a_pane_without_an_agent_reports_the_miss() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("jumppanemiss");
    spawn_session(&s, "home", "exec sleep 100000");
    let out = s.tma(&["jump", "--pane", "%999"]);
    assert!(out.status.success(), "a miss is not a failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no agent in pane %999"),
        "the miss names the pane: {stderr}"
    );
}

/// `--menu` renders a real `tmux display-menu` on the attached client (exit 0). The PTY client
/// presses `q`, which dismisses the overlay so the otherwise-blocking `display-menu` returns; `q`
/// never selects an entry, so nothing jumps. Mirrors the `act --menu` execution test.
#[test]
fn jump_menu_renders_with_attached_client() {
    if !preflight() {
        return;
    }
    let mut s = Scratch::new("jumpmenu");
    let (home_pane, _work_pane) = setup_home_work(&s);

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

    let out = s.tma(&["jump", "--menu"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the menu renders on an attached client; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("no agents") && !stderr.contains("cannot show"),
        "it reached and succeeded at display-menu; stderr: {stderr}"
    );
    assert_eq!(
        s.display("", "#{pane_id}"),
        home_pane,
        "dismissing the menu selects nothing, so no jump happened"
    );
}

/// The negative control for the test above: with no attached client the real `display-menu` answers
/// "no current client", so `--menu` fails with its own note rather than pretending to have shown a
/// menu. This is what makes the attached exit 0 mean "it rendered".
#[test]
fn jump_menu_without_attached_client_fails() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("jumpmenunoclient");
    setup_home_work(&s);

    let out = s.tma(&["jump", "--menu"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("cannot show the jump menu"),
        "the failure names the menu-render step: {stderr}"
    );
}

/// An empty agent list is a clean refusal, not a render: exit 0 with "no agents", and tmux is never
/// asked to draw a menu with no entries (which it rejects).
#[test]
fn jump_menu_with_no_agents_refuses_cleanly() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("jumpmenuempty");
    spawn_session(&s, "home", "exec sleep 100000");

    let out = s.tma(&["jump", "--menu"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("no agents"), "{stderr}");
}
