//! Acceptance: the `config.toml` reaches real behavior, and zero-config is unchanged.
//!
//! Every case drives an isolated scratch `tmux -L …` server (`-f /dev/null`, killed on drop) with an
//! explicit `--config <path>`, so the real `~/.config/tma/config.toml` is never read. An absent path
//! is the zero-config floor.

use std::path::Path;
use std::process::{Command, Output};

use common::Scratch;
use tma_test_support as common;

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Run `tma` against the scratch server + the suite's `agents/` dir, with an explicit `--config`
/// path. The shared `Scratch::tma` pins an empty config; this suite drives a real one, so it builds
/// its own command over the shared harness core.
fn tma(s: &Scratch, config: &Path, args: &[&str]) -> Output {
    Command::new(common::tma_bin())
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .arg("--config")
        .arg(config)
        // Pinned into the scratch: the developer's own `~/.config/tma` must never be read, still
        // less written, by a test run.
        .env("TMA_CONFIG_DIR", s.workdir.join("cfg"))
        .output()
        .expect("spawn tma")
}

/// Launch a fake agent pane printing `chrome` then a long-lived process, and discover the pane's
/// real process names (so a manifest's `[identity]` can match, or a case can author a non-matching
/// one).
fn setup_pane(s: &Scratch, chrome: &str) -> (String, Vec<String>) {
    let cmd = format!("printf '{chrome}'; exec sleep 100000");
    let out = s.tmux(&["new-session", "-d", "-x", "100", "-y", "24", &cmd]);
    assert!(out.status.success(), "new-session failed");
    // The `READY` marker gates readiness: its render proves the `printf` ran and the shell reached
    // its `exec`, so the ps-walk below sees the final process.
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "agent pane's chrome did not render"
    );
    let pane = s.display("", "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    let current = basename(&s.display(&pane, "#{pane_current_command}"));
    let pane_pid = s.display(&pane, "#{pane_pid}");
    let ps_comm = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pane_pid])
            .output()
            .expect("ps")
            .stdout,
    ));
    let mut names = vec![current, ps_comm];
    names.sort();
    names.dedup();
    (pane, names)
}

fn idle_manifest(names: &[String]) -> String {
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names_toml}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"idle\"\npriority = 50\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
    )
}

/// Zero-config: an absent config file yields the documented default glyphs/colors, so
/// `tma status` is byte-identical to the pre-config behavior.
#[test]
fn zero_config_status_matches_documented_defaults() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("zero");
    let (_pane, names) = setup_pane(&s, "READY\\n");
    s.write_manifest("agent.toml", &idle_manifest(&names));

    // `--config` pointing at a file that does not exist ⇒ zero-config floor (defaults).
    let absent = s.workdir.join("does-not-exist.toml");
    let out = tma(&s, &absent, &["status"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "#[range=user|tma:idle]#[fg=green]○1#[norange]",
        "zero-config renders the default green ○ for an idle agent"
    );
}

/// A `[status]` glyph + color override reaches `tma status` output.
#[test]
fn status_glyph_and_color_override_is_honored() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("statusov");
    let (_pane, names) = setup_pane(&s, "READY\\n");
    s.write_manifest("agent.toml", &idle_manifest(&names));
    s.write_config("[status]\nidle = { glyph = \"I\", color = \"colour40\" }\n");

    let out = tma(&s, &s.config_path(), &["status"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "#[range=user|tma:idle]#[fg=colour40]I1#[norange]",
        "the config glyph + color override is rendered"
    );
}

/// A `[fold] freshness_secs = 0` override reaches the consumer/producer boundary: the second
/// cycle re-captures because no stamp is ever "fresh" (the boundary is the injected FoldConfig).
#[test]
fn fold_freshness_override_changes_recapture_boundary() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("fresh");
    let (_pane, names) = setup_pane(&s, "READY\\n");
    s.write_manifest("agent.toml", &idle_manifest(&names));
    s.write_config("[fold]\nfreshness_secs = 0\n");

    let captures_in = |stderr: &str| -> Option<u32> {
        stderr
            .split(',')
            .find_map(|seg| seg.trim().strip_suffix(" captures"))
            .and_then(|n| n.trim().parse().ok())
    };

    let first = tma(&s, &s.config_path(), &["ls", "--debug-timing"]);
    assert!(first.status.success());
    assert_eq!(
        captures_in(&String::from_utf8_lossy(&first.stderr)),
        Some(1),
        "first cycle captures"
    );
    // Wait past the same-second stampede guard so the second cycle's produce/consume choice is the
    // freshness window alone: with freshness_secs = 0 the ~1 s-old stamp is never fresh, so it
    // re-captures (unlike the default 3 s). The injected FoldConfig reaching the pure boundary.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let second = tma(&s, &s.config_path(), &["ls", "--debug-timing"]);
    assert!(second.status.success());
    assert_eq!(
        captures_in(&String::from_utf8_lossy(&second.stderr)),
        Some(1),
        "freshness_secs=0 forces a re-capture on the second cycle"
    );
}

