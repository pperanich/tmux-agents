//! Acceptance: the `@agent_ignore` escape hatch on a scratch tmux server. A pane the walk
//! recognizes as an agent stops being one the moment the option is set, its stamp is cleared, and
//! `tma doctor` says why.

use std::process::Command;

use common::Scratch;
use tma_test_support as common;

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// A manifest matching the pane's real process names, discovered at runtime so the identity walk
/// works on both macOS and Linux without a hard-coded binary name.
fn write_manifest(s: &Scratch, pane: &str) {
    let current_command = basename(&s.display(pane, "#{pane_current_command}"));
    let pane_pid = s.display(pane, "#{pane_pid}");
    let ps = Command::new("ps")
        .args(["-o", "comm=", "-p", &pane_pid])
        .output()
        .expect("ps");
    let mut names = vec![
        current_command,
        basename(&String::from_utf8_lossy(&ps.stdout)),
    ];
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
             [capture]\nvisible = [\"working\", \"idle\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n",
        ),
    )
    .unwrap();
}

fn ls(s: &Scratch) -> String {
    let out = s.tma(&["ls"]);
    assert!(
        out.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn an_ignored_pane_leaves_every_surface_and_loses_its_stamp() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("ignore");
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "the pane never rendered"
    );
    let pane = s.display("", "#{pane_id}");
    write_manifest(&s, &pane);

    // Baseline: the pane detects as an agent and carries a stamp.
    assert!(
        ls(&s).contains(&pane),
        "the pane should be detected before it is ignored"
    );
    assert!(
        !s.display(&pane, "#{@agent_state}").is_empty(),
        "the cycle should have stamped the pane"
    );

    // The escape hatch: any non-empty value takes the pane out of detection.
    s.tmux(&["set-option", "-p", "-t", &pane, "@agent_ignore", "1"]);
    assert_eq!(ls(&s), "", "an ignored pane must not be listed");
    assert_eq!(
        s.display(&pane, "#{@agent_state}"),
        "",
        "the stale stamp must be cleared, not left to age"
    );

    // Discoverability: doctor names the pane and the option that silenced it.
    let doc = s.tma(&["doctor"]);
    let text = String::from_utf8_lossy(&doc.stdout);
    assert!(
        text.contains("ignored via @agent_ignore"),
        "doctor lists the ignored pane:\n{text}"
    );
    assert!(text.contains(&pane), "doctor names the pane:\n{text}");

    // `explain` agrees rather than folding a verdict nothing else honors.
    let exp = s.tma(&["debug", "explain", &pane]);
    let exp_text = String::from_utf8_lossy(&exp.stdout);
    assert!(
        exp_text.contains("@agent_ignore is set on this pane"),
        "explain names the opt-out:\n{exp_text}"
    );

    // Unsetting brings the pane back: the option is the only thing that was holding it out.
    s.tmux(&["set-option", "-p", "-t", &pane, "-u", "@agent_ignore"]);
    assert!(
        ls(&s).contains(&pane),
        "clearing @agent_ignore restores detection"
    );
}
