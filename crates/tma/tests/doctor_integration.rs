//! `tma doctor` acceptance on a scratch tmux server, no daemon running.
//!
//! Scratch `tmux -L tma_test_<unique>` (`-f /dev/null`), killed on drop — never the default
//! server, never the user's real config (settings/wrapper/config-dir are pinned to a temp
//! workdir). Exercises the two things the task names: the no-daemon tier reporting (tier 1 for a
//! hookless agent, tier 2 for a wired agent with no daemon) and the `--json` schema shape.

use std::path::PathBuf;
use std::process::{Command, Output};

use common::Scratch;
use tma_test_support as common;

// The hook-config paths this suite pins so both `install-hooks` and `doctor` resolve the same
// files. Suite-specific free helpers over the shared [`Scratch`] core.
fn settings(s: &Scratch) -> PathBuf {
    s.workdir.join("settings.json")
}
fn config_dir(s: &Scratch) -> PathBuf {
    s.workdir.join("cfg")
}
fn wrapper(s: &Scratch) -> PathBuf {
    s.workdir.join("bin/tma-hook")
}

/// Run `tma <args>` against the scratch server + temp manifest dir, hook-config paths pinned via env
/// so both `install-hooks` and `doctor` resolve the same files.
fn tma(s: &Scratch, args: &[&str]) -> Output {
    tma_cmd(s, args).output().expect("spawn tma")
}

/// The command [`tma`] runs, for the one check that has to alter the child's environment.
fn tma_cmd(s: &Scratch, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tma"));
    cmd.args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_BIN", env!("CARGO_BIN_EXE_tma"))
        .env("TMA_CLAUDE_SETTINGS", settings(s))
        .env("TMA_WRAPPER_PATH", wrapper(s))
        .env("TMA_CONFIG_DIR", config_dir(s));
    cmd
}

/// Run `tma <args>` like [`tma`] but feed `stdin_payload` on stdin (for `tma event --payload -`),
/// with `TMUX_PANE` set the way a hook fires (the state intake resolves its pane from that env).
fn tma_stdin(s: &Scratch, pane: &str, args: &[&str], stdin_payload: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMUX_PANE", pane)
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_BIN", env!("CARGO_BIN_EXE_tma"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn tma");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    let status = child.wait().expect("wait tma");
    Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Launch a fake agent pane and return (pane_id, process-names TOML fragment matching it).
fn spawn_agent(s: &Scratch) -> (String, String) {
    let cmd = "printf 'READY\\n'; exec sleep 100000";
    let out = s.tmux(&["new-session", "-d", "-x", "100", "-y", "24", cmd]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "agent pane's chrome did not render"
    );

    let pane = s.display("", "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    let current_command = basename(&s.display(&pane, "#{pane_current_command}"));
    let pane_pid = s.display(&pane, "#{pane_pid}");
    let ps_comm = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pane_pid])
            .output()
            .expect("ps")
            .stdout,
    ));
    let mut names = vec![current_command, ps_comm];
    names.sort();
    names.dedup();
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    (pane, names_toml)
}

/// A hookless agent on a daemonless server reports tier 1, with the `--json` schema shape, and
/// the ambient-driver check reports NOT polling before any `tma status`/`ls` has run.
#[test]
fn doctor_json_no_daemon_hookless_agent_is_tier_1() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("hookless");
    let (pane, names) = spawn_agent(&s);
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}]\n\
             [capture]\nvisible = [\"idle\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();

    let out = tma(&s, &["doctor", "--json"]);
    assert!(
        out.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);

    assert!(json.contains("\"schema\":1"), "schema 1: {json}");
    assert!(
        json.contains("\"alive\":false"),
        "no daemon on the scratch server: {json}"
    );
    assert!(
        json.contains("\"polling\":false"),
        "nothing has polled yet ⇒ no ambient driver: {json}"
    );
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")),
        "the agent pane is reported: {json}"
    );
    assert!(
        json.contains("\"agent\":\"agent\""),
        "agent name from the manifest stem: {json}"
    );
    assert!(json.contains("\"tier\":1"), "hookless ⇒ tier 1: {json}");
    assert!(
        json.contains("\"hook_status\":\"hookless\""),
        "hookless wiring reported: {json}"
    );
    // Unstamped (no cycle has run) ⇒ null evidence fields, and it still lists the agent.
    assert!(
        json.contains("\"state\":null"),
        "unstamped pane has null state: {json}"
    );

    // Human-readable form names the tier and the reason.
    let text_out = tma(&s, &["doctor"]);
    assert!(text_out.status.success());
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(text.contains("tier 1"), "text names the tier: {text}");
    assert!(
        text.contains("not running"),
        "text reports the daemon is down: {text}"
    );
    assert!(
        text.contains("NOT polling"),
        "text flags the missing ambient driver: {text}"
    );
}

