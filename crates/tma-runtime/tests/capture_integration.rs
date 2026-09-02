//! Acceptance: on-demand capture (quiet edge + contradiction), the reconciliation sweep, and
//! hook-liveness demotion, on an isolated scratch tmux server.
//!
//! Every daemon is a foreground `tma daemon` child reaped on drop ([`DaemonGuard`]), targeting only
//! the scratch server (`--socket-name` + private `XDG_RUNTIME_DIR` + `--manifest-dir`). The "agent"
//! is a real process (a shell or `sleep`) whose comm is discovered at runtime and written into a
//! test manifest, so the identity walk runs on both procps and BSD. Introspection is the daemon
//! `--status-file` counters.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tma_tmux::tmux::ps_all;

use common::{DaemonGuard, Scratch};
use tma_test_support as common;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// A detached session running a MINIMAL interactive shell (so `send-keys` produces output ⇒ activity
/// edges). Returns `(pane_id, pane_pid)`. The shell is isolated the way the tmux server is: `env -i`
/// keeps the developer's rc files out, so no themed prompt wraps a typed marker across two screen
/// lines and no prompt hook sprays output of its own into the edge accounting.
fn new_shell_session(s: &Scratch, name: &str) -> (String, u32) {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let shell = format!(
        "exec env -i PATH={path} HOME={home} TERM=xterm-256color PS1='tma> ' sh -i",
        home = s.workdir.display()
    );
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            &shell
        ])
        .status
        .success());
    let pane = s.get(name, "#{pane_id}");
    assert!(pane.starts_with('%'), "got pane {pane:?}");
    // The shell must be up before its name is read or keys are sent to it: a just-created pane can
    // still report the pre-exec command under load, which would author a manifest matching nothing.
    // Its own echo of the marker proves the shell reached its prompt and is reading input.
    s.tmux(&[
        "send-keys",
        "-t",
        &pane,
        "printf 'tma-shell-ready\\n'",
        "Enter",
    ]);
    assert!(
        common::wait_capture_contains(&s.socket, &pane, "tma-shell-ready", common::POLL_CEILING),
        "the pane shell never reached its prompt"
    );
    let pid: u32 = s.get(name, "#{pane_pid}").parse().unwrap();
    (pane, pid)
}

/// A detached `exec sleep` session (no output ⇒ no activity edges). Returns `(pane_id, pane_pid)`.
fn new_idle_session(s: &Scratch, name: &str) -> (String, u32) {
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    let pane = s.get(name, "#{pane_id}");
    assert!(pane.starts_with('%'), "got pane {pane:?}");
    let pid: u32 = s.get(name, "#{pane_pid}").parse().unwrap();
    // The pane execs `sleep` over the spawning shell; until that lands both name reads still see the
    // shell and the manifest they author matches nothing. The pane pid's comm is the signal — tmux's
    // own name can be the multi-call binary (`coreutils` under uutils), so it cannot serve here.
    assert!(
        common::wait_until(common::POLL_CEILING, || comm_of(pid) == "sleep"),
        "the idle pane never exec'd its sleep"
    );
    (pane, pid)
}

/// The comm basename of a pid (procps/BSD portable), for authoring a matching manifest.
fn comm_of(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    basename(&String::from_utf8_lossy(&out.stdout))
}

