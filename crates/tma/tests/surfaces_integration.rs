//! `tma ls` / `tma status` and the poll-cycle driver acceptance on a scratch server.
//!
//! Scratch `tmux -L tma_test_<unique>` (`-f /dev/null`), killed on drop — never the default
//! server. Process names are discovered at runtime and written into a test-only manifest, so
//! the identity path works regardless of how `sleep` resolves on the host.

use std::process::Command;

use common::Scratch;
use tma_test_support as common;

/// Read a server-scoped user option (`show-options -sqv`), empty when unset. Suite-specific
/// (server scope, `-s`; the shared harness carries the pane-scoped `pane_option`), so a free helper.
fn server_opt(s: &Scratch, key: &str) -> String {
    let out = s.tmux(&["show-options", "-sqv", key]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run `tma <args>` against the scratch server + this suite's manifest dir, via `CARGO_BIN_EXE_tma`
/// (tests inside the `tma` package); behavior matches the shared `Scratch::tma`.
fn tma(s: &Scratch, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma")
}

/// Invoke tma with exactly the given argv — no auto-appended flags. Lets a test control where
/// `--socket-name` sits relative to the subcommand.
fn tma_raw(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(args)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma")
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Launch a fake agent pane (prints known chrome, then a long-lived process) and author a
/// manifest matching its real process names. Returns the pane id.
fn setup_agent(s: &Scratch, chrome: &str, rules: &str) -> String {
    let cmd = format!("printf '{chrome}'; exec sleep 100000");
    let out = s.tmux(&["new-session", "-d", "-x", "100", "-y", "24", &cmd]);
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

    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names_toml}]\n\
             [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n{rules}"
        ),
    )
    .unwrap();
    pane
}

fn captures_in(stderr: &str) -> Option<u32> {
    stderr
        .split(',')
        .find_map(|seg| seg.trim().strip_suffix(" captures"))
        .and_then(|n| n.trim().parse().ok())
}

#[test]
fn ls_json_correct_and_second_cycle_does_not_recapture() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("ls");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    // First cycle: produces (captures) the stale/never-stamped pane.
    let first = tma(&s, &["ls", "--json", "--debug-timing"]);
    assert!(first.status.success());
    let json = String::from_utf8_lossy(&first.stdout);
    let first_err = String::from_utf8_lossy(&first.stderr);

    assert!(
        json.contains("\"schema\":1"),
        "ls --json carries schema 1: {json}"
    );
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")),
        "row for the pane: {json}"
    );
    assert!(
        json.contains("\"agent\":\"agent\""),
        "agent name from manifest stem: {json}"
    );
    assert!(
        json.contains("\"state\":\"idle\""),
        "idle chrome detected: {json}"
    );
    assert_eq!(
        captures_in(&first_err),
        Some(1),
        "first cycle captures once: {first_err}"
    );

    // Second cycle within the freshness window: consume the fresh stamp, no capture.
    let second = tma(&s, &["ls", "--json", "--debug-timing"]);
    assert!(second.status.success());
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert_eq!(
        captures_in(&second_err),
        Some(0),
        "second cycle must not re-capture a fresh pane: {second_err}"
    );
}

fn skipped_in(stderr: &str) -> Option<u32> {
    stderr
        .split(',')
        .find_map(|seg| seg.trim().strip_suffix(" capture-skipped"))
        .and_then(|n| n.trim().parse().ok())
}