/// A fully hook-wired agent with NO daemon running reports tier 2, with the reason "daemon not
/// running". Exercises the reused `install-hooks --check` machinery via `diagnose_hooks`.
#[test]
fn doctor_reports_tier_2_for_wired_agent_without_daemon() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("wired");
    let (pane, names) = spawn_agent(&s);
    // A `claude`-named manifest so the Claude installer adapter applies (config_target).
    std::fs::write(
        s.workdir.join("claude.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}]\n\
             [hooks]\ncovers = [\"blocked\", \"lifecycle\"]\n\
             [[hooks.map]]\nevent = \"SessionStart\"\nclaim = {{ lifecycle = \"start\" }}\n\
             [[hooks.map]]\nevent = \"SessionEnd\"\nclaim = {{ lifecycle = \"end\" }}\n\
             [[hooks.map]]\nevent = \"Notification\"\nmatcher = \"permission_prompt\"\n\
             claim = {{ state = \"blocked\", detail = \"permission\" }}\n\
             [capture]\nvisible = [\"idle\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();

    // Wire the hooks (settings/wrapper/config-dir come from the pinned env, matching doctor).
    let install = tma(&s, &["install-hooks", "claude", "--yes"]);
    assert!(
        install.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(settings(&s).exists(), "settings written");
    assert!(wrapper(&s).exists(), "wrapper written");

    let out = tma(&s, &["doctor", "--json"]);
    assert!(
        out.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);

    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")),
        "agent pane reported: {json}"
    );
    assert!(
        json.contains("\"agent\":\"claude\""),
        "claude agent name: {json}"
    );
    assert!(
        json.contains("\"hooks_wired\":true") && json.contains("\"hook_status\":\"wired\""),
        "hooks wired via the reused --check machinery: {json}"
    );
    assert!(
        json.contains("\"tier\":2"),
        "wired + no daemon ⇒ tier 2: {json}"
    );
    assert!(
        json.contains("daemon not running"),
        "tier_reason explains the missing rung: {json}"
    );
    assert!(
        json.contains("\"alive\":false"),
        "daemon is not running: {json}"
    );
}

/// A pane running a nested multiplexer client is listed with the "run tma there" hint, so the
/// operator is told why that pane has no row rather than left to guess.
#[test]
fn doctor_names_a_nested_multiplexer_pane() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new("nested");
    // The inner server is owned by the scratch guard: killed on drop even if an assertion below
    // panics, so a failing run leaks no tmux server.
    let inner = s.nested_socket("nested-inner");

    let cmd = format!("exec tmux -L {inner} -f /dev/null new-session -A -s in");
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &cmd])
        .status
        .success());
    let pane = s.display("", "#{pane_id}");
    let ready = common::wait_until(common::POLL_CEILING, || {
        basename(&s.display(&pane, "#{pane_current_command}")) == "tmux"
    });
    if !ready {
        eprintln!("skipping: the pane's foreground never became the nested tmux client");
        return;
    }

    let json = String::from_utf8_lossy(&tma(&s, &["doctor", "--json"]).stdout).to_string();
    assert!(
        json.contains("\"nested_multiplexers\":[{"),
        "the pane is listed as a nested multiplexer: {json}"
    );
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")) && json.contains("\"command\":\"tmux\""),
        "with its pane id and the matched command: {json}"
    );

    let text = String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).to_string();
    assert!(
        text.contains("nested:") && text.contains("run tma there"),
        "the human form carries the hint: {text}"
    );
}