/// The `process_names = [..]` literal for a pane's agent: both `#{pane_current_command}` (so
/// `foreground_is_agent` matches) and the `ps` comm (they differ for login shells), deduped.
fn process_names_toml(s: &Scratch, pane_name: &str, pid: u32) -> String {
    let cur = basename(&s.get(pane_name, "#{pane_current_command}"));
    let mut names = vec![cur, comm_of(pid)];
    names.sort();
    names.dedup();
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `--sweep-ms` every on-demand test here pins: 10 minutes, ~13x [`common::POLL_CEILING`].
///
/// These tests assert `sweeps == 0` to prove the capture came from the on-demand tier. Left at the
/// default the cadence is 45 s — the SAME number as the ceiling they wait out — so the assertion
/// rests on two unrelated constants happening to be equal; and a push probe that degrades (a real
/// possibility on a loaded box: `ProbeOutcome::Unavailable` shortens the cadence to `SWEEP_DEGRADED`
/// = 5 s) puts a sweep well inside the window, where it would silently rescue the pane and green a
/// run in which the on-demand tier did nothing at all. Pinning the cadence out of reach makes
/// `sweeps == 0` true by construction, which is what turns the positive assertion into a real one.
const SWEEP_PINNED_OUT_MS: &str = "600000";

/// The daemon args pinning the sweep out of reach ([`SWEEP_PINNED_OUT_MS`]).
const PINNED_SWEEP: &[&str] = &["--sweep-ms", SWEEP_PINNED_OUT_MS];

fn write_manifest(s: &Scratch, body: &str) {
    std::fs::write(s.workdir.join("agent.toml"), body).unwrap();
}

/// Type a command line into the shell pane and press Enter — one output burst ⇒ one edge.
fn burst(s: &Scratch, pane: &str, line: &str) {
    assert!(s
        .tmux(&["send-keys", "-t", pane, line, "Enter"])
        .status
        .success());
}

/// Fire `tma event` as a hook would (delivered to the running daemon over its socket). Retried once
/// on a transient failure under parallel fork pressure.
fn fire(s: &Scratch, agent: &str, kind: &str, pane: &str, payload: &str) {
    use std::io::Write;
    let run = || -> bool {
        let spawned = s
            .command()
            .args(["event", "--agent", agent, "--kind", kind, "--payload", "-"])
            .args(["--socket-name", &s.socket])
            .args(["--manifest-dir", s.workdir.to_str().unwrap()])
            .env("TMUX_PANE", pane)
            .env("TMA_NOTIFY_FROM_EVENT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else {
            return false;
        };
        if child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .is_err()
        {
            return false;
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    };
    if !run() {
        // Back off before the retry: a first failure here is fork pressure, not a real error.
        std::thread::sleep(Duration::from_millis(100));
        assert!(run(), "tma event failed twice (transient fork pressure?)");
    }
}

/// Spawn a foreground daemon (status file + test manifest dir); `extra` adds flags such as
/// `--sweep-ms`. Suite-specific CLI shape over the shared [`Scratch::command`]; reaped on drop.
/// stderr goes to the scratch's daemon log, not `/dev/null`, so a timing failure's
/// [`Scratch::forensics`] block can quote what the daemon said it was doing.
fn spawn_daemon(s: &Scratch, extra: &[&str]) -> DaemonGuard {
    let status = s.status_path();
    let child = s
        .command()
        .args(["daemon", "--socket-name", &s.socket])
        .args(["--manifest-dir", s.workdir.to_str().unwrap()])
        .args(["--status-file", status.to_str().unwrap()])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(s.daemon_log_stdio())
        .spawn()
        .expect("spawn daemon");
    DaemonGuard::new(child)
}

/// Wait until `key` (parsed as u64) is `>= want`, returning the observed value. Waits out the
/// shared [`common::POLL_CEILING`], so a loaded box cannot turn a slow counter into a failure.
fn wait_status_ge(s: &Scratch, key: &str, want: u64) -> u64 {
    let deadline = Instant::now() + common::POLL_CEILING;
    loop {
        let got = s.status_u64(key);
        if got >= want || Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_opt(s: &Scratch, pane: &str, key: &str, want: &str) -> bool {
    let deadline = Instant::now() + common::POLL_CEILING;
    loop {
        if s.get(pane, &format!("#{{{key}}}")) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// --- 1. Hookless quiet edge: one capture, correct (blocked) classification, not a fan-out. ---

#[test]
fn hookless_quiet_edge_captures_once_and_classifies_blocked() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20");
    let (pane, pid) = new_shell_session(&s, "s1");
    let names = process_names_toml(&s, "s1", pid);
    // Hookless manifest (no `[hooks]`): a blocked rule keyed on a unique marker the burst echoes.
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n",
        ),
    );

    // Long default sweep (45 s) ⇒ no reconciliation sweep during this short test, so any
    // capture here is provably the on-demand path, not a fan-out.
    let _daemon = spawn_daemon(&s, PINNED_SWEEP);
    s.expect_status("clients", "1");

    // Let attach-time shell output settle to a quiet baseline (drains any attach-noise edge).
    wait_quiescent(&s);
    let baseline = s.status_u64("on_demand_captures");

    // One output burst that leaves the blocked marker on screen ⇒ one active→quiet edge.
    burst(&s, &pane, "echo tma-block-marker");

    assert!(
        wait_opt(&s, &pane, "@agent_state", "blocked"),
        "the quiet edge must capture and classify the hookless pane as blocked{}",
        s.forensics(&[&pane])
    );
    // The pane stamp lands before the counter does: wait for the capture to be counted, then for
    // the pool to fall quiet again, so a (wrongly) fanned-out second capture has run and counted too.
    wait_captures_ge(&s, baseline + 1);
    wait_quiescent(&s);
    let captured = s.status_u64("on_demand_captures") - baseline;
    assert_eq!(
        captured, 1,
        "exactly ONE on-demand capture (not a fan-out), got {captured}"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "capture");
    // The reconciliation sweep never ran (45 s cadence): the fan-out path is off the hot path.
    assert_eq!(
        s.status_u64("sweeps"),
        0,
        "no sweep during the on-demand test"
    );
}

// --- 2. Contradiction: a hook `working` stamp held quiet ⇒ one corrective capture flips it. ---

#[test]
fn contradiction_capture_flips_stale_hook_working() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20");
    let (pane, pid) = new_shell_session(&s, "s1");
    let names = process_names_toml(&s, "s1", pid);
    let comm = comm_of(pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n",
        ),
    );

    // Simulate a hook that stamped `working` a while ago (missing `blocked` coverage). `@agent_session`
    // marks the pane hook-capable; `@agent_pid` is 0 so no episode-boundary reset fires.
    let old = now_secs() - 1000;
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_session", "sess-1");
    s.set_opt(&pane, "@agent_evidence_at", &old.to_string());
    s.set_opt(&pane, "@agent_since", &old.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &old.to_string());

    let _daemon = spawn_daemon(&s, PINNED_SWEEP);
    s.expect_status("clients", "1");
    wait_quiescent(&s);
    let baseline = s.status_u64("on_demand_captures");

    // The pane hits a permission prompt (output, then quiet). The quiet edge is a contradiction (hook
    // working, chrome blocked): one corrective capture flips it (blocker chrome postdates the hook).
    burst(&s, &pane, "echo tma-block-marker");

    assert!(
        wait_opt(&s, &pane, "@agent_state", "blocked"),
        "the contradiction capture must correct the stale hook `working` to `blocked`{}",
        s.forensics(&[&pane])
    );
    assert_eq!(
        s.get(&pane, "#{@agent_source}"),
        "capture",
        "the corrective state is capture-sourced, not the stale hook"
    );
    // As above: count the capture, then wait out the pool so any extra corrective capture is in.
    wait_captures_ge(&s, baseline + 1);
    wait_quiescent(&s);
    let captured = s.status_u64("on_demand_captures") - baseline;
    assert_eq!(
        captured, 1,
        "exactly ONE corrective capture, got {captured}"
    );
    assert!(
        s.status_u64("contradiction_captures") >= 1,
        "the capture was counted as a contradiction trigger"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Reconciliation sweep: the ONLY multi-capture fan-out — discovers never-announced agents,
//    clears a silently-killed one, and its wall time is measured.
// ---------------------------------------------------------------------------------------------

#[test]
fn reconciliation_sweep_fans_out_and_clears_dead() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20");
    // Two never-announced agents on IDLE `sleep` panes: past the one-time look each session gets
    // when its control client attaches, they emit no activity edges, so the on-demand counter
    // stands still and every later capture is provably the sweep fan-out.
    let (_pane1, pid1) = new_idle_session(&s, "s1");
    let (_pane2, pid2) = new_idle_session(&s, "s2");
    let names = process_names_toml(&s, "s1", pid1);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n",
        ),
    );

    // `sweep_captures` reports only the LAST sweep, and freshness_secs = 0 keeps every sweep a full
    // producer pass (zero is an explicit re-read, so neither the fresh-stamp consume nor the
    // quiet-pane skip applies). Without it the fan-out count is a single-sweep transient the poll
    // below has to catch. The config lives in a subdir: every `*.toml` in the manifest dir is a manifest.
    let conf_dir = s.workdir.join("conf");
    std::fs::create_dir_all(&conf_dir).unwrap();
    let conf = conf_dir.join("config.toml");
    std::fs::write(&conf, "[fold]\nfreshness_secs = 0\n").unwrap();

    // Fast sweep so the acceptance runs quickly (the interval is the only cadence knob; the
    // on-demand path is unaffected).
    let _daemon = spawn_daemon(
        &s,
        &["--sweep-ms", "800", "--config", conf.to_str().unwrap()],
    );
    s.expect_status("clients", "2");
    // Drain the attach look (one edge per pane of a newly covered session) so what follows
    // measures the sweep alone.
    wait_quiescent(&s);
    let on_demand = s.status_u64("on_demand_captures");

    // Every sweep captures both never-announced agents: the N-capture fan-out, the only place it
    // occurs.
    s.expect_status("sweep_captures", "2"); // the fan-out captures once per agent (= agent count)
    let wall_ms = s.status_u64("last_sweep_wall_ms");
    eprintln!("reconciliation sweep wall time: {wall_ms} ms (2 agents)");
    assert_eq!(
        s.status_u64("on_demand_captures"),
        on_demand,
        "no repeating fan-out on the on-demand path (idle panes emit no further edges): \
         every capture the sweeps add is the sweep's"
    );
    // Discovery: both never-announced panes now carry a state stamp.
    assert!(!s
        .get(&s.get("s1", "#{pane_id}"), "#{@agent_state}")
        .is_empty());
    assert!(!s
        .get(&s.get("s2", "#{pane_id}"), "#{@agent_state}")
        .is_empty());
    s.expect_status("sweep_agents", "2");

    // A silent death: kill the second agent's process with SIGKILL (no SessionEnd). The next
    // sweep re-enumerates and clears it — the agent count drops.
    let victim = rustix::process::Pid::from_raw(pid2 as i32).expect("pid2 > 0");
    assert!(
        rustix::process::kill_process(victim, rustix::process::Signal::KILL).is_ok(),
        "SIGKILL {pid2}"
    );
    // The sweep clears the silently-killed agent, no SessionEnd.
    s.expect_status("sweep_agents", "1");
}