/// A pane whose window has produced no output since its stamp reuses that stamp instead of paying
/// another `capture-pane`, even on the producer path. Freshness is one second: long enough that
/// zero (which is an explicit "re-read every cycle" and bypasses the skip) is not in play, short
/// enough that a sleep past it puts each later cycle on the producer path.
#[test]
fn a_quiet_pane_skips_its_capture_on_the_producer_path() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("quiet");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    // The config lives outside the manifest dir: every `*.toml` in there is read as a manifest.
    let conf_dir = s.workdir.join("conf");
    std::fs::create_dir_all(&conf_dir).unwrap();
    let conf = conf_dir.join("config.toml");
    std::fs::write(&conf, "[fold]\nfreshness_secs = 1\n").unwrap();
    let conf = conf.to_str().unwrap().to_string();

    // `#{window_activity}` has one-second resolution and the skip demands the whole activity second
    // to precede the stamp, so wait out a full second before the first cycle stamps: without this
    // the two could share a second and the skip below would come down to boundary luck.
    std::thread::sleep(std::time::Duration::from_millis(1_200));

    let first = tma(&s, &["ls", "--debug-timing", "--config", &conf]);
    assert!(first.status.success());
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert_eq!(
        captures_in(&first_err),
        Some(1),
        "the first cycle must read the screen: {first_err}"
    );
    assert_eq!(skipped_in(&first_err), Some(0), "nothing to reuse yet");

    // Clear the stampede guard: two back-to-back cycles in the same second would otherwise consume
    // without reaching the producer path at all, which is not what this test is about.
    let clear_stampede = || {
        assert!(s
            .tmux(&["set-option", "-s", "@tma_last_poll", "0"])
            .status
            .success());
    };

    // Nothing has been written to the pane since. Sleep past the freshness window so the stamp is
    // stale and this cycle is a producer, not a consumer of a still-fresh stamp.
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    clear_stampede();
    let second = tma(&s, &["ls", "--debug-timing", "--config", &conf]);
    assert!(second.status.success());
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert_eq!(
        captures_in(&second_err),
        Some(0),
        "a quiet pane must not be re-captured: {second_err}"
    );
    assert_eq!(
        skipped_in(&second_err),
        Some(1),
        "and the skip is counted: {second_err}"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_state"),
        "idle",
        "the reused stamp is still the pane's state"
    );

    // Output moves `#{window_activity}` past the stamp, so the next cycle re-reads the screen.
    assert!(s.tmux(&["send-keys", "-t", &pane, "x"]).status.success());
    assert!(
        common::wait_until(common::POLL_CEILING, || {
            clear_stampede();
            let out = tma(&s, &["ls", "--debug-timing", "--config", &conf]);
            captures_in(&String::from_utf8_lossy(&out.stderr)) == Some(1)
        }),
        "activity after the stamp must force a capture"
    );
}

#[test]
fn status_renders_glyph_counts_for_agents() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("status");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let _pane = setup_agent(&s, "READY\\n", rules);

    let out = tma(&s, &["status"]);
    assert!(out.status.success());
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(
        status.contains("#[fg=green]○1"),
        "one idle agent renders as a green ○1: {status:?}"
    );
}

/// The four `--format` renderings of one live server: `tmux` (default, byte-identical to a bare
/// `tma status`), `plain` (same glyphs, no color codes), `json`, and `prom`. Each one is a full
/// ambient driver — the assertions only cover the rendering, since the cycle is shared.
#[test]
fn status_formats_render_the_same_counts_four_ways() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("statusfmt");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    let bare = String::from_utf8_lossy(&tma(&s, &["status"]).stdout).into_owned();
    let tmux_fmt = String::from_utf8_lossy(&tma(&s, &["status", "--format", "tmux"]).stdout)
        .trim_end()
        .to_string();
    assert_eq!(
        bare.trim_end(),
        tmux_fmt,
        "--format tmux is the unchanged default rendering"
    );
    assert!(bare.contains("#[fg=green]○1"), "{bare:?}");

    let plain = String::from_utf8_lossy(&tma(&s, &["status", "--format", "plain"]).stdout)
        .trim_end()
        .to_string();
    assert_eq!(plain, "○1", "plain keeps the glyph and drops the color");

    let json =
        String::from_utf8_lossy(&tma(&s, &["status", "--format", "json"]).stdout).into_owned();
    assert!(json.starts_with("{\"schema\":1,\"counts\":{"), "{json}");
    assert!(json.contains("\"idle\":1"), "{json}");
    assert!(json.contains("\"blocked\":0"), "zero classes stay: {json}");

    let prom =
        String::from_utf8_lossy(&tma(&s, &["status", "--format", "prom"]).stdout).into_owned();
    assert!(prom.contains("# TYPE tma_agents gauge"), "{prom}");
    assert!(prom.contains("tma_agents{state=\"idle\"} 1"), "{prom}");
    assert!(
        prom.contains(&format!("tma_agent_state_seconds{{pane=\"{pane}\",")),
        "the per-pane age series names the live pane: {prom}"
    );
}