/// A malformed config fails loudly, names the file, and exits non-zero (never silent defaults).
#[test]
fn malformed_config_names_file_and_exits_nonzero() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("bad");
    // A type error (freshness_secs wants an integer) — toml names the key + span.
    s.write_config("[fold]\nfreshness_secs = \"three\"\n");

    let out = tma(&s, &s.config_path(), &["status"]);
    assert!(
        !out.status.success(),
        "a malformed config must exit non-zero, not silently default"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("config.toml"),
        "the error names the config file: {stderr:?}"
    );
    assert!(
        stderr.contains("freshness_secs"),
        "the error names the offending key: {stderr:?}"
    );
}

/// An unknown key is rejected too (deny_unknown_fields — never a silently-ignored typo).
#[test]
fn unknown_config_key_is_rejected() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("unknown");
    s.write_config("[notify]\nfrom_evemt = true\n"); // typo: from_evemt

    let out = tma(&s, &s.config_path(), &["status"]);
    assert!(!out.status.success(), "an unknown key must be a loud error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("from_evemt"),
        "the error names the unknown key: {stderr:?}"
    );
}

/// `[[agent]] enabled = false` drops that manifest from the loaded set, so its pane resolves as
/// no agent.
#[test]
fn agent_disable_drops_the_manifest() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("disable");
    let (pane, names) = setup_pane(&s, "READY\\n");
    s.write_manifest("agent.toml", &idle_manifest(&names));
    // (The same `idle_manifest` pane is detected under zero-config in
    // `zero_config_status_matches_documented_defaults`; here we prove the disable drops it.)

    // Disabling the `agent` manifest (its stem) drops it from the loaded set, so the pane is
    // never identified as an agent ⇒ no rows. No prior stamp exists to be consumed.
    s.write_config("[[agent]]\nname = \"agent\"\nenabled = false\n");
    let out = tma(&s, &s.config_path(), &["ls", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        !json.contains(&format!("\"pane\":\"{pane}\"")),
        "a disabled agent is dropped: {json}"
    );
    assert!(
        json.contains("\"agents\":[]"),
        "no agent rows with the manifest disabled: {json}"
    );
}

/// A custom `[[agent]] process_names` extends a manifest's identity match so a pane the shipped
/// names would miss is detected as that agent.
#[test]
fn custom_process_name_detects_a_formerly_unmatched_pane() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("procname");
    let (pane, real_names) = setup_pane(&s, "READY\\n");
    // A manifest whose process_names match NOTHING the pane runs.
    s.write_manifest(
        "agent.toml",
        &idle_manifest(&["no-such-agent-xyz".to_string()]),
    );

    // Without the config extension: not detected.
    let absent = s.workdir.join("none.toml");
    let before = String::from_utf8_lossy(&tma(&s, &absent, &["ls", "--json"]).stdout).to_string();
    assert!(
        !before.contains(&format!("\"pane\":\"{pane}\"")),
        "unmatched process name ⇒ no agent before config: {before}"
    );

    // Extend the `agent` manifest with the pane's REAL process names via config.
    let extra = real_names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    s.write_config(&format!(
        "[[agent]]\nname = \"agent\"\nprocess_names = [{extra}]\n"
    ));
    let out = tma(&s, &s.config_path(), &["ls", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")),
        "the config process-name map makes the pane resolve as the agent: {json}"
    );
}

/// Drive one daemonless blocked direct-fire: a hook-capable manifest mapping a synthetic `Block`
/// event, the caller's `[notify]` config, and one `tma event` fired exactly as the wrapper would with
/// NO `TMA_NOTIFY_*` env at all, so only the config can fire it (no daemon runs). Returns the firing
/// pane, having asserted the event direct-stamped `blocked`.
/// A distinctive pane title the notify tests can search every carrier for. Shaped like the thing a
/// real title leaks — a branch name with a ticket id — so a hit in a payload, a log line or an env
/// var is unambiguous.
const SECRET_TITLE: &str = "feat/ACME-1234-rotate-customer-keys";