// ---------------------------------------------------------------------------------------------
// 4. Hook-liveness demotion: N stale-hook edges (default 5) ⇒ capture verdicts write UNGUARDED
//    (overriding the stale hook stamp); a hook event resumes guarded behaviour on the next capture.
//    The test first stamps `working` via a real hook, then ages that stamp past the decay window so
//    the subsequent edges legitimately count (a fresh-hook edge would not — that mitigation is
//    pinned by the `hook_fresh_edges_do_not_count_toward_demotion` unit test).
// ---------------------------------------------------------------------------------------------

#[test]
fn hook_liveness_demotion_unguards_then_hook_resumes() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20");
    let (pane, pid) = new_shell_session(&s, "s1");
    let names = process_names_toml(&s, "s1", pid);
    // Hook-capable manifest (`[hooks].covers` + a map). The `idle` screen rule keys on a per-burst
    // marker: the non-hook evidence the demoting capture publishes, flipping `@agent_source` off `hook`.
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [hooks]\ncovers = [\"working\", \"idle\", \"blocked\", \"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"UserPromptSubmit\"\nclaim = {{ state = \"working\" }}\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"idle\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-idle-marker\" }}\n",
        ),
    );

    let _daemon = spawn_daemon(&s, PINNED_SWEEP); // long sweep ⇒ no sweep interference
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // A live hook stamps `working` (source=hook) and, via the daemon's socket handler, RESETS
    // this pane's demotion counter to a clean zero.
    fire(
        &s,
        "agent",
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
    );
    assert!(
        wait_opt(&s, &pane, "@agent_state", "working"),
        "hook stamps working{}",
        s.forensics(&[&pane])
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    let demotions0 = s.status_u64("demotions");
    // Clean slate: drain any pending (attach-noise) edges so the reset counter starts controlled.
    wait_quiescent(&s);

    // Age the hook claim past `hook_decay_secs` and settle it to `idle`, without moving the
    // just-reset edge counter: the genuine demotion scenario (the last hook fired a while ago, then
    // the wiring went silent while output flows). `idle`, not `working`, so the per-edge activity
    // delta does not corroborate the claim and refresh it. `@agent_source` stays `hook`.
    let stale = now_secs() - 1000;
    s.set_opt(&pane, "@agent_state", "idle");
    s.set_opt(&pane, "@agent_evidence_at", &stale.to_string());
    s.set_opt(&pane, "@agent_since", &stale.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &stale.to_string());

    // Feed activity edges with zero intervening hook events (one per burst). An `idle` hook claim is
    // not a `working` contradiction, so the daemon writes nothing while the pane is trusted, only
    // accruing the counter; once DEMOTE_EDGES accrue, the demoting capture runs UNGUARDED and flips
    // `@agent_source` off `hook`. Counter-agnostic: assert the guarded→unguarded property, not N.
    let mut edges = 0;
    loop {
        edges += 1;
        assert!(
            edges <= 9,
            "demotion should occur within a few stale-hook edges"
        );
        burst(&s, &pane, "echo tma-idle-marker");
        // One edge per burst: wait past the quiet threshold so the edge is drained + folded before
        // the next burst (trusted idle-hook edges write nothing, so spacing is time-based).
        wait_quiescent(&s);
        if s.status_u64("demotions") > demotions0 {
            assert!(
                edges >= 2,
                "demotion is not instant — it takes multiple edges"
            );
            s.expect_status("demoted", "1");
            assert!(
                wait_opt_not(&s, &pane, "@agent_source", "hook"),
                "demoted ⇒ the capture verdict writes UNGUARDED, overriding the hook \
                 `@agent_source` the source guard would otherwise protect"
            );
            break;
        }
        assert_eq!(
            s.get(&pane, "#{@agent_source}"),
            "hook",
            "edge {edges}: still guarded ⇒ the hook stamp is protected"
        );
    }

    // Confirm the pane is genuinely demoted (unguarded) right before the resume, so the re-guard
    // transition below is a tight before/after boundary, not a coincidence.
    assert_ne!(
        s.get(&pane, "#{@agent_source}"),
        "hook",
        "pre-resume: the demoted pane's last capture wrote UNGUARDED (source off hook)"
    );

    // Drain any trailing edges so the resume hook is not raced by a pending demoted capture.
    wait_quiescent(&s);

    // A hook event resumes: it resets the counter and re-stamps source=hook. The serve-loop ordering
    // (edges folded before frames accepted, demotion cleared before the next drain) re-guards the next capture.
    fire(
        &s,
        "agent",
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
    );
    assert!(
        wait_opt(&s, &pane, "@agent_source", "hook"),
        "the resumed hook re-stamps source=hook{}",
        s.forensics(&[&pane])
    );
    s.expect_status("demoted", "0"); // a hook event clears the demotion
                                     // The next edge after the resume is guarded again (the capture cannot override the fresh hook
                                     // stamp) and must not re-demote off this single fresh-hook edge (the mitigation).
    let captures = s.status_u64("on_demand_captures");
    burst(&s, &pane, "echo tma-edge-after");
    wait_captures_ge(&s, captures + 1);
    assert_eq!(
        s.get(&pane, "#{@agent_source}"),
        "hook",
        "guarded behaviour resumed on the very next capture after the hook event"
    );
    assert_eq!(
        s.status_u64("demoted"),
        0,
        "a fresh-hook edge does not re-demote the re-guarded pane"
    );
}