/// The "done" surface: an idle agent still carrying `@agent_attention` renders as the done glyph in
/// `tma status` and exposes `"attention":true` (token still `idle`) in `tma ls --json`. Presentation
/// only, the flag is seeded on the pane.
#[test]
fn idle_with_attention_renders_as_done() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("done");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    // First cycle produces the idle stamp (and a fresh `@agent_stamped_at`).
    let first = tma(&s, &["status"]);
    assert!(first.status.success());
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("○1"),
        "first cycle: plain idle"
    );

    // Seed the presentation flag the write path sets on a working→idle completion.
    let set = s.tmux(&["set-option", "-p", "-t", &pane, "@agent_attention", "1"]);
    assert!(set.status.success(), "seed @agent_attention failed");

    // Within the freshness window the stamp is consumed as-is (no re-fold), so the seeded
    // attention reaches the surfaces: status renders the done glyph, not the idle glyph.
    let status = tma(&s, &["status"]);
    assert!(status.status.success());
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.contains("#[fg=magenta]✓1"),
        "idle + attention renders as the done glyph ✓: {status:?}"
    );
    assert!(
        !status.contains("○1"),
        "the row moved from idle to done, not double-counted: {status:?}"
    );

    let ls = tma(&s, &["ls", "--json"]);
    assert!(ls.status.success());
    let json = String::from_utf8_lossy(&ls.stdout);
    assert!(
        json.contains("\"state\":\"idle\""),
        "the @agent_state token stays idle (presentation only): {json}"
    );
    assert!(
        json.contains("\"attention\":true"),
        "ls --json exposes the attention flag additively: {json}"
    );
}

#[test]
fn status_is_the_sidebar_icon_alone_on_an_agentless_server() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("empty");
    // A plain shell pane and a manifest that matches nothing: no agents.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(out.status.success());
    std::fs::write(
        s.workdir.join("agent.toml"),
        "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"no-such-agent-xyz\"]\n\
         [capture]\nvisible = []\n",
    )
    .unwrap();
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "agent pane did not exec into `sleep`"
    );

    let out = tma(&s, &["status"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "#[range=user|tma:sidebar]#[fg=colour244]☰#[norange]",
        "every count is omitted, leaving only the always-present sidebar toggle"
    );

    // `plain` is the external-bar form and carries no icon, so it is still empty here.
    let plain = tma(&s, &["status", "--format", "plain"]);
    assert!(plain.status.success());
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "");
}

/// Regression: a lingering `@agent_summary` (window) or `@agent_session_summary` (session) with no
/// matching agent panes must be cleared by a poll cycle. Without the end-of-cycle reconciliation
/// nothing recomputes them once the last agent pane is gone but plain panes survive.
#[test]
fn stale_window_summary_is_cleared_by_a_cycle() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("stalesummary");
    // A plain shell pane and a manifest that matches nothing: the window has no agent.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(out.status.success());
    std::fs::write(
        s.workdir.join("agent.toml"),
        "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"no-such-agent-xyz\"]\n\
         [capture]\nvisible = []\n",
    )
    .unwrap();
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "agent pane did not exec into `sleep`"
    );

    let pane = s.display("", "#{pane_id}");
    // Author a phantom rollup on the window: no agent pane backs it.
    let set = s.tmux(&[
        "set-option",
        "-w",
        "-t",
        &pane,
        "@agent_summary",
        "blocked:1",
    ]);
    assert!(set.status.success());
    // And the same phantom at session scope.
    let set = s.tmux(&[
        "set-option",
        "-t",
        &pane,
        "@agent_session_summary",
        "blocked:1",
    ]);
    assert!(set.status.success());
    assert_eq!(
        s.display(&pane, "#{@agent_summary}"),
        "blocked:1",
        "phantom summary is present before the cycle"
    );

    let out = tma(&s, &["ls"]);
    assert!(out.status.success());

    assert_eq!(
        s.display(&pane, "#{@agent_summary}"),
        "",
        "reconciliation unsets a window summary with no matching agent panes"
    );
    assert_eq!(
        s.display(&pane, "#{@agent_session_summary}"),
        "",
        "and the session mirror with it"
    );
}

/// Documented degrade: on a server lacking `-F` expansion, a producing cycle uses the advisory (plain)
/// write path and still lands the detected state. Forced by seeding `@tma_setpf_ok=0`.
#[test]
fn advisory_path_writes_plain_state_when_setpf_unsupported() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("advisory");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    // Force the degrade: claim this server has no `-F` conditional-write support.
    let set = s.tmux(&["set-option", "-s", "@tma_setpf_ok", "0"]);
    assert!(set.status.success(), "seed @tma_setpf_ok failed");

    let out = tma(&s, &["ls"]);
    assert!(
        out.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The advisory (plain) write landed the detected state onto the pane.
    assert_eq!(
        s.display(&pane, "#{@agent_state}"),
        "idle",
        "advisory path writes the detected state plainly"
    );
    assert_eq!(
        s.display(&pane, "#{@agent_source}"),
        "capture",
        "advisory write records provenance"
    );
    // The stored value is literal, not a leftover format string.
    assert!(
        !s.display(&pane, "#{@agent_state}").contains("#{"),
        "advisory value must be literal"
    );
    // The forced cache is respected (the degrade did not overwrite it).
    assert_eq!(
        server_opt(&s, "@tma_setpf_ok"),
        "0",
        "forced advisory cache is honoured, not re-probed"
    );
}

