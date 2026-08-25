//! `tma act` acceptance, driving the built binary against a scratch `tmux -L` server (killed on
//! drop). A pane is stamped as a fresh `blocked/permission` claude agent so the freshness re-verify
//! is skipped (no real claude process needed) and the guarded keys path runs end to end. Exit codes
//! are the contract, so every case pins `status.code()`; the `--json` cases parse the result object.
//!
//! `XDG_CONFIG_HOME` is pinned at the workdir so the user action dir is empty and only the bundled
//! actions (approve/deny/interrupt/compact) load — a developer's real `~/.config/tma/actions` never
//! leaks in.

use std::process::{Command, Output};

use tma_test_support::{
    empty_config_path, wait_capture_contains, AttachOutcome, Scratch, POLL_CEILING,
};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// Stamp `pane` as a fresh `blocked/permission` claude agent: the gate passes and the keys action
/// skips re-verify (fresh `@agent_stamped_at`).
fn stamp_blocked_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

/// Stamp `pane` as a fresh `blocked/permission` opencode agent with a pending request id and an
/// endpoint: the fireable state for the `[api]` `permission-reply` lane.
fn stamp_blocked_opencode(s: &Scratch, pane: &str, request_id: &str, endpoint: &str) {
    stamp_blocked_claude(s, pane);
    s.set_opt(pane, "@agent_name", "opencode");
    s.set_opt(pane, "@agent_permission_request", request_id);
    s.set_opt(pane, "@agent_api_endpoint", endpoint);
}

/// A one-shot HTTP/1.1 server on `127.0.0.1:0` that answers `status_line` to the first request;
/// returns `(http_base_url, join_handle)`.
fn mock_http(status_line: &'static str) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.read(&mut [0u8; 1024]);
        let _ = stream.write_all(
            format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
        );
    });
    (base, handle)
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

/// A user action with no `when`, so nothing but target resolution can decide the exit code: the
/// bundled actions all gate on a state this idle pane is not in.
fn write_ungated_action(s: &Scratch) {
    let dir = s.workdir.join("tma/actions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ping.toml"),
        "min_engine_version = \"0.1\"\nname = \"ping\"\nlabel = \"Ping\"\nkind = \"keys\"\n\
         [keys]\nclaude = [\"p\"]\n",
    )
    .unwrap();
}

/// Run `tma act <args>` against the scratch server, with the user action dir pinned empty
/// (`XDG_CONFIG_HOME` at the workdir) so only the bundled actions load. `TMA_CONFIG` points at the
/// scratch's own config path, which is the zero-config floor until a test writes one
/// ([`hold_the_stamp`]).
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

/// Widen the stamp-freshness window for the tests whose target resolution runs a cycle. Their panes
/// are shells wearing a hand-written claude stamp: only the consumer path keeps that fiction alive,
/// and under parallel load the default 3 s window can lapse between the stamp and the invocation,
/// which would let the producer path correctly unmask them mid-test.
fn hold_the_stamp(s: &Scratch) {
    s.write_config("[fold]\nfreshness_secs = 600\n");
}

#[test]
fn keys_action_fires_and_exits_zero() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_fire");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["approve", "--pane", &pane]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "approve fires on a blocked pane"
    );
    assert!(
        wait_capture_contains(&s.socket, &pane, "1", POLL_CEILING),
        "the approve keystroke `1` should reach the pane"
    );
    // The single-flight lock is released on the send path (empty == absent).
    assert!(
        s.pane_option(&pane, "@agent_action").is_empty(),
        "the lock should be cleared"
    );
}

/// A live pane whose `send-keys` tmux rejects reports an error carrying tmux's own words, not
/// "pane vanished". Every failed pane command used to fold to `vanished` (exit 3), so a key
/// spelling tmux would not take reported a pane that is plainly still there, with no diagnostic.
#[test]
fn a_key_tmux_rejects_reports_the_error_with_its_stderr() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_bad_key");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    // `-Z` is not a key name: tmux's own getopt reads it as a flag and refuses the command.
    let dir = s.workdir.join("tma/actions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bad-key.toml"),
        "min_engine_version = \"0.1\"\nname = \"bad-key\"\nlabel = \"Bad key\"\nkind = \"keys\"\n\
         [keys]\nclaude = [\"-Z\"]\n",
    )
    .unwrap();

    let out = act(&s, &["bad-key", "--pane", &pane, "--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a rejected command is a broker error (exit 1), not a vanished pane (exit 3)"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""outcome":"error""#),
        "the JSON result says error: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("-Z"),
        "tmux's own stderr reaches the user: {stderr}"
    );
    assert!(
        !stderr.contains("vanished"),
        "the pane is still there: {stderr}"
    );
}