// ---------------------------------------------------------------------------------------------
// 4b. The working-claim carve-out (issue #10): the same stale-hook setup as test 4, except the
//     hook's last claim is `working` and the stored state still agrees. Those edges are a working
//     agent's own output, so they must NOT demote however many arrive, and `@agent_source` must
//     stay `hook` (the option pair `tma doctor` reads for its `hook_demoted` warning).
// ---------------------------------------------------------------------------------------------

/// Comfortably past the default `demote_edges` of 5, which is what the old rule would have taken.
const WORKING_CARVE_OUT_EDGES: usize = 7;

#[test]
fn a_stale_working_hook_claim_survives_a_long_tool_call() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20b");
    let (pane, pid) = new_shell_session(&s, "s1");
    let names = process_names_toml(&s, "s1", pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [hooks]\ncovers = [\"working\", \"idle\", \"blocked\", \"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"UserPromptSubmit\"\nclaim = {{ state = \"working\" }}\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"idle\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-idle-marker\" }}\n",
        ),
    );

    let _daemon = spawn_daemon(&s, PINNED_SWEEP);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    fire(
        &s,
        "agent",
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
    );
    assert!(
        wait_opt(&s, &pane, "@agent_state", "working"),
        "hook stamps working{}",
        s.forensics(&[&pane])
    );
    let demotions0 = s.status_u64("demotions");
    wait_quiescent(&s);

    // Age the claim past `hook_decay_secs` WITHOUT settling it: this is minute two of one tool
    // call, so the hooks' last word is still `working` and so is the stored state.
    let stale = now_secs() - 1000;
    s.set_opt(&pane, "@agent_evidence_at", &stale.to_string());
    s.set_opt(&pane, "@agent_since", &stale.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &stale.to_string());

    // Feed well past `demote_edges` output bursts with no hook in between. None matches the `idle`
    // rule, so every capture holds and the counter is the only thing that could move. The
    // contradiction capture each edge triggers is what makes the edge count observable.
    let mut captures = s.status_u64("on_demand_captures");
    for _ in 0..WORKING_CARVE_OUT_EDGES {
        burst(&s, &pane, "echo tma-work-marker");
        captures = wait_captures_ge(&s, captures + 1);
    }
    wait_quiescent(&s);

    assert_eq!(
        s.status_u64("demotions"),
        demotions0,
        "a working agent's own output must not demote it{}",
        s.forensics(&[&pane])
    );
    s.expect_status("demoted", "0");
    assert_eq!(
        s.get(&pane, "#{@agent_source}"),
        "hook",
        "the hook stamp is still guarded, so doctor reports hook_demoted: false"
    );
}