/// A pane behind a remote shell is listed with the socket condition an agent there has to meet,
/// and any stamp it still carries is called held: that pane used to show frozen state, silently.
#[test]
fn doctor_names_a_remote_pane_and_its_held_stamp() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("remote");
    // A process whose comm IS `ssh` (what the identity carve-out matches) but that just sleeps,
    // so the test needs no network and no remote host.
    let fake_ssh = s.workdir.join("ssh");
    std::fs::copy("/bin/sleep", &fake_ssh).expect("a sleeper named ssh");
    let cmd = format!("exec {} 100000", fake_ssh.display());
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &cmd])
        .status
        .success());
    let pane = s.display("", "#{pane_id}");
    let ready = common::wait_until(common::POLL_CEILING, || {
        basename(&s.display(&pane, "#{pane_current_command}")) == "ssh"
    });
    if !ready {
        eprintln!("skipping: the pane's foreground never became ssh");
        return;
    }
    // A stamp from before the boundary went up: no cycle can refresh it now.
    s.set_opt(&pane, "@agent_state", "working");

    let json = String::from_utf8_lossy(&tma(&s, &["doctor", "--json"]).stdout).to_string();
    assert!(
        json.contains("\"remote_panes\":[{"),
        "the pane is listed as remote: {json}"
    );
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\""))
            && json.contains("\"command\":\"ssh\"")
            && json.contains("\"stamped\":true"),
        "with its pane id, the matched command, and the held stamp: {json}"
    );

    let text = String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).to_string();
    assert!(
        text.contains("remote:")
            && text.contains("reach this tmux socket")
            && text.contains("agents-in-containers.md"),
        "the human form carries the condition and the recipe: {text}"
    );
}

/// A manifest the loader had to skip is named, with its parse error, in both output forms — and the
/// good sibling still loads, so one bad file degrades the roster instead of killing the surface.
#[test]
fn doctor_lists_a_skipped_manifest_and_still_loads_the_rest() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("badmanifest");
    let (_pane, names) = spawn_agent(&s);
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}]\n\
             [capture]\nvisible = [\"idle\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(s.workdir.join("broken.toml"), "min_engine_version = \n").unwrap();

    let out = tma(&s, &["doctor", "--json"]);
    assert!(out.status.success(), "a skipped manifest is not fatal");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("broken.toml"),
        "the skipped file is named: {json}"
    );
    assert!(
        json.contains("\"agent\":\"agent\""),
        "the good sibling still identifies the pane: {json}"
    );

    let text_out = tma(&s, &["doctor"]);
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        text.contains("1 skipped") && text.contains("broken.toml"),
        "the human-readable form names the skipped manifest: {text}"
    );

    // A poll surface warns on stderr (never stdout, so `--json` stays parseable) and still runs.
    let ls = tma(&s, &["ls", "--json"]);
    assert!(ls.status.success(), "the surface still runs");
    assert!(
        String::from_utf8_lossy(&ls.stderr).contains("skipping manifest"),
        "the surface warns about the skipped file on stderr"
    );
    assert!(
        !String::from_utf8_lossy(&ls.stdout).contains("skipping manifest"),
        "the warning never contaminates --json stdout"
    );
}