fn daemonless_blocked_fire(s: &Scratch, notify_toml: &str) -> String {
    let (pane, names) = setup_pane(s, "READY\\n");
    // Every notify fire in this file runs against a pane with a title worth redacting, so the
    // privacy assertions below have something real to look for.
    s.tmux(&["select-pane", "-t", &pane, "-T", SECRET_TITLE]);
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    s.write_manifest("agent.toml", &format!(
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names_toml}]\n\
         [hooks]\ncovers = [\"blocked\"]\n\
         [[hooks.map]]\nevent = \"Block\"\nclaim = {{ state = \"blocked\", detail = \"permission\" }}\n\
         [capture]\nvisible = [\"blocked\"]\n"
    ));
    s.write_config(notify_toml);

    use std::io::Write;
    let mut child = Command::new(common::tma_bin())
        .args([
            "event",
            "--agent",
            "agent",
            "--kind",
            "Block",
            "--payload",
            "-",
        ])
        .args(["--socket-name", &s.socket])
        .args(["--manifest-dir", s.manifest_dir().to_str().unwrap()])
        .args(["--config", s.config_path().to_str().unwrap()])
        .env("TMUX_PANE", &pane)
        // Keep the notify failure marker inside the scratch: it lives beside the daemon sockets in
        // the runtime dir, and a test must never touch the developer's real one.
        .env("XDG_RUNTIME_DIR", &s.workdir)
        .env_remove("TMA_NOTIFY_FROM_EVENT")
        .env_remove("TMA_NOTIFY_CMD")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"sess-1"}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        s.display(&pane, "#{@agent_state}"),
        "blocked",
        "the hook event direct-stamped blocked"
    );
    pane
}

/// `notify.command` + `notify.from_event` in CONFIG (no `TMA_NOTIFY_*` env) fire the hook command
/// on a daemonless blocked transition — proving config is the canonical notify mechanism.
#[test]
fn notify_command_from_config_fires_daemonless() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("notify");
    // TOML literal string (single quotes) so `\n` stays literal for printf and the inner double
    // quotes are fine.
    let sink = s.workdir.join("sink");
    let pane = daemonless_blocked_fire(
        &s,
        &format!(
            "[notify]\nfrom_event = true\ncommand = 'printf \"fire %s\\n\" \"$TMA_PANE\" >> {}'\n",
            sink.display()
        ),
    );
    let lines = std::fs::read_to_string(&sink).unwrap_or_default();
    assert_eq!(
        lines.trim(),
        format!("fire {pane}"),
        "the config notify.command fired exactly once with the pane env var: {lines:?}"
    );
}

/// `[notify.blocked]` routes the blocked fire to its own command, and the global `notify.command`
/// stays untouched for the triggers with no override (here: never invoked at all).
#[test]
fn notify_sub_table_routes_the_blocked_fire() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("route");
    let routed = s.workdir.join("routed");
    let global = s.workdir.join("global");
    let pane = daemonless_blocked_fire(
        &s,
        &format!(
            "[notify]\nfrom_event = true\ncommand = 'printf global >> {}'\n\
             [notify.blocked]\ncommand = 'printf \"routed %s\" \"$TMA_PANE\" >> {}'\n",
            global.display(),
            routed.display()
        ),
    );
    assert_eq!(
        std::fs::read_to_string(&routed).unwrap_or_default(),
        format!("routed {pane}"),
        "the blocked fire ran the sub-table's command"
    );
    assert!(
        !global.exists(),
        "the global command must not also fire for a routed trigger"
    );
}