// --- 5. skip_state_update freeze: a capture of a history-overlay pane must NOT read `blocked`. ---

/// Generic mechanism (a synthetic `skip_state_update` rule): a screen with both a blocked marker and
/// a history-view marker must freeze on the prior state, never publishing `blocked`.
#[test]
fn skip_state_update_freezes_and_does_not_read_blocked() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t20");
    let (pane, pid) = new_shell_session(&s, "s1");
    let names = process_names_toml(&s, "s1", pid);
    let comm = comm_of(pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n\
         [[rules]]\nstate = \"idle\"\npriority = 90\nskip_state_update = true\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-history-marker\" }}\n",
        ),
    );

    // Prior state: idle (the pane was idle before the user opened a history overlay).
    let old = now_secs() - 1000;
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "idle");
    s.set_opt(&pane, "@agent_source", "capture");
    s.set_opt(&pane, "@agent_evidence_at", &old.to_string());
    s.set_opt(&pane, "@agent_since", &old.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &old.to_string());

    let _daemon = spawn_daemon(&s, PINNED_SWEEP);
    s.expect_status("clients", "1");
    wait_quiescent(&s);
    let baseline = s.status_u64("on_demand_captures");

    // Both the blocked marker and the history-view marker are on screen. The `skip_state_update`
    // match raises `history_view`, so the fold freezes on the prior state and never reads `blocked`.
    burst(&s, &pane, "echo tma-block-marker tma-history-marker");

    // Wait for the on-demand capture to run (the edge fired, one capture).
    assert_eq!(
        wait_status_ge(&s, "on_demand_captures", baseline + 1),
        baseline + 1,
    );
    // Wait out the pool rather than a fixed margin: a (wrongly) published `blocked` verdict has
    // been written by the time the daemon is quiet again.
    wait_quiescent(&s);
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "idle",
        "a history-overlay capture must FREEZE on the prior state, never read blocked"
    );
    assert_ne!(s.get(&pane, "#{@agent_state}"), "blocked");
}

