//! Acceptance: `tma debug capture` / `explain` against a scratch tmux server.
//!
//! Uses an isolated `tmux -L tma_test_<unique> -f /dev/null` socket killed in a drop guard. The
//! "agent" is a real `sleep` whose name is discovered at runtime and written into a test manifest,
//! so the process-identity path runs on both macOS and Linux without a hard-coded name.

use std::process::Command;

use common::{unique_id, Scratch};
use tma_test_support as common;

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

#[test]
fn capture_and_explain_on_scratch_server() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("main");

    // A permission-prompt chrome block, padded with leading blank lines so it sits at the
    // bottom of the viewport (as a real agent's prompt box does), then a long-lived agent.
    let chrome = "\\n\\n\\n\\n\\n\\n\\n\\n\\n\\n\\n\\n\\n\\n\
        ╭──────────────────────────╮\\n\
        │ Do you want to proceed?   │\\n\
        │ ❯ 1. Yes                  │\\n\
        │   2. No                   │\\n\
        ╰──────────────────────────╯\\n";
    let cmd = format!("printf '{chrome}'; exec sleep 100000");
    let out = s.tmux(&["new-session", "-d", "-x", "100", "-y", "24", &cmd]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_capture_contains(
            &s.socket,
            "",
            "Do you want to proceed?",
            common::POLL_CEILING
        ),
        "agent pane's chrome did not render"
    );

    // Discover the pane and the actual process names, then author a matching manifest.
    let pane = s.display("", "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    let current_command = basename(&s.display(&pane, "#{pane_current_command}"));
    let pane_pid = s.display(&pane, "#{pane_pid}");
    let ps = Command::new("ps")
        .args(["-o", "comm=", "-p", &pane_pid])
        .output()
        .expect("ps");
    let ps_comm = basename(&String::from_utf8_lossy(&ps.stdout));

    let mut names = vec![current_command.clone(), ps_comm.clone()];
    names.sort();
    names.dedup();
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");

    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names_toml}]\n\
             [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
             [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"Do you want to proceed?\" }}\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"❯\" }}\n",
        ),
    )
    .unwrap();

    // --- capture: fixture format ------------------------------------------------------
    let cap = s.tma(&["debug", "capture", &pane]);
    assert!(
        cap.status.success(),
        "capture failed: {}",
        String::from_utf8_lossy(&cap.stderr)
    );
    let cap_text = String::from_utf8_lossy(&cap.stdout);
    assert!(
        cap_text.starts_with("# agent: "),
        "capture is fixture format:\n{cap_text}"
    );
    assert!(cap_text.contains("\n---\n"), "capture has separator");
    assert!(
        cap_text.contains("Do you want to proceed?"),
        "capture carries the chrome"
    );

    // --- explain: text form -----------------------------------------------------------
    let exp = s.tma(&["debug", "explain", &pane]);
    assert!(
        exp.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&exp.stderr)
    );
    let exp_text = String::from_utf8_lossy(&exp.stdout);
    assert!(
        exp_text.contains("agent     agent"),
        "identified the agent:\n{exp_text}"
    );
    assert!(
        exp_text.contains("verdict   blocked"),
        "detected blocked:\n{exp_text}"
    );
    assert!(
        exp_text.contains("[match]"),
        "shows a matched rule:\n{exp_text}"
    );

    // --- explain --json: schema + shape ------------------------------------------
    let js = s.tma(&["debug", "explain", &pane, "--json"]);
    assert!(
        js.status.success(),
        "explain --json failed: {}",
        String::from_utf8_lossy(&js.stderr)
    );
    let json = String::from_utf8_lossy(&js.stdout);
    assert!(
        json.contains("\"schema\":1"),
        "json carries schema 1:\n{json}"
    );
    assert!(
        json.contains("\"state\":\"blocked\""),
        "json verdict blocked:\n{json}"
    );
    assert!(
        json.contains("\"agent\":\"agent\""),
        "json agent name:\n{json}"
    );
}

#[test]
fn server_gone_is_a_clean_error() {
    if !tma_test_support::tmux_available() {
        return;
    }
    // A socket with no server: every tmux read must degrade to a clean message, not panic.
    let out = Command::new(common::tma_bin())
        .args([
            "debug",
            "explain",
            "%0",
            "--socket-name",
            "tma_test_definitely_absent_9999",
        ])
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no tmux server"),
        "clean server-gone message: {stderr}"
    );
}

/// The config-isolation guarantee bites: a server started with `-f /dev/null` does NOT see an
/// option a user's `~/.tmux.conf` would set, whereas a server pointed at that same config
/// does. Hermetic — a throwaway config in temp is used; the real `~/.tmux.conf` is untouched.
#[test]
fn scratch_server_ignores_user_config() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let id = unique_id();
    let sock_leak = format!("tma_test_leak_{id}");
    let sock_iso = format!("tma_test_iso_{id}");
    let conf = std::env::temp_dir().join(format!("tma_hostile_{id}.conf"));
    // A config a hostile/normal user might have; it sets a server option marker.
    std::fs::write(&conf, "set -s @tma_hostile_marker leaked\n").unwrap();
    let conf_str = conf.display().to_string();

    let tmux = |sock: &str, cfg: &str, args: &[&str]| -> String {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(sock)
            .arg("-f")
            .arg(cfg)
            .args(args)
            .output()
            .expect("spawn tmux");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Control: a server that sources the config observes the marker.
    tmux(&sock_leak, &conf_str, &["new-session", "-d", "sleep", "60"]);
    let leaked = tmux(
        &sock_leak,
        &conf_str,
        &["show-options", "-sqv", "@tma_hostile_marker"],
    );
    // Isolated: a `-f /dev/null` server never sources it.
    tmux(
        &sock_iso,
        "/dev/null",
        &["new-session", "-d", "sleep", "60"],
    );
    let isolated = tmux(
        &sock_iso,
        "/dev/null",
        &["show-options", "-sqv", "@tma_hostile_marker"],
    );

    // Clean up both servers, their socket files, and the throwaway config before asserting.
    let _ = tmux(&sock_leak, "/dev/null", &["kill-server"]);
    let _ = tmux(&sock_iso, "/dev/null", &["kill-server"]);
    common::cleanup_scratch_socket(&sock_leak);
    common::cleanup_scratch_socket(&sock_iso);
    let _ = std::fs::remove_file(&conf);

    assert_eq!(
        leaked, "leaked",
        "control: a server sourcing the config must observe the marker"
    );
    assert_eq!(
        isolated, "",
        "a -f /dev/null scratch server must NOT observe the user config (isolation bites)"
    );
}