/// Probe caching: a producing cycle on this (3.6a) server probes `-F` support once and
/// records the verdict in `@tma_setpf_ok`, so later one-shots skip the probe.
#[test]
fn probe_result_is_cached_after_a_producing_cycle() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("probecache");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let _pane = setup_agent(&s, "READY\\n", rules);

    // No pre-seed: the option is absent until the cycle probes.
    assert_eq!(
        server_opt(&s, "@tma_setpf_ok"),
        "",
        "unset before any cycle"
    );

    let out = tma(&s, &["ls"]);
    assert!(out.status.success());

    // The producing cycle probed and cached the result; 3.6a supports `-F`, so it is "1".
    assert_eq!(
        server_opt(&s, "@tma_setpf_ok"),
        "1",
        "producing cycle probes and caches -F support (verified on 3.6a)"
    );
}

/// Multi-agent: a window with two agent panes in distinct states must roll up to the correct summary
/// after one cycle. The per-`apply()` append rolls each pane against a stale sibling snapshot; only the
/// end-of-cycle reconciliation, from full window membership and this cycle's verdicts, is right.
#[test]
fn multi_agent_window_rolls_up_correctly() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("multiagent");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n\
                 [[rules]]\nstate = \"blocked\"\npriority = 60\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"PROCEED?\" }\n";
    // First agent pane (idle) — also authors the manifest matching the pane's real process.
    let pane1 = setup_agent(&s, "READY\\n", rules);

    // Second agent pane (blocked) in the *same* window: split, capture its id from `-P -F`.
    let out = s.tmux(&[
        "split-window",
        "-t",
        &pane1,
        "-P",
        "-F",
        "#{pane_id}",
        "printf 'PROCEED?\\n'; exec sleep 100000",
    ]);
    assert!(
        out.status.success(),
        "split-window failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane2 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(pane2.starts_with('%'), "unexpected pane id {pane2:?}");
    // `tma ls` classifies the second pane from its on-screen chrome, so wait for that chrome to
    // actually render rather than guessing a fixed delay.
    assert!(
        common::wait_capture_contains(&s.socket, &pane2, "PROCEED?", common::POLL_CEILING),
        "second pane's chrome did not render"
    );

    let out = tma(&s, &["ls"]);
    assert!(
        out.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Fixed order (blocked working idle unknown), zeros omitted, shared across the window.
    assert_eq!(
        s.display(&pane1, "#{@agent_summary}"),
        "blocked:1 idle:1",
        "window rolls up both agents in canonical order"
    );
    assert_eq!(
        s.display(&pane2, "#{@agent_summary}"),
        "blocked:1 idle:1",
        "the summary is a shared window option"
    );
    assert_eq!(
        s.display(&pane1, "#{@agent_session_summary}"),
        "blocked:1 idle:1",
        "the session mirror rolls up the same members in the same grammar"
    );
}

/// The selector filters DISPLAY only. A scoped `tma status` counts one session while the very same
/// invocation still stamps every agent pane on the server — the invariant a per-session status-line
/// driver depends on (a filtered cycle would leave the hidden panes stale forever).
#[test]
fn scoped_status_counts_one_session_and_still_stamps_the_others() {
    if !tma_test_support::tmux_available() {
        return;
    }
    let s = Scratch::new("scopedstatus");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    // The first session's agent pane also authors the manifest matching its real process.
    let pane1 = setup_agent(&s, "READY\\n", rules);
    let session1 = s.display(&pane1, "#{session_name}");

    // A second session running the same command, so the same manifest identifies it.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "scoped",
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
    let pane2 = s.display("scoped", "#{pane_id}");
    assert!(
        common::wait_capture_contains(&s.socket, &pane2, "READY", common::POLL_CEILING),
        "second session's chrome did not render"
    );

    // Unscoped: both agents counted.
    let all = tma(&s, &["status"]);
    assert!(all.status.success());
    assert_eq!(
        String::from_utf8_lossy(&all.stdout),
        "#[range=user|tma:idle]#[fg=green]○2#[norange] \
         #[range=user|tma:sidebar]#[fg=colour244]☰#[norange]",
        "the unscoped status counts both sessions"
    );

    // Clear the stamps so the scoped run has to produce them again for BOTH panes.
    for pane in [&pane1, &pane2] {
        for key in ["@agent_state", "@agent_stamped_at", "@agent_evidence_at"] {
            assert!(s
                .tmux(&["set-option", "-p", "-u", "-t", pane, key])
                .status
                .success());
        }
    }

    let scoped = tma(&s, &["status", "--session", "scoped"]);
    assert!(scoped.status.success());
    assert_eq!(
        String::from_utf8_lossy(&scoped.stdout),
        "#[range=user|tma:idle]#[fg=green]○1#[norange] \
         #[range=user|tma:sidebar]#[fg=colour244]☰#[norange]",
        "the scoped status counts only the named session"
    );
    // The out-of-scope pane was still stamped by that same cycle.
    assert_eq!(
        s.display(&pane1, "#{@agent_state}"),
        "idle",
        "a filtered surface still refreshes the panes it does not show"
    );
    assert_ne!(session1, "scoped", "the two sessions are distinct");

    // `ls --session` narrows the printed rows the same way.
    let rows = tma(&s, &["ls", "--session", "scoped"]);
    let text = String::from_utf8_lossy(&rows.stdout);
    assert_eq!(text.lines().count(), 1, "one row in scope: {text:?}");
    assert!(
        text.contains(&pane2),
        "the in-scope pane is the one printed: {text:?}"
    );
}

/// Footgun regression: `--socket-name` / `--manifest-dir` are clap globals, targeting the same server
/// before or after the subcommand. Before the fix, a top-level `tma --socket-name X ls` bound X to a
/// duplicate picker-only field while `run_ls` read an empty copy, silently hitting the DEFAULT server.
/// This drives both orderings against the scratch server and asserts each returns its own agent pane.
#[test]
fn socket_name_targets_scratch_server_in_either_position() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("globalsock");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);

    // Ordering 1: globals BEFORE the subcommand (the ordering that regressed).
    let before = tma_raw(&[
        "--socket-name",
        &s.socket,
        "--manifest-dir",
        &s.workdir.to_string_lossy(),
        "ls",
        "--json",
    ]);
    assert!(
        before.status.success(),
        "ls (globals first) failed: {}",
        String::from_utf8_lossy(&before.stderr)
    );
    let before_json = String::from_utf8_lossy(&before.stdout);
    assert!(
        before_json.contains(&format!("\"pane\":\"{pane}\"")),
        "globals-first must target the scratch server (its pane appears): {before_json}"
    );
    assert!(
        before_json.contains("\"agent\":\"agent\""),
        "globals-first sees the scratch server's agent: {before_json}"
    );

    // Ordering 2: globals AFTER the subcommand — same canonical field, same server.
    let after = tma_raw(&[
        "ls",
        "--json",
        "--socket-name",
        &s.socket,
        "--manifest-dir",
        &s.workdir.to_string_lossy(),
    ]);
    assert!(
        after.status.success(),
        "ls (globals last) failed: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    let after_json = String::from_utf8_lossy(&after.stdout);
    assert!(
        after_json.contains(&format!("\"pane\":\"{pane}\"")),
        "globals-last targets the scratch server too: {after_json}"
    );
    assert!(
        after_json.contains("\"agent\":\"agent\""),
        "globals-last sees the scratch server's agent: {after_json}"
    );
}