/// The Claude transcript-viewer acceptance: real `tma debug capture`s of a Claude pane with the
/// detailed-transcript overlay open (`claude_transcript_w{100,60}.txt`, redacted) replayed through
/// the bundled Claude engine + fold (deterministic, no daemon), asserting the overlay freezes on the
/// prior state and never reads blocked.
#[test]
fn claude_transcript_viewer_capture_is_not_blocked() {
    use tma_core::snapshot::PaneSnapshot;
    use tma_core::stamp::StampedState;
    use tma_core::{
        verdict, AgentState, FoldConfig, Manifest, Provenance, RuleEngine, SnapshotFacts,
    };

    let manifest = Manifest::parse(
        include_str!("../../tma-core/manifests/claude.toml"),
        "claude.toml",
    )
    .expect("bundled claude manifest parses");
    let engine = RuleEngine::build(&manifest).expect("claude manifest regexes compile");

    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tma-core/fixtures");

    for name in ["claude_transcript_w100.txt", "claude_transcript_w60.txt"] {
        let raw = std::fs::read_to_string(fixtures.join(name))
            .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
        // Minimal fixture parse (the tma-core `fixtures` feature is not enabled for this
        // crate): the header is `# key: value` lines, then a `---` separator, then the body.
        let (header, body) = raw
            .split_once("\n---\n")
            .unwrap_or_else(|| panic!("{name}: missing --- separator"));
        let title = header
            .lines()
            .find_map(|l| l.strip_prefix("# title: "))
            .unwrap_or("")
            .to_string();

        let snap = PaneSnapshot {
            pane_id: "%0".to_string(),
            pid_tree: vec![],
            title,
            tail_text: body.to_string(),
            tail_hash: 0,
            alternate_on: true,
            scroll_position: None,
            visible_height: None,
            captured_at: 1_000_000,
        };
        let ev = engine.evaluate(&snap);
        assert!(
            ev.history_view,
            "{name}: the transcript overlay must raise history_view (freeze)"
        );

        // Under the overlay the fold freezes the prior state: a pane idle before the transcript
        // opened stays idle; the scrolled-back history must never be re-read as blocked.
        let prior = StampedState {
            state: AgentState::Idle,
            detail: None,
            source: Provenance::Hook,
            evidence_at: 999_000,
            since: 999_000,
            turn_at: 0,
            stamped_at: 999_000,
            attention: false,
            notified_at: None,
            hash: None,
            pid: 1,
            session: None,
            subagents: vec![],
        };
        let facts = SnapshotFacts {
            pid: 1,
            foreground_is_agent: true,
            scrolled: false,
            history_view: ev.history_view,
        };
        let v = verdict(
            Some(prior),
            &facts,
            &ev.evidence,
            &manifest,
            &FoldConfig::default(),
            snap.captured_at + 10,
        );
        assert_ne!(
            v.state,
            AgentState::Blocked,
            "{name}: transcript overlay must never read blocked"
        );
        assert_eq!(
            v.state,
            AgentState::Idle,
            "{name}: overlay freezes the prior idle stamp (held), never restated from history"
        );
    }
}

// --- 6. Attach window: output printed before the control client attached still gets captured. ---

/// Seconds the shim below stalls the pool's control-mode attach. Only has to outlast the burst, so
/// the pane is provably silent by the time `%output` delivery starts; the daemon's quiet threshold
/// is 1 s, so 3 leaves no ambiguity without padding the suite.
const SLOW_ATTACH_SECS: u32 = 3;

/// The absolute `tmux` the shim execs. `command -v` rather than a fixed path: the suites run on
/// whatever tmux the box provides (homebrew, nix, distro).
fn real_tmux_path() -> String {
    let out = Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .expect("command -v tmux");
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!p.is_empty(), "tmux not on PATH");
    p
}

/// Seconds the shim stalls the seed's `list-panes` when [`slow_attach_tmux`] is asked to. Must
/// exceed the daemon's own 3 s `TMUX_TIMEOUT` so the read comes back as `TmuxError::Timeout` — the
/// exact outcome a CPU-saturated runner produces, where process spawn alone measures p50 3.8 s.
const STALL_SEED_READ_SECS: u32 = 5;

