//! `tma act <text action>` acceptance against a scratch `tmux -L` server: the guarded literal-text
//! path from the CLI through the broker to characters on a pane.
//!
//! The pane runs `cat` rather than a shell, so the assertion is unambiguous. `cat` echoes what is
//! typed at it and then writes the line back once Enter submits it, so a delivered payload appears
//! TWICE and an unsubmitted one appears once, which separates "the text arrived" from "the suffix
//! submitted it". A key, by contrast, produces no text at all: that is what makes the `-l` flag
//! testable rather than merely reviewable.
//!
//! `XDG_CONFIG_HOME` is pinned at the workdir so only the bundled actions load and a developer's
//! own `~/.config/tma/actions` never leaks in.

use std::process::{Command, Output};

use tma_test_support::{empty_config_path, wait_capture_contains, Scratch, POLL_CEILING};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// The marker the `cat` pane prints before it execs, so a test knows the process is reading input
/// rather than still starting up.
const READY: &str = "cat-ready";

/// A detached 80x24 session running `cat`: it echoes what is typed at it and writes the line back
/// on Enter, and it dies on a real `C-c`. Returns the pane id, once `cat` is up.
fn new_cat_pane(s: &Scratch) -> String {
    let cmd = format!("printf '{READY}\\n'; exec cat");
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &cmd])
        .status
        .success());
    let pane = s.get("", "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    assert!(
        wait_capture_contains(&s.socket, &pane, READY, POLL_CEILING),
        "cat must be reading input before the pane is steered"
    );
    pane
}

/// Stamp `pane` as a freshly-seen idle claude agent: `steer`'s gate passes and the stamp is new
/// enough that the freshness re-verify is skipped (no real claude process needed).
fn stamp_idle_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

fn act(s: &Scratch, args: &[&str]) -> Output {
    let config = s.config_path();
    let config = if config.exists() {
        config
    } else {
        empty_config_path().to_path_buf()
    };
    Command::new(s.bin())
        .arg("act")
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMA_CONFIG", config)
        .env("XDG_CONFIG_HOME", &s.workdir)
        .output()
        .expect("spawn tma act")
}

fn capture(s: &Scratch, pane: &str) -> String {
    let out = s.tmux(&["capture-pane", "-p", "-t", pane]);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Does tmux still know this pane? A real `C-c` would have killed `cat` and closed it.
fn pane_alive(s: &Scratch, pane: &str) -> bool {
    s.tmux(&["display-message", "-p", "-t", pane, "#{pane_id}"])
        .status
        .success()
}

/// Poll until `needle` appears at least `want` times in the pane, returning the final count.
fn wait_for_count(s: &Scratch, pane: &str, needle: &str, want: usize) -> usize {
    let mut seen = 0;
    tma_test_support::wait_until(POLL_CEILING, || {
        seen = capture(s, pane).matches(needle).count();
        seen >= want
    });
    seen
}

/// A-250. Steering the text `Enter` delivers five characters and presses no Return of its own: the
/// word appears on screen, and it appears twice because the manifest's `Enter` suffix submitted the
/// line to `cat`. `[MUT]`: drop the `-l` from `Tmux::send_text` and this fails, because tmux reads `Enter`
/// as the named key, the pane sees two bare newlines, and the capture holds the word nowhere.
#[test]
fn steering_the_word_enter_types_it_and_presses_no_key() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_literal");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    let out = act(&s, &["steer", "--pane", &pane, "--text", "Enter"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "steer fires on an idle pane: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = wait_for_count(&s, &pane, "Enter", 2);
    assert!(
        seen >= 2,
        "the literal `Enter` should be typed and then submitted; capture:\n{}",
        capture(&s, &pane)
    );
    assert!(
        s.pane_option(&pane, "@agent_action").is_empty(),
        "the single-flight lock should be cleared"
    );
}

/// A-250, the other half: `C-c` is three characters, not an interrupt. A named-key send would
/// deliver SIGINT and `cat` would be gone.
#[test]
fn steering_c_dash_c_types_it_and_does_not_interrupt() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_ctrlc");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    assert_eq!(
        act(&s, &["steer", "--pane", &pane, "--text", "C-c"])
            .status
            .code(),
        Some(0)
    );
    let seen = wait_for_count(&s, &pane, "C-c", 2);
    assert!(seen >= 2, "capture:\n{}", capture(&s, &pane));
    assert!(pane_alive(&s, &pane), "the running process was interrupted");
}

/// A-282(b). The `--` terminator makes a payload beginning with `-` data rather than a flag tmux's
/// own getopt would read (and reject).
#[test]
fn a_payload_beginning_with_a_dash_reaches_the_pane_intact() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_dash");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    let out = act(&s, &["steer", "--pane", &pane, "--text", "-l -foo"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a leading dash is data: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = wait_for_count(&s, &pane, "-l -foo", 2);
    assert!(seen >= 2, "capture:\n{}", capture(&s, &pane));
}