#[test]
fn gate_refusal_exits_four() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_gated");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    // Flip to idle: approve's `when` (blocked/permission) no longer holds.
    s.set_opt(&pane, "@agent_state", "idle");

    let out = act(&s, &["approve", "--pane", &pane]);
    assert_eq!(out.status.code(), Some(4), "a gated action refuses with 4");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("gated"),
        "the refusing fact names the gate: {stderr}"
    );
}

#[test]
fn json_result_object_on_a_sent_action() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_json");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["approve", "--pane", &pane, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"schema\":1"), "schema present: {stdout}");
    assert!(
        stdout.contains("\"outcome\":\"sent\""),
        "outcome sent: {stdout}"
    );
    assert!(stdout.contains("\"exit_code\":0"), "exit_code 0: {stdout}");
    assert!(stdout.contains("\"reason\":null"), "null reason: {stdout}");
    assert!(stdout.contains(&format!("\"pane\":\"{pane}\"")));
}

#[test]
fn unknown_action_exits_two() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_unknown");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["nope", "--pane", &pane]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unknown action is a usage error"
    );
}

#[test]
fn vanished_pane_exits_three() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_vanished");
    let _pane = s.new_shell_pane();
    let out = act(&s, &["approve", "--pane", "%999", "--json"]);
    assert_eq!(out.status.code(), Some(3), "a nonexistent pane is exit 3");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""reason":"pane-gone""#),
        "the tmux producer of `vanished` names the pane: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pane %999 vanished"),
        "a genuinely gone pane still says so: {stderr}"
    );
}

/// A permission reply the server answers `404` is the REQUEST that went away, not the pane. The
/// `vanished` outcome and exit 3 are deliberate (the act's target disappeared), but the message and
/// the `reason` token have to name which target: re-firing approve at an already-answered request is
/// the most likely refusal on the API lane and "pane vanished" points the user at tmux for something
/// tmux did not do.
#[test]
fn api_404_is_request_gone_and_never_blames_the_pane() {
    if !have_tmux() {
        return;
    }
    let (endpoint, server) = mock_http("HTTP/1.1 404 Not Found");
    let s = Scratch::new("act_api_404");
    let pane = s.new_shell_pane();
    stamp_blocked_opencode(&s, &pane, "per_spent", &endpoint);

    let out = act(&s, &["approve", "--pane", &pane, "--json"]);
    let _ = server.join();
    assert_eq!(
        out.status.code(),
        Some(3),
        "a spent request is still exit 3"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""outcome":"vanished""#),
        "the outcome token does not move: {stdout}"
    );
    assert!(
        stdout.contains(r#""reason":"request-gone""#),
        "the reason names the request: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("pane"),
        "the pane is alive and must not be blamed: {stderr}"
    );
    assert!(
        stderr.contains("already answered or withdrawn"),
        "the line says what actually happened: {stderr}"
    );
    // The pane really is still there, so the assertion above is not vacuously true.
    assert_eq!(s.pane_option(&pane, "@agent_name"), "opencode");
}

/// A 2xx reply spends the pending request, so tma clears `@agent_permission_request` itself rather
/// than waiting for the plugin's `permission.replied` event. A 404 leaves the stamp alone.
#[test]
fn a_replied_permission_clears_the_request_stamp() {
    if !have_tmux() {
        return;
    }
    let (endpoint, server) = mock_http("HTTP/1.1 200 OK");
    let s = Scratch::new("act_api_clear");
    let pane = s.new_shell_pane();
    stamp_blocked_opencode(&s, &pane, "per_live", &endpoint);

    let out = act(&s, &["approve", "--pane", &pane, "--json"]);
    let _ = server.join();
    assert_eq!(out.status.code(), Some(0), "the reply landed");
    assert_eq!(
        s.pane_option(&pane, "@agent_permission_request"),
        "",
        "a spent request id must not outlive its reply"
    );
}

/// `--all` fans out over every selector-matched pane: both panes get the keystroke, the envelope
/// carries one result object each, and the batch exits 0 because both acted.
#[test]
fn all_fans_out_over_every_matched_pane() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_all");
    hold_the_stamp(&s);
    let pane_a = s.new_shell_pane();
    let pane_b = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane_a);
    stamp_blocked_claude(&s, &pane_b);

    let out = act(&s, &["approve", "--all", "--agent", "claude", "--json"]);
    assert_eq!(out.status.code(), Some(0), "both panes acted");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("{\"schema\":1,\"results\":["),
        "the fan-out envelope: {stdout}"
    );
    for pane in [&pane_a, &pane_b] {
        assert!(
            stdout.contains(&format!("\"pane\":\"{pane}\"")),
            "{pane} has a result: {stdout}"
        );
        assert!(
            wait_capture_contains(&s.socket, pane, "1", POLL_CEILING),
            "the approve keystroke should reach {pane}"
        );
        assert!(
            s.pane_option(pane, "@agent_action").is_empty(),
            "each target releases its own lock"
        );
    }
}