/// `--socket-path` reaches the same server `--socket-name` does (tmux `-S` against `-L`), the
/// `TMA_SOCKET_PATH` env is its fallback, and naming a server both ways is a usage error (exit 2).
#[test]
fn socket_path_targets_the_server_and_conflicts_with_socket_name() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("sockpath");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let pane = setup_agent(&s, "READY\\n", rules);
    // The server's own view of its socket, which is what `-S` must be handed.
    let socket_path = s.display("", "#{socket_path}");
    assert!(!socket_path.is_empty(), "resolved a server socket path");
    let workdir = s.workdir.to_string_lossy().into_owned();

    let by_path = tma_raw(&[
        "--socket-path",
        &socket_path,
        "--manifest-dir",
        &workdir,
        "ls",
        "--json",
    ]);
    assert!(
        by_path.status.success(),
        "ls --socket-path failed: {}",
        String::from_utf8_lossy(&by_path.stderr)
    );
    assert!(
        String::from_utf8_lossy(&by_path.stdout).contains(&format!("\"pane\":\"{pane}\"")),
        "--socket-path reached the scratch server: {}",
        String::from_utf8_lossy(&by_path.stdout)
    );

    // The env fallback: no socket flag at all, TMA_SOCKET_PATH names the same server.
    let by_env = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["ls", "--json", "--manifest-dir", &workdir])
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_SOCKET_PATH", &socket_path)
        .output()
        .expect("spawn tma");
    assert!(by_env.status.success());
    assert!(
        String::from_utf8_lossy(&by_env.stdout).contains(&format!("\"pane\":\"{pane}\"")),
        "TMA_SOCKET_PATH is the fallback target: {}",
        String::from_utf8_lossy(&by_env.stdout)
    );

    // An explicit flag beats the env, even when the env points at a live server.
    let flag_wins = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["ls", "--socket-name", "tma_test_no_such_server", "--json"])
        .env("TMA_CONFIG", common::empty_config_path())
        .env("TMA_SOCKET_PATH", &socket_path)
        .output()
        .expect("spawn tma");
    assert!(
        !String::from_utf8_lossy(&flag_wins.stdout).contains(&pane),
        "--socket-name must not fall back to TMA_SOCKET_PATH"
    );

    // Both flags at once is a usage error, not a silent precedence rule.
    let both = tma_raw(&[
        "ls",
        "--socket-name",
        &s.socket,
        "--socket-path",
        &socket_path,
    ]);
    assert_eq!(
        both.status.code(),
        Some(2),
        "naming the server twice is exit 2: {}",
        String::from_utf8_lossy(&both.stderr)
    );
}