/// A-281. The sigil refusal is the host's, it carries its own reason token, and it delivers
/// nothing: the agent's command plane is not reachable through a message.
#[test]
fn a_sigil_payload_is_refused_and_nothing_reaches_the_pane() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_sigil");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    for payload in ["/clear", "  /compact", "!ls"] {
        let out = act(&s, &["steer", "--pane", &pane, "--text", payload, "--json"]);
        assert_eq!(out.status.code(), Some(4), "{payload:?} should refuse");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(r#""reason":"sigil""#),
            "{payload:?} reason: {stdout}"
        );
    }
    // A control byte and an empty string are the other two host-side refusals.
    for (payload, reason) in [("one\ttwo", "control-bytes"), ("   ", "empty")] {
        let out = act(&s, &["steer", "--pane", &pane, "--text", payload, "--json"]);
        assert_eq!(out.status.code(), Some(4), "{payload:?} should refuse");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!(r#""reason":"{reason}""#)),
            "{payload:?} reason: {stdout}"
        );
    }
    let screen = capture(&s, &pane);
    assert!(
        !screen.contains("clear") && !screen.contains("compact") && !screen.contains("ls"),
        "a refused payload must not reach the pane; capture:\n{screen}"
    );
}

/// A-252. A text action at a blocked pane is refused by the ordinary gate, `awaiting-text`
/// included, which is the detail v1 deliberately offers no affordance at.
#[test]
fn a_blocked_pane_refuses_a_steer_whatever_its_detail() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_blocked");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);
    s.set_opt(&pane, "@agent_state", "blocked");

    for detail in ["permission", "awaiting-text"] {
        s.set_opt(&pane, "@agent_detail", detail);
        s.set_opt(
            &pane,
            "@agent_stamped_at",
            &tma_runtime::now_ms().to_string(),
        );
        let out = act(
            &s,
            &["steer", "--pane", &pane, "--text", "try main", "--json"],
        );
        assert_eq!(out.status.code(), Some(4), "blocked/{detail} should refuse");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(r#""reason":"gated""#),
            "blocked/{detail}: {stdout}"
        );
    }
    assert!(
        !capture(&s, &pane).contains("try main"),
        "a gated steer must deliver nothing"
    );
}

/// `steer` is idle-only and `steer_now` is the working-pane action; the split is the manifest's,
/// and both directions refuse `gated`.
#[test]
fn steer_and_steer_now_swap_at_working() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_now");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    assert_eq!(
        act(&s, &["steer_now", "--pane", &pane, "--text", "and rebase"])
            .status
            .code(),
        Some(4),
        "steer_now needs a working pane"
    );

    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(
        &pane,
        "@agent_stamped_at",
        &tma_runtime::now_ms().to_string(),
    );
    assert_eq!(
        act(&s, &["steer", "--pane", &pane, "--text", "and rebase"])
            .status
            .code(),
        Some(4),
        "steer needs an idle pane"
    );
    let out = act(&s, &["steer_now", "--pane", &pane, "--text", "and rebase"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "steer_now fires at a working claude: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = wait_for_count(&s, &pane, "and rebase", 2);
    assert!(seen >= 2, "capture:\n{}", capture(&s, &pane));
}

/// The flags fit the kind or the invocation is a usage error, before any pane is touched: a text
/// action needs its string, and no other kind takes one.
#[test]
fn text_flag_usage_is_enforced_per_kind() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_usage");
    let pane = new_cat_pane(&s);
    stamp_idle_claude(&s, &pane);

    let missing = act(&s, &["steer", "--pane", &pane]);
    assert_eq!(missing.status.code(), Some(2), "a text action needs --text");
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--text"));

    let on_keys = act(&s, &["approve", "--pane", &pane, "--text", "hello"]);
    assert_eq!(on_keys.status.code(), Some(2), "a keys action takes none");

    let with_arg = act(
        &s,
        &["steer", "--pane", &pane, "--text", "hi", "--arg", "x"],
    );
    assert_eq!(
        with_arg.status.code(),
        Some(2),
        "a text action takes no --arg"
    );

    assert!(
        !capture(&s, &pane).contains("hello"),
        "a usage error must reach no pane"
    );
}

/// `--list --json` carries the new kind, and its applicability is the `[text]` table.
#[test]
fn the_list_document_reports_the_text_kind_and_its_agents() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("steer_list");
    let out = act(&s, &["--list", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""name":"steer","label":"Steer","kind":"text""#),
        "the list document: {stdout}"
    );
    assert!(
        stdout.contains(r#""name":"steer_now","label":"Steer now","kind":"text""#),
        "the list document: {stdout}"
    );
}