/// A fan-out exits with its WORST result: one gated pane makes the batch exit 4 even though the
/// other one fired (and fire it did — a refusal does not abort the rest of the batch).
#[test]
fn all_reports_the_worst_outcome_and_still_fires_the_rest() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_all_worst");
    hold_the_stamp(&s);
    let good = s.new_shell_pane();
    let gated = s.new_shell_pane();
    stamp_blocked_claude(&s, &good);
    stamp_blocked_claude(&s, &gated);
    s.set_opt(&gated, "@agent_state", "idle"); // approve's `when` no longer holds here

    let out = act(&s, &["approve", "--all", "--agent", "claude", "--json"]);
    assert_eq!(out.status.code(), Some(4), "the gate refusal is the worst");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"outcome\":\"sent\""),
        "one fired: {stdout}"
    );
    assert!(
        stdout.contains("\"reason\":\"gated\""),
        "one refused: {stdout}"
    );
    assert!(
        wait_capture_contains(&s.socket, &good, "1", POLL_CEILING),
        "the fireable pane still received its keystroke"
    );
}

/// `--all` with a selector nothing matches is a usage error, not a silent no-op.
#[test]
fn all_with_no_matching_pane_exits_two() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_all_empty");
    hold_the_stamp(&s);
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["approve", "--all", "--agent", "nosuchagent"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nothing to act on"),
        "the message says why: {stderr}"
    );
}

/// `--all --dry-run` lists the resolved targets with each one's verdict and fires nothing.
#[test]
fn all_dry_run_lists_targets_and_verdicts_without_firing() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_all_dry");
    hold_the_stamp(&s);
    let fireable = s.new_shell_pane();
    let gated = s.new_shell_pane();
    stamp_blocked_claude(&s, &fireable);
    stamp_blocked_claude(&s, &gated);
    s.set_opt(&gated, "@agent_state", "idle");

    let out = act(&s, &["approve", "--all", "--agent", "claude", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "a dry run reports, never fails");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("targets: 2"), "target count: {stdout}");
    assert!(
        stdout.contains(&format!("{fireable} ")) && stdout.contains("would fire"),
        "the fireable target: {stdout}"
    );
    assert!(
        stdout.contains(&format!("{gated} ")) && stdout.contains("refused: gated"),
        "the gated target: {stdout}"
    );
    for pane in [&fireable, &gated] {
        assert!(
            s.pane_option(pane, "@agent_action").is_empty(),
            "a dry run acquires no lock"
        );
    }
}

/// An ambiguous selection without `--all` still refuses (exit 1) rather than picking one, and now
/// points at `--all` as the way to mean every pane.
#[test]
fn ambiguous_selection_without_all_exits_one() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_ambiguous");
    hold_the_stamp(&s);
    let pane_a = s.new_shell_pane();
    let pane_b = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane_a);
    stamp_blocked_claude(&s, &pane_b);

    let out = act(&s, &["approve", "--agent", "claude"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&pane_a) && stderr.contains(&pane_b));
    assert!(stderr.contains("--all"), "it names the fan-out: {stderr}");
}