/// `[notify] log`: a daemonless fire appends one JSON line per notification, with the fire time and
/// the payload's keys, creating the parent directory on the way.
#[test]
fn notify_log_appends_one_line_per_fire() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("notifylog");
    // A nested path that does not exist yet: the sink creates it.
    let log = s.workdir.join("state/fires.jsonl");
    let pane = daemonless_blocked_fire(
        &s,
        &format!("[notify]\nfrom_event = true\nlog = '{}'\n", log.display()),
    );
    let body = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one fire was logged: {body:?}");
    assert!(
        lines[0].contains(&format!("\"pane\":\"{pane}\""))
            && lines[0].contains("\"state\":\"blocked\"")
            && lines[0].contains("\"at\":"),
        "the line carries the payload plus the fire time: {}",
        lines[0]
    );
    // No `command` is configured: the log is a standalone sink, not a side effect of a hook.
    assert!(lines[0].starts_with(r#"{"schema":2,"at":"#));

    // A-512 / A-514, end to end through a real fire: the audit line must not carry the pane title.
    // The log is the file most likely to be pasted into an issue, and it shares one writer with the
    // payload that goes to a third-party carrier — so if the title is here, it went out too.
    assert!(
        !lines[0].contains(SECRET_TITLE) && !lines[0].contains(r#""title""#),
        "the pane title reached the audit line: {}",
        lines[0]
    );
    // A-515: the absolute episode stamp rides, and the age survives beside it.
    assert!(
        lines[0].contains(r#""episode_ms":"#) && lines[0].contains(r#""since_ms":"#),
        "both episode fields present: {}",
        lines[0]
    );

    // A-516: the log is created 0600. Without an explicit mode it lands at `0666 & ~umask` — 0664
    // under the common `umask 002`, and world-WRITABLE under `umask 000`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "notify log mode is 0o{mode:o}, want 0o600");
    }
}

/// A-512 / A-514 for the two carriers a hook actually reads: the JSON on stdin and `TMA_TITLE`.
/// Redacting the payload but not the env var would leave the title in the channel a shell one-liner
/// interpolates, which is the one most `[notify] command` recipes use.
#[test]
fn the_pane_title_reaches_neither_the_payload_nor_tma_title() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("notifyredact");
    let seen = s.workdir.join("seen.txt");
    daemonless_blocked_fire(
        &s,
        &format!(
            "[notify]\nfrom_event = true\n\
             command = 'cat >> {0}; printf \"\\nTMA_TITLE=[%s]\\n\" \"${{TMA_TITLE:-<unset>}}\" >> {0}'\n",
            seen.display()
        ),
    );
    let body = std::fs::read_to_string(&seen).unwrap_or_default();
    assert!(!body.is_empty(), "the hook command did not run");
    assert!(
        !body.contains(SECRET_TITLE),
        "the pane title reached the hook: {body}"
    );
    assert!(
        body.contains("TMA_TITLE=[<unset>]"),
        "TMA_TITLE must be unset, not empty: {body}"
    );
    // The rest of the payload still arrives, so a hook has something to format.
    assert!(body.contains(r#""state":"blocked""#), "{body}");
    assert!(body.contains(r#""episode_ms":"#), "{body}");
}

/// The back-compat lever, and the only thing that opens it: `[notify] include_title = true` puts the
/// title back in all three carriers at once.
#[test]
fn include_title_restores_the_title_in_every_carrier() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("notifytitleon");
    let seen = s.workdir.join("seen.txt");
    let log = s.workdir.join("fires.jsonl");
    daemonless_blocked_fire(
        &s,
        &format!(
            "[notify]\nfrom_event = true\ninclude_title = true\nlog = '{1}'\n\
             command = 'cat >> {0}; printf \"\\nTMA_TITLE=[%s]\\n\" \"${{TMA_TITLE:-<unset>}}\" >> {0}'\n",
            seen.display(),
            log.display()
        ),
    );
    let body = std::fs::read_to_string(&seen).unwrap_or_default();
    assert!(
        body.contains(&format!(r#""title":"{SECRET_TITLE}""#)),
        "payload title: {body}"
    );
    assert!(
        body.contains(&format!("TMA_TITLE=[{SECRET_TITLE}]")),
        "env title: {body}"
    );
    let line = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(line.contains(SECRET_TITLE), "log title: {line}");
}

/// A notify command that exits non-zero is otherwise silent (its output is discarded), so the fire
/// records it and `tma doctor` reports it.
#[test]
fn a_failing_notify_command_is_recorded_and_reported() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("notifyfail");
    daemonless_blocked_fire(&s, "[notify]\nfrom_event = true\ncommand = 'exit 9'\n");

    let marker = std::fs::read_to_string(s.workdir.join("tma/notify-error")).unwrap_or_default();
    assert!(
        marker.contains("reason=exited 9") && marker.contains("command=exit 9"),
        "the fire recorded the failing command: {marker:?}"
    );

    let out = Command::new(common::tma_bin())
        .args(["doctor", "--json"])
        .args(["--socket-name", &s.socket])
        .args(["--manifest-dir", s.manifest_dir().to_str().unwrap()])
        .args(["--config", s.config_path().to_str().unwrap()])
        .env("XDG_RUNTIME_DIR", &s.workdir)
        .output()
        .expect("spawn tma doctor");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(r#""reason":"exited 9""#),
        "doctor surfaces the recorded failure: {json}"
    );
}

/// The notify hook's stdin payload on the daemonless path carries the documented enriched key set:
/// the repo/branch labels resolved from the pane's cwd, the episode age, and the context gauge. The
/// pane runs in a real checkout (this crate's own directory), so the labels are non-empty.
#[test]
fn notify_payload_carries_repo_branch_and_episode_age() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("payload");
    let dump = s.workdir.join("payload.json");
    let pane = daemonless_blocked_fire(
        &s,
        &format!(
            "[notify]\nfrom_event = true\ncommand = 'cat >> {}'\n",
            dump.display()
        ),
    );
    let payload = std::fs::read_to_string(&dump).unwrap_or_default();
    assert!(
        payload.contains(&format!("\"pane\":\"{pane}\""))
            && payload.contains("\"state\":\"blocked\""),
        "the fire delivered its payload on stdin: {payload:?}"
    );
    for key in [
        "\"repo\":",
        "\"branch\":",
        "\"since_ms\":",
        "\"context_pct\":null",
    ] {
        assert!(
            payload.contains(key),
            "payload is missing {key}: {payload:?}"
        );
    }
    // The pane inherits the test process's cwd (this crate, a checkout), so the labels resolve.
    let cwd = std::env::current_dir().expect("cwd");
    if let Some(want) = tma_runtime::repo::resolve(cwd.to_str().unwrap()) {
        assert!(
            payload.contains(&format!("\"repo\":\"{}\"", want.repo_name)),
            "the payload carries the pane's repo label: {payload:?}"
        );
    }
}

/// `notify.bell`: a daemonless blocked direct-fire also rings the firing pane's bell, observable as
/// `#{window_bell_flag}` flipping to 1 (a BEL written to the pane tty). The `bell = false` control
/// proves the flip comes from the bell config, not the `printf` chrome or the event.
#[test]
fn notify_bell_rings_the_firing_pane() {
    if !common::tmux_available() {
        return;
    }

    // Drive the same daemonless blocked fire with the bell on vs off; return the firing pane's
    // `#{window_bell_flag}` after the `tma event` completes. The pane's tty is the fire target.
    let ring = |bell: bool| -> String {
        let s = Scratch::new(if bell { "bell_on" } else { "bell_off" });
        // No `command` — the bell is a standalone companion, independent of the hook command.
        let pane =
            daemonless_blocked_fire(&s, &format!("[notify]\nfrom_event = true\nbell = {bell}\n"));

        // tmux reads the BEL off the pty asynchronously, so poll briefly for the flag to settle
        // rather than racing the server's event loop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let flag = s.display(&pane, "#{window_bell_flag}");
            if flag == "1" || std::time::Instant::now() >= deadline {
                break flag;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    assert_eq!(
        ring(true),
        "1",
        "notify.bell = true rings the firing pane (window_bell_flag set)"
    );
    assert_eq!(
        ring(false),
        "0",
        "control: with bell off the pane never rings (nothing writes a BEL)"
    );
}

/// Harness isolation: pinning `TMA_CONFIG` to the shared empty file shields `tma status` from a
/// hostile config at the `$XDG_CONFIG_HOME/tma/config.toml` default location, because `TMA_CONFIG`
/// outranks the XDG/`HOME` lookup. The no-pin control reads the hostile config, proving the pin is
/// what isolates. Hermetic: the hostile config lives in a temp `XDG_CONFIG_HOME`.
#[test]
fn pinned_empty_config_shields_status_from_hostile_default_config() {
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("shield");
    let (_pane, names) = setup_pane(&s, "READY\\n");
    s.write_manifest("agent.toml", &idle_manifest(&names));

    // A hostile config at the XDG default location: a non-default idle glyph + color that a leak
    // would render as `#[fg=red]X1` instead of the documented `#[fg=green]○1`.
    let xdg = s.workdir.join("xdg_hostile");
    std::fs::create_dir_all(xdg.join("tma")).unwrap();
    std::fs::write(
        xdg.join("tma/config.toml"),
        "[status]\nidle = { glyph = \"X\", color = \"red\" }\n",
    )
    .unwrap();

    // `tma status` with the hostile config on `$XDG_CONFIG_HOME`, optionally pinned to the empty
    // config the way the shared harness pins it.
    let status = |pinned: bool| -> String {
        let mut cmd = Command::new(common::tma_bin());
        cmd.args(["status", "--socket-name", &s.socket])
            .args(["--manifest-dir", s.manifest_dir().to_str().unwrap()])
            .env("XDG_CONFIG_HOME", &xdg);
        if pinned {
            cmd.env("TMA_CONFIG", common::empty_config_path());
        } else {
            cmd.env_remove("TMA_CONFIG");
        }
        let out = cmd.output().expect("spawn tma status");
        assert!(
            out.status.success(),
            "status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    assert_eq!(
        status(true),
        "#[range=user|tma:idle]#[fg=green]○1#[norange]",
        "the empty-config pin shields status from the hostile default config"
    );
    assert_eq!(
        status(false),
        "#[range=user|tma:idle]#[fg=red]X1#[norange]",
        "without the pin the hostile default config is read (so the pin is what isolates)"
    );
}