/// A model stamped through the hook intake (SessionStart's `model` field) that no
/// `[telemetry.windows]` entry names is reported as unrecognized (`window_covered:false`) without
/// gating red: nothing reads that table, so it is bookkeeping, not misconfiguration. Faithful to the
/// acceptance: the model reaches the pane via the real `tma event` hook path, not a bare option write.
#[test]
fn doctor_reports_an_unrecognized_hook_stamped_model_without_gating() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("model");
    let (pane, names) = spawn_agent(&s);
    // A claude manifest matching the pane, wired for SessionStart so the hook registers + stamps model.
    std::fs::write(
        s.workdir.join("claude.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}]\n\
             [hooks]\ncovers = [\"lifecycle\"]\n\
             [[hooks.map]]\nevent = \"SessionStart\"\nclaim = {{ lifecycle = \"start\" }}\n\
             [capture]\nvisible = []\n"
        ),
    )
    .unwrap();

    // Fire SessionStart carrying a model no entry names (only gemini-* ships as recognized).
    let payload = r#"{"session_id":"s","hook_event_name":"SessionStart","source":"startup","model":"claude-sonnet-5"}"#;
    let ev = tma_stdin(
        &s,
        &pane,
        &[
            "event",
            "--agent",
            "claude",
            "--kind",
            "SessionStart",
            "--payload",
            "-",
        ],
        payload,
    );
    assert!(ev.status.success(), "tma event must exit 0");
    assert_eq!(
        s.display(&pane, "#{@agent_model}"),
        "claude-sonnet-5",
        "the hook intake stamped @agent_model"
    );

    let out = tma(&s, &["doctor", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"model\":\"claude-sonnet-5\""),
        "the stamped model is reported: {json}"
    );
    assert!(
        json.contains("\"window_covered\":false"),
        "an unrecognized model is reported as such: {json}"
    );

    let doctor_text = tma(&s, &["doctor"]);
    let text = String::from_utf8_lossy(&doctor_text.stdout);
    assert!(
        text.contains("unrecognized; no [telemetry.windows] entry names it"),
        "the human-readable form says the name is unrecognized, not that a gauge is unsized: {text}"
    );
    assert!(
        !text.contains("size its gauge"),
        "and never tells the user to size a gauge from a table nothing reads: {text}"
    );
}

/// The server-wide posture checks in one pass on one scratch server: a detached server (no client
/// runs the `#()` status jobs), `status` turned off (no driver, no notifications), a manifest
/// `process_names` entry past the 15-char comm truncation, and a pane that registered through a hook
/// but is now running on capture evidence.
#[test]
fn doctor_flags_detached_server_status_off_long_process_name_and_hook_demotion() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("posture");
    let (pane, names) = spawn_agent(&s);
    // A name no truncated comm can ever match, with no 15-char spelling beside it.
    let long_name = "very-long-agent-binary";
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}, \"{long_name}\"]\n\
             [capture]\nvisible = [\"idle\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();
    assert!(s
        .tmux(&["set-option", "-g", "status", "off"])
        .status
        .success());
    // A hook-registered pane (a session id was stamped) whose current evidence came from capture.
    for (key, value) in [
        ("@agent_name", "agent"),
        ("@agent_state", "idle"),
        ("@agent_source", "capture"),
        ("@agent_session", "sess-1"),
    ] {
        assert!(s
            .tmux(&["set-option", "-p", "-t", &pane, key, value])
            .status
            .success());
    }

    let out = tma(&s, &["doctor", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"clients\":{\"attached\":0}"),
        "a detached server reports zero clients: {json}"
    );
    assert!(
        json.contains("\"status_option\":{\"enabled\":false}"),
        "`status off` is reported: {json}"
    );
    assert!(
        json.contains(&format!("\"name\":\"{long_name}\"")) && json.contains("\"comm_max\":15"),
        "the unreachable process name is reported with the truncation width: {json}"
    );
    assert!(
        json.contains("\"hook_demoted\":true"),
        "the registered-then-capture pane is flagged: {json}"
    );

    let text = String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).to_string();
    assert!(
        text.contains("nothing polls this server"),
        "the detached server is named: {text}"
    );
    assert!(
        text.contains("`status` option is off") && text.contains("display-message"),
        "both consequences of `status off` are named: {text}"
    );
    assert!(
        text.contains(long_name) && text.contains("truncate"),
        "the comm truncation is explained: {text}"
    );
    assert!(
        text.contains("demoted:"),
        "the demoted pane is named: {text}"
    );

    // The exit contract: plain doctor is a report (exit 0) whatever it finds; `--exit-code` turns
    // the same findings into a CI gate.
    assert!(
        tma(&s, &["doctor"]).status.success(),
        "plain doctor stays exit 0"
    );
    let gated = tma(&s, &["doctor", "--exit-code"]);
    assert!(
        !gated.status.success(),
        "--exit-code fails on a server with warnings"
    );
    assert!(
        String::from_utf8_lossy(&gated.stderr).contains("warning(s)"),
        "the gate says how many findings fired: {}",
        String::from_utf8_lossy(&gated.stderr)
    );
    assert!(
        String::from_utf8_lossy(&gated.stdout).contains("panes"),
        "the report still prints on stdout under --exit-code"
    );
}