/// Write a user `exec` action that records its `TMA_ARG*` environment to `out`, into the user action
/// dir the [`act`] helper pins (`XDG_CONFIG_HOME` at the workdir).
fn write_echo_arg_action(s: &Scratch, out: &std::path::Path) {
    let dir = s.workdir.join("tma/actions");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("echo-arg.sh");
    std::fs::write(
        &script,
        // Every expansion quoted: the values are untrusted text that must stay data.
        "#!/bin/sh\nprintf 'arg=%s count=%s two=%s\\n' \"$TMA_ARG\" \"$TMA_ARG_COUNT\" \"${TMA_ARG_2:-}\" > \"$1\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("echo-arg.toml"),
        format!(
            "min_engine_version = \"0.1\"\nname = \"echo-arg\"\nlabel = \"Echo\"\nkind = \"exec\"\n\
             command = \"sh {} {}\"\n",
            script.display(),
            out.display()
        ),
    )
    .unwrap();
}

/// `--arg` values reach an exec child as environment, verbatim: `TMA_ARG` is the first, the rest are
/// numbered, and shell metacharacters stay inert because nothing is interpolated into the command.
#[test]
fn arg_values_reach_an_exec_action_as_environment() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_arg");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    let out = s.workdir.join("echo-arg.out");
    write_echo_arg_action(&s, &out);

    let output = act(
        &s,
        &[
            "echo-arg",
            "--pane",
            &pane,
            "--arg",
            "review PR 412",
            "--arg",
            "$(touch /tmp/tma-should-not-exist)",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "the exec child exited 0");
    let recorded = std::fs::read_to_string(&out).expect("the action wrote its env");
    assert_eq!(
        recorded.trim(),
        "arg=review PR 412 count=2 two=$(touch /tmp/tma-should-not-exist)",
        "values arrive verbatim, unexpanded"
    );
    assert!(
        !std::path::Path::new("/tmp/tma-should-not-exist").exists(),
        "the substitution in a value must never have been evaluated"
    );
}

/// A `keys` action refuses `--arg` (exit 2): its sequence is manifest-static, so there is nowhere
/// for a value to go — and silently dropping one would be worse.
#[test]
fn keys_action_rejects_arg_values() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_arg_keys");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["approve", "--pane", &pane, "--arg", "nope"]);
    assert_eq!(out.status.code(), Some(2), "a keys action takes no --arg");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keys action") && stderr.contains("--arg"),
        "the refusal says why: {stderr}"
    );
}

#[test]
fn list_json_enumerates_bundled_actions() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_list");
    let _pane = s.new_shell_pane();

    let out = act(&s, &["--list", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"schema\":1"));
    for want in ["approve", "deny", "interrupt", "compact"] {
        assert!(
            stdout.contains(&format!("\"name\":\"{want}\"")),
            "lists {want}: {stdout}"
        );
    }
    // No `--pane`, so no per-action verdict keys.
    assert!(!stdout.contains("fireable"), "no verdict without --pane");
}

#[test]
fn list_with_pane_carries_fireability() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("act_list_pane");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let out = act(&s, &["--list", "--json", "--pane", &pane]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // approve is fireable on a blocked/permission pane; compact refuses no-coverage (no telemetry).
    assert!(
        stdout.contains("\"fireable\":true"),
        "a fireable verdict appears: {stdout}"
    );
    assert!(
        stdout.contains("\"reason\":\"no-coverage\""),
        "compact has no telemetry: {stdout}"
    );
}

/// `--state done` must match a pane whose marker `act`'s OWN target-resolution cycle would
/// otherwise have retracted. `done` is idle + `@agent_attention`, and the ordered-input clear used
/// to run inside that cycle: a client parked on the pane plus one keystroke after the raise arms the
/// clear, the cycle strips the flag before the selector reads the rows, and the pane the operator
/// asked for resolves to nothing (exit 3).
#[test]
fn act_does_not_retract_the_done_mark_it_is_selecting_on() {
    if !have_tmux() {
        return;
    }
    let mut s = Scratch::new("act_seen");
    hold_the_stamp(&s);
    let pane = s.new_shell_pane();
    let session = s.display(&pane, "#{session_name}");
    stamp_idle_claude(&s, &pane);
    write_ungated_action(&s);

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

    let out = act(&s, &["ping", "--state", "done"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--state done must resolve the pane whose mark was standing when the cycle read it: {stderr}"
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