/// Every JSON row carries where it was observed: `server` is the tmux server's own `#{socket_path}`
/// and `host` this machine's name, so rows merged from two boxes stay addressable.
#[test]
fn json_rows_carry_the_server_and_host_they_came_from() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("origin");
    let rules = "[[rules]]\nstate = \"idle\"\npriority = 50\n\
                 region = \"tail_lines(50)\"\nmatch = { contains = \"READY\" }\n";
    let _pane = setup_agent(&s, "READY\\n", rules);
    let socket_path = s.display("", "#{socket_path}");

    let out = tma(&s, &["ls", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(&format!("\"server\":\"{socket_path}\"")),
        "the row names the server socket it came from: {json}"
    );
    assert!(
        json.contains(&format!("\"host\":\"{}\"", tma_runtime::origin::hostname())),
        "the row names this host: {json}"
    );
}

/// The tmux binary is configurable, and `TMA_TMUX_BIN` beats `[tmux] bin`. No tmux server is needed:
/// an unresolvable binary is caught at construction and reported as the guided not-installed error,
/// which is exactly the signal each half of the precedence is read from.
#[test]
fn tmux_bin_comes_from_config_with_the_env_overriding() {
    let run = |config: Option<&std::path::Path>, env_bin: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tma"));
        cmd.arg("ls");
        match config {
            Some(p) => cmd.env("TMA_CONFIG", p),
            None => cmd.env("TMA_CONFIG", common::empty_config_path()),
        };
        if let Some(bin) = env_bin {
            cmd.env("TMA_TMUX_BIN", bin);
        }
        let out = cmd.output().expect("spawn tma");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    // A config-named binary that does not resolve: the not-installed hint, not a per-spawn error.
    let dir = std::env::temp_dir().join(format!("tma-binoverride-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("config.toml");
    std::fs::write(&conf, "[tmux]\nbin = \"/no/such/tmux\"\n").unwrap();
    assert!(
        run(Some(&conf), None).contains("tmux is not installed"),
        "[tmux] bin is the binary tma resolves"
    );

    // The env alone does the same.
    assert!(run(None, Some("/no/such/tmux")).contains("tmux is not installed"));

    // Env over config: `sh` resolves, so construction succeeds and the failure is whatever `sh`
    // makes of tmux's argv — anything but the not-installed hint the config value would have given.
    let stderr = run(Some(&conf), Some("sh"));
    assert!(
        !stderr.contains("tmux is not installed"),
        "TMA_TMUX_BIN must win over [tmux] bin: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