/// The two halves of the clickable status segments are only useful together, so doctor pairs them:
/// the managed keys file carries the mouse group but the server's `mouse` option is off (tmux's
/// default, and tma never sets it) ⇒ a warning naming the fix. Turning the option on clears it.
#[test]
fn doctor_pairs_installed_mouse_bindings_against_the_mouse_option() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("mouse");
    spawn_agent(&s);

    // Before the install there is nothing to warn about, whatever the option says.
    let out = tma(&s, &["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!text.contains("`mouse` option is off"), "{text}");

    // `--conf` keeps the marked `source-file` line off the real ~/.tmux.conf (SAFETY).
    let conf = s.workdir.join(".tmux.conf");
    let installed = tma(
        &s,
        &[
            "install-keys",
            "--mouse",
            "--yes",
            "--conf",
            &conf.display().to_string(),
        ],
    );
    assert!(
        installed.status.success(),
        "install-keys failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );

    let text = String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).to_string();
    assert!(
        text.contains("`mouse` option is off") && text.contains("set -g mouse on"),
        "the warning names the option and its fix: {text}"
    );
    let json = String::from_utf8_lossy(&tma(&s, &["doctor", "--json"]).stdout).to_string();
    assert!(
        json.contains("\"mouse\":{\"bindings_installed\":true,\"enabled\":false}"),
        "{json}"
    );
    assert!(s
        .tmux(&["set-option", "-g", "mouse", "on"])
        .status
        .success());
    let text = String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).to_string();
    assert!(!text.contains("`mouse` option is off"), "{text}");
    let json = String::from_utf8_lossy(&tma(&s, &["doctor", "--json"]).stdout).to_string();
    assert!(json.contains("\"mouse\":{\"bindings_installed\":true,\"enabled\":true}"));
}

/// A `ps` the child cannot spawn (a stripped PATH, a sandbox that blocks the system copy) costs the
/// process walk and nothing else: the server-side half of the report still prints, the failure is
/// named where the missing pane rows are, and the exit stays 0 without `--exit-code`.
#[test]
fn doctor_degrades_when_the_process_walk_fails() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("no-ps");
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24"])
        .status
        .success());

    // A PATH with tmux and nothing else: every server read still works, `ps` is unreachable.
    let bin = s.workdir.join("path-without-ps");
    std::fs::create_dir_all(&bin).unwrap();
    let tmux = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("tmux"))
        .find(|p| p.is_file())
        .expect("tmux on PATH");
    std::os::unix::fs::symlink(tmux, bin.join("tmux")).unwrap();

    let out = tma_cmd(&s, &["doctor"])
        .env("PATH", &bin)
        .output()
        .expect("spawn tma");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "a failed walk is not a failed report");
    assert!(
        text.contains("procs:") && text.contains("`ps` is on PATH"),
        "the failure is named with its fix: {text}"
    );
    assert!(
        text.contains("daemon:") && text.contains("hooks:") && text.contains("wrapper:"),
        "the checks that do not need `ps` still print: {text}"
    );

    let json = String::from_utf8_lossy(
        &tma_cmd(&s, &["doctor", "--json"])
            .env("PATH", &bin)
            .output()
            .expect("spawn tma")
            .stdout,
    )
    .to_string();
    assert!(
        json.contains("\"process_walk\":{\"ok\":false,\"error\":\""),
        "the JSON carries the verdict and the error: {json}"
    );

    // The same server with `ps` reachable reports a clean walk and says nothing about it.
    let json = String::from_utf8_lossy(&tma(&s, &["doctor", "--json"]).stdout).to_string();
    assert!(
        json.contains("\"process_walk\":{\"ok\":true,\"error\":null}"),
        "{json}"
    );
    assert!(!String::from_utf8_lossy(&tma(&s, &["doctor"]).stdout).contains("procs:"));
}