/// Write a `tmux` shim that stalls every control-mode attach AFTER the first, and return its path.
/// The daemon's push probe attaches first and must stay fast (a probe that misses its marker
/// degrades the daemon to the 5 s sweep, and the sweep — not the quiet edge — would then be what
/// rescues the pane, which is the opposite of what this test asserts). The pool's attach is the
/// second, and stalling it is what opens the pre-attach window.
///
/// `stall_seed_read` additionally stalls ONE `list-panes` past the daemon's `TMUX_TIMEOUT`: the
/// first one issued after that attach lands, which is `seed_attached`'s read (the serve loop calls
/// it before the summary reconcile, and the sweep is pinned out of the way). That single failed
/// read is the whole slow-runner failure, made deterministic.
fn slow_attach_tmux(s: &Scratch, stall_seed_read: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let shim = s.workdir.join("slow-attach-tmux");
    let count = s.workdir.join("attach-count");
    let attached = s.workdir.join("attach-landed");
    let stalled = s.workdir.join("seed-read-stalled");
    // The attach arm drops `attach-landed` immediately before exec'ing the real client, so the
    // list-panes arm can tell "before the attach" (every startup read) from "after" (the seed).
    let seed_arm = if stall_seed_read {
        format!(
            "*\" list-panes \"*)\n\
             [ -f '{attached}' ] && [ ! -f '{stalled}' ] && \
             {{ : > '{stalled}'; sleep {secs}; }} ;;\n",
            attached = attached.display(),
            stalled = stalled.display(),
            secs = STALL_SEED_READ_SECS,
        )
    } else {
        String::new()
    };
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *\" -C attach-session \"*)\n\
             n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n+1)); echo \"$n\" > '{count}'\n\
             [ \"$n\" -gt 1 ] && {{ sleep {secs}; : > '{attached}'; }} ;;\n\
             {seed_arm}\
             esac\n\
             exec '{real}' \"$@\"\n",
            count = count.display(),
            attached = attached.display(),
            secs = SLOW_ATTACH_SECS,
            real = real_tmux_path(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

/// Spawn the slow-attach daemon behind `shim`, burst the blocked marker into the pre-attach window,
/// and assert the pane still ends up captured as `blocked` by the ON-DEMAND tier. Shared by the two
/// attach-window tests, which differ only in whether the seed's own `list-panes` read succeeds.
fn assert_attach_window_pane_is_captured(s: &Scratch, pane: &str, shim: &PathBuf) {
    let status = s.status_path();
    let child = s
        .command()
        .args(["daemon", "--socket-name", &s.socket])
        .args(["--manifest-dir", s.workdir.to_str().unwrap()])
        .args(["--status-file", status.to_str().unwrap()])
        .args(PINNED_SWEEP)
        .env("TMA_TMUX_BIN", shim)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(s.daemon_log_stdio())
        .spawn()
        .expect("spawn daemon");
    let _daemon = DaemonGuard::new(child);

    // `clients=1` lands as soon as the `tmux -C` child is SPAWNED — the shim is still sleeping, so
    // the client has not attached and the session has no `%output` coverage yet. That gap is the
    // whole test: burst into it.
    s.expect_status("clients", "1");
    burst(s, pane, "echo tma-block-marker");

    assert!(
        wait_opt(s, pane, "@agent_state", "blocked"),
        "a pane that printed during the attach window must still be captured{}",
        s.forensics(&[pane])
    );
    assert_eq!(s.get(pane, "#{@agent_source}"), "capture");
    // The sweep is pinned 13x past the poll ceiling (`PINNED_SWEEP`), so this is not a coincidence
    // of two equal constants: the capture above can only have come from the on-demand tier.
    assert_eq!(
        s.status_u64("sweeps"),
        0,
        "the post-attach look must come from the on-demand tier, not a reconciliation sweep{}",
        s.forensics(&[pane])
    );
}

/// The manifest both attach-window tests use: one blocked rule keyed on the marker the burst echoes.
fn write_block_marker_manifest(s: &Scratch, names: &str) {
    write_manifest(
        s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n",
        ),
    );
}

/// A pane that printed its blocked prompt while the daemon's control client was still attaching is
/// still captured. tmux streams `%output` from the attach onward and never replays what came
/// before, so the burst here produces no activity mark at all — and a blocked prompt is the absence
/// of further output, so no later edge would arrive either. The daemon must therefore look at the
/// panes of a session the moment its client's coverage actually starts. Regression: the pool
/// counted a client as membership at spawn, so this pane sat unstamped until a sweep (which, being
/// a full cadence out, is past every caller's patience).
#[test]
fn slow_control_attach_still_captures_the_blocked_pane() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t26");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_block_marker_manifest(&s, &process_names_toml(&s, "s1", pid));
    let shim = slow_attach_tmux(&s, false);
    assert_attach_window_pane_is_captured(&s, &pane, &shim);
}

/// The same attach window, with the seed's OWN `list-panes` read failing once. That read is a `tmux`
/// one-shot under the daemon's 3 s `TMUX_TIMEOUT`, and on a CPU-saturated 3-core CI runner — where
/// process spawn alone measures p50 3.8 s — it times out routinely. The seed must survive it: the
/// post-attach look is the only thing covering a pane that printed before coverage began, so a
/// dropped seed spends a whole sweep cadence, which is exactly the latency the seed exists to avoid.
///
/// Regression: [`tma_tmux::control::seed_attached`] took its queue BEFORE the read, so one timed-out
/// `list-panes` discarded the look permanently. That is what turned this suite's attach-window test
/// into an intermittent CI failure with no evidence attached — it fails only when the read happens
/// to be the one that times out.
#[test]
fn a_seed_whose_list_panes_times_out_is_retried_not_dropped() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t27");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_block_marker_manifest(&s, &process_names_toml(&s, "s1", pid));
    let shim = slow_attach_tmux(&s, true);
    assert_attach_window_pane_is_captured(&s, &pane, &shim);
    // The read really did fail: without this the test could pass because the stall never landed on
    // the seed's read, and would then assert nothing about the retry.
    assert!(
        s.status_u64("seed_retries") >= 1,
        "the shim must have made the seed's list-panes time out{}",
        s.forensics(&[&pane])
    );
}

// --- 7. Agent exit: the quit pane is cleared by its next quiet edge, not by the sweep. ---

/// The pane shell's child process (`pid`, comm basename), `None` once it exits. The same `ps` walk
/// identity runs, rather than `#{pane_current_command}`, which reports the multi-call binary under
/// uutils.
fn child_of(pane_pid: u32) -> Option<(u32, String)> {
    ps_all()
        .expect("ps")
        .into_iter()
        .find(|p| p.ppid == pane_pid)
        .map(|p| (p.pid, basename(&p.comm)))
}

/// A user quits their agent and the pane falls back to its shell. The daemon's per-edge look used
/// to identify the pane, find no agent, and leave removal to the sweep, so a status line rendering
/// `#{@agent_summary}` kept showing `zsh* idle:1` for a whole sweep cadence (36 s measured against
/// the default 45 s one). The pane's own quiet edge is the daemon's first look after the exit, so
/// that is where the stamp comes down; the sweep here is pinned 13x past the poll ceiling, which
/// makes a pass the on-demand tier's alone.
#[test]
fn a_quit_agent_is_cleared_by_the_quiet_edge_not_the_sweep() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t28");
    let (pane, pane_pid) = new_shell_session(&s, "s1");

    // The agent is a foreground CHILD of the pane's shell, so killing it leaves the pane, its
    // shell, and the stale stamp standing, which is the shape the bug shows up in.
    burst(&s, &pane, "sleep 600");
    common::poll_until("the agent child to start", || child_of(pane_pid).is_some());
    let (agent_pid, _comm) = child_of(pane_pid).expect("the agent child");
    let names = process_names_toml(&s, "s1", agent_pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n",
        ),
    );

    let _daemon = spawn_daemon(&s, PINNED_SWEEP);
    s.expect_status("clients", "1");
    assert!(
        wait_opt_not(&s, &pane, "@agent_state", ""),
        "the daemon must stamp the live agent before its exit means anything{}",
        s.forensics(&[&pane])
    );

    let quit = Instant::now();
    let victim = rustix::process::Pid::from_raw(agent_pid as i32).expect("agent pid > 0");
    assert!(
        rustix::process::kill_process(victim, rustix::process::Signal::KILL).is_ok(),
        "SIGKILL {agent_pid}"
    );
    // The shell repaints its prompt over the dead agent: that output is the quiet edge.
    assert!(
        wait_opt(&s, &pane, "@agent_state", ""),
        "the quit agent's stamp must come down on the pane's next quiet edge{}",
        s.forensics(&[&pane])
    );
    eprintln!(
        "quit agent's stamp cleared {} ms after the exit",
        quit.elapsed().as_millis()
    );
    assert_eq!(
        s.get(&pane, "#{@agent_summary}"),
        "",
        "the window rollup comes down in the same invocation as the stamp"
    );
    assert_eq!(
        s.status_u64("sweeps"),
        0,
        "the clear must be the on-demand tier's: no sweep has run{}",
        s.forensics(&[&pane])
    );
}

// --- helpers --------------------------------------------------------------------------------

/// Poll `on_demand_captures` until it reaches `want`, returning the observed value.
fn wait_captures_ge(s: &Scratch, want: u64) -> u64 {
    wait_status_ge(s, "on_demand_captures", want)
}

/// Block until the daemon's control pool is quiescent (`active == 0` held past the quiet threshold),
/// so no pending edge or capture inflates a test's baseline. A condition-poll, not a blind sleep.
fn wait_quiescent(s: &Scratch) {
    assert!(
        common::wait_daemon_quiescent(
            &s.status_path(),
            Duration::from_millis(1200),
            common::POLL_CEILING,
        ),
        "daemon did not reach quiescence (attach-noise never drained)"
    );
}

/// Wait until `key` is present and NOT equal to `avoid`; returns whether that happened.
fn wait_opt_not(s: &Scratch, pane: &str, key: &str, avoid: &str) -> bool {
    let deadline = Instant::now() + common::POLL_CEILING;
    loop {
        let v = s.get(pane, &format!("#{{{key}}}"));
        if !v.is_empty() && v != avoid {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
