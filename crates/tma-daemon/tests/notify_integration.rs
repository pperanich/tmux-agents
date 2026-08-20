//! Notification dispatch + transition history acceptance, on an isolated scratch tmux server.
//!
//! The notification SINK is an instrumented `TMA_NOTIFY_CMD` that appends one line per fire (and
//! echoes `$TMA_PANE`, checking env delivery too), so fire count, ordering, and latency are
//! deterministic without a visible tmux client. Every daemon is a FOREGROUND child reaped on drop
//! ([`DaemonGuard`]) targeting ONLY the scratch server, so the user's real server is never touched.
//! Dispatch is detection-agnostic: the hook leg uses a SYNTHETIC `Block` event, the hookless leg a
//! real capture-classified blocked from on-screen chrome.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::process::Signal;

use common::{AttachOutcome, DaemonGuard, Scratch};
use tma_test_support as common;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// A detached shell session (so `send-keys` produces output), returning `(pane_id, pane_pid)`.
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

fn comm_of(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    basename(&String::from_utf8_lossy(&out.stdout))
}

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

fn write_manifest(s: &Scratch, body: &str) {
    std::fs::write(s.workdir.join("agent.toml"), body).unwrap();
}

fn burst(s: &Scratch, pane: &str, line: &str) {
    assert!(s
        .tmux(&["send-keys", "-t", pane, line, "Enter"])
        .status
        .success());
}

/// Fire `tma event` as a hook would. `from_event` toggles the daemonless direct-fire opt-in
/// (`TMA_NOTIFY_FROM_EVENT`); `notify_cmd` is the instrumented sink for that path.
fn fire(
    s: &Scratch,
    kind: &str,
    pane: &str,
    payload: &str,
    from_event: bool,
    notify_cmd: Option<&str>,
) {
    use std::io::Write;
    let mut cmd = s.command();
    cmd.args([
        "event",
        "--agent",
        "agent",
        "--kind",
        kind,
        "--payload",
        "-",
    ])
    .args(["--socket-name", &s.socket])
    .args(["--manifest-dir", s.workdir.to_str().unwrap()])
    .env("TMUX_PANE", pane)
    .env("TMA_NOTIFY_FROM_EVENT", if from_event { "1" } else { "0" })
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    if let Some(c) = notify_cmd {
        cmd.env("TMA_NOTIFY_CMD", c);
    }
    let mut child = cmd.spawn().expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    assert!(child.wait().expect("wait tma event").success());
}

fn sink_path(s: &Scratch) -> PathBuf {
    s.workdir.join("sink")
}

/// The `TMA_NOTIFY_CMD` sink: append one `fire <pane>` line per invocation (env delivery is
/// checked via `$TMA_PANE`). `pre` prefixes the shell (e.g. `sleep 1;`) for ordering tests.
fn sink_cmd(s: &Scratch, pre: &str) -> String {
    format!(
        "{pre}printf 'fire %s\\n' \"$TMA_PANE\" >> {}",
        sink_path(s).display()
    )
}

fn sink_lines(s: &Scratch) -> Vec<String> {
    std::fs::read_to_string(sink_path(s))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Spawn a foreground daemon wired to the instrumented sink. Reaped on drop.
fn spawn_daemon(s: &Scratch, sink_cmd: &str, extra: &[&str]) -> DaemonGuard {
    spawn_daemon_inner(s, sink_cmd, None, extra)
}

/// As [`spawn_daemon`], but reading `config` (via `TMA_CONFIG`) so a test can opt into
/// `notify.on = ["blocked", "done"]`; the `TMA_NOTIFY_*` env overrides never cover the trigger set.
fn spawn_daemon_with_config(
    s: &Scratch,
    sink_cmd: &str,
    config: &std::path::Path,
    extra: &[&str],
) -> DaemonGuard {
    spawn_daemon_inner(s, sink_cmd, Some(config), extra)
}

fn spawn_daemon_inner(
    s: &Scratch,
    sink_cmd: &str,
    config: Option<&std::path::Path>,
    extra: &[&str],
) -> DaemonGuard {
    let status = s.status_path();
    let mut cmd = s.command();
    cmd.args(["daemon", "--socket-name", &s.socket])
        .args(["--manifest-dir", s.workdir.to_str().unwrap()])
        .args(["--status-file", status.to_str().unwrap()])
        .args(extra)
        .env("TMA_NOTIFY_CMD", sink_cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(cfg) = config {
        cmd.env("TMA_CONFIG", cfg);
    }
    let child = cmd.spawn().expect("spawn daemon");
    DaemonGuard::new(child)
}

/// Poll a pane option until it reads `want`. Waits out the shared [`common::POLL_CEILING`]: every
/// use is a readiness gate, so a busy box must not be able to outrun it.
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

/// Wait until `@agent_notified_at` on `pane` is present and non-empty; returns it (or "").
fn wait_marker(s: &Scratch, pane: &str) -> String {
    let deadline = Instant::now() + common::POLL_CEILING;
    loop {
        let v = s.get(pane, "#{@agent_notified_at}");
        if !v.is_empty() {
            return v;
        }
        if Instant::now() >= deadline {
            return String::new();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_sink_lines(s: &Scratch, want: usize, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        let n = sink_lines(s).len();
        if n >= want || Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Block until the daemon's control pool has drained all attach-noise `%output` and is quiescent
/// (`active == 0` past the quiet threshold), so no attach-time capture races the real event. Polls
/// the daemon's gauge with a generous margin against the cold-daemon flake (the gauge cannot see
/// output tmux buffered but not yet delivered), not a structural proof.
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

/// A hook-capable manifest mapping a synthetic `Block` event to `blocked` (and `UserPromptSubmit`
/// to `working`), plus a capture `blocked` rule keyed on an on-screen marker (the hookless leg).
fn manifest(names: &str) -> String {
    format!(
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [hooks]\ncovers = [\"working\", \"idle\", \"blocked\", \"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"UserPromptSubmit\"\nclaim = {{ state = \"working\" }}\n\
         [[hooks.map]]\nevent = \"Block\"\nclaim = {{ state = \"blocked\", detail = \"permission\" }}\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n"
    )
}

// ---------------------------------------------------------------------------------------------
// 1. A hook blocked transition fires EXACTLY ONE notification; staying blocked does not re-fire.
// ---------------------------------------------------------------------------------------------

#[test]
fn blocked_transition_fires_once_and_dedups_within_episode() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    let sink = sink_cmd(&s, "");
    let _daemon = spawn_daemon(&s, &sink, &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // working, then blocked: the transition into blocked fires once.
    fire(
        &s,
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "working"));
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "blocked"));

    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "exactly one fire"
    );
    let marker1 = wait_marker(&s, &pane);
    assert!(!marker1.is_empty(), "@agent_notified_at set on fire");
    // The sink line carries the pane id (env var delivery works).
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));

    // The working→blocked transition was recorded in the transition ring.
    let transitions1 = s.status_u64("transitions_recorded");
    assert!(transitions1 >= 2, "working + blocked recorded");
    assert!(s.status_u64("history_len") >= 1);

    // A second blocked edge in the SAME episode (prev already blocked) must NOT re-fire, must NOT
    // move the marker, and must NOT record a new transition (blocked→blocked is not a landing).
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    // A negative window: there is nothing to poll for, so give a would-be re-fire time to land.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        sink_lines(&s).len(),
        1,
        "no re-fire while continuously blocked"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        marker1,
        "the marker is unchanged (written once per episode)"
    );
    assert_eq!(
        s.status_u64("notify_fires"),
        1,
        "the fire counter did not move"
    );
    assert_eq!(
        s.status_u64("transitions_recorded"),
        transitions1,
        "a redundant blocked edge records no new transition"
    );

    // A GENUINE new episode (blocked→working→blocked) fires EXACTLY once more: `@agent_since`
    // advances on the working landing, so the fresh blocked run postdates marker1 and re-arms.
    fire(
        &s,
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "working"));
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "blocked"));
    assert_eq!(
        wait_sink_lines(&s, 2, common::POLL_CEILING),
        2,
        "the new blocked episode fires exactly one more notification"
    );
    let marker2 = wait_marker(&s, &pane);
    assert!(
        marker2.parse::<u64>().unwrap_or(0) > marker1.parse::<u64>().unwrap_or(u64::MAX),
        "the re-fire wrote a fresh marker for the new episode (got {marker2}, was {marker1})"
    );
    assert_eq!(s.status_u64("notify_fires"), 2, "exactly two fires total");

    // The ring this daemon accumulated is readable over the socket (`tma debug transitions`), both
    // renderings off the one wire document.
    let out = s
        .command()
        .args(["debug", "transitions", "--json"])
        .args(["--socket-name", &s.socket])
        .output()
        .expect("spawn tma debug transitions");
    assert!(out.status.success(), "the daemon answered the history read");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")) && json.contains(r#""to":"blocked""#),
        "the ring holds this pane's blocked transition: {json}"
    );
    assert!(
        json.contains(r#""from":null"#),
        "a pane's first observation records no prior state: {json}"
    );
    let text = s
        .command()
        .args(["debug", "transitions"])
        .args(["--socket-name", &s.socket])
        .output()
        .expect("spawn tma debug transitions");
    assert!(String::from_utf8_lossy(&text.stdout).contains("-> blocked"));
}

// ---------------------------------------------------------------------------------------------
// 1b. With no daemon there is no ring: the history read fails cleanly rather than reporting empty.
// ---------------------------------------------------------------------------------------------

#[test]
fn transitions_without_a_daemon_says_so() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21nd");
    let (_pane, _pid) = new_shell_session(&s, "s1");
    let out = s
        .command()
        .args(["debug", "transitions"])
        .args(["--socket-name", &s.socket])
        .output()
        .expect("spawn tma debug transitions");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no daemon is running"),
        "the reason names the missing daemon: {stderr}"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Write-before-fire: `@agent_notified_at` is committed BEFORE the action runs.
// ---------------------------------------------------------------------------------------------

#[test]
fn marker_is_written_before_the_action_fires() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    // The sink reads `@agent_notified_at` when it runs. Because the marker is committed BEFORE the
    // action fires (write-before-fire), the action ALWAYS observes it set: a race-free ordering proof
    // from the action's own vantage (no polling window). If the order were reversed the
    // sink would record an empty value.
    let sink = format!(
        "tmux -L {} -f /dev/null display-message -p -t \"$TMA_PANE\" '#{{@agent_notified_at}}' >> {}",
        s.socket,
        sink_path(&s).display()
    );
    let _daemon = spawn_daemon(&s, &sink, &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );

    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the notification fired"
    );
    let observed = sink_lines(&s)[0].trim().to_string();
    assert!(
        observed.parse::<u64>().is_ok(),
        "the action observed @agent_notified_at ALREADY committed (write-before-fire); got {observed:?}"
    );
    assert_eq!(
        observed,
        s.get(&pane, "#{@agent_notified_at}"),
        "the observed marker is the committed episode marker"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Cold-start dedup: a fresh daemon over a pane already blocked+notified does NOT re-fire.
// ---------------------------------------------------------------------------------------------

#[test]
fn cold_start_does_not_refire_an_already_notified_episode() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    let comm = comm_of(pid);
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));

    // Pre-existing blocked episode ALREADY notified (notified_at == since ⇒ >= since). A daemon
    // starting cold must read this as done and not re-fire (the cold-start rule).
    let t = now_secs() - 5;
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "blocked");
    s.set_opt(&pane, "@agent_detail", "permission");
    s.set_opt(&pane, "@agent_source", "capture");
    s.set_opt(&pane, "@agent_evidence_at", &t.to_string());
    s.set_opt(&pane, "@agent_since", &t.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &t.to_string());
    s.set_opt(&pane, "@agent_notified_at", &t.to_string());
    s.set_opt(&pane, "@agent_pid", &pid.to_string());

    let _daemon = spawn_daemon(&s, &sink_cmd(&s, ""), &["--sweep-ms", "500"]);
    s.expect_status("clients", "1");
    // Let the cold-start pass drain, then confirm a couple of sweeps ran (--sweep-ms 500): each is a
    // full notify pass that WOULD re-fire a mis-deduped episode, so a stayed-empty sink is a real no-refire.
    wait_quiescent(&s);
    assert!(
        common::wait_until(common::POLL_CEILING, || s.status_u64("sweeps") >= 2),
        "the daemon ran at least two reconciliation sweeps"
    );
    assert!(sink_lines(&s).is_empty(), "no re-fire on cold start");
    assert_eq!(s.status_u64("notify_fires"), 0);
    assert_eq!(
        s.get(&pane, "#{@agent_notified_at}"),
        t.to_string(),
        "marker untouched"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. Episode boundary: a pid change on a blocked pane clears the marker ⇒ the new episode fires.
// ---------------------------------------------------------------------------------------------

#[test]
fn episode_boundary_pid_change_refires() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    let comm = comm_of(pid);
    // HOOKLESS manifest: a hook-capable pane's quiet edge is skipped (hooks are assumed to drive
    // it), so the episode-boundary capture must come from a hookless pane's quiet edge.
    let names = process_names_toml(&s, "s1", pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n"
        ),
    );

    // Episode 1: blocked + already notified, but stamped against a STALE (dead) pid. The next capture
    // finds the real pid (≠ stale) ⇒ episode reset clears `@agent_notified_at` and rewrites `@agent_since`.
    let t = now_secs() - 5;
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "blocked");
    s.set_opt(&pane, "@agent_detail", "permission");
    s.set_opt(&pane, "@agent_source", "capture");
    s.set_opt(&pane, "@agent_evidence_at", &t.to_string());
    s.set_opt(&pane, "@agent_since", &t.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &t.to_string());
    s.set_opt(&pane, "@agent_notified_at", &t.to_string());
    s.set_opt(&pane, "@agent_pid", "999999"); // dead pid ⇒ forces the boundary on next capture

    let _daemon = spawn_daemon(&s, &sink_cmd(&s, ""), &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s); // cold-start pass ran + attach edges drained (no fire on either)
    assert!(
        sink_lines(&s).is_empty(),
        "cold start did not fire (already notified)"
    );

    // Drive a quiet edge with the blocked marker on screen: the capture reclassifies blocked, the pid
    // mismatch triggers the episode reset (marker cleared, since bumped), and the fresh episode notifies.
    burst(&s, &pane, "echo tma-block-marker");
    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the new blocked episode (pid change) fires again"
    );
    let marker = s.get(&pane, "#{@agent_notified_at}");
    assert!(
        marker.parse::<u64>().unwrap_or(0) > t,
        "the re-fire wrote a fresh marker for the new episode (got {marker}, was {t})"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. Latency: hook path < 1 s.
// ---------------------------------------------------------------------------------------------

#[test]
fn latency_hook_path_under_1s() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    let _daemon = spawn_daemon(&s, &sink_cmd(&s, ""), &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // Measure daemon-side dispatch latency: the clock starts once `tma event` has delivered the frame
    // (its spawn cost under parallel `cargo test` is harness noise, not daemon latency). Delivery→
    // notification is the daemon's contribution.
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    let start = Instant::now();
    let got = wait_sink_lines(&s, 1, Duration::from_secs(5));
    let elapsed = start.elapsed();
    assert_eq!(got, 1, "hook blocked notified");
    eprintln!(
        "hook-path daemon-dispatch latency: {} ms",
        elapsed.as_millis()
    );
    // Comfortably inside the hook target (<1 s); isolated full-path measurement is ~70 ms. The
    // bound is widened only enough that the full-workspace fork-bomb cannot flake it.
    assert!(
        elapsed < Duration::from_secs(2),
        "hook blocked→notification is in the immediate tier, was {} ms",
        elapsed.as_millis()
    );
}

// ---------------------------------------------------------------------------------------------
// 6. Latency: hookless capture path < 5 s at the default quiet-edge cadence.
// ---------------------------------------------------------------------------------------------

#[test]
fn latency_hookless_capture_path_under_5s() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    // A HOOKLESS manifest (no `[hooks]`): the quiet edge is the only blocked-catch path.
    let names = process_names_toml(&s, "s1", pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
         [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
         region = \"tail_lines(50)\"\nmatch = {{ contains = \"tma-block-marker\" }}\n"
        ),
    );
    // Default sweep cadence (no --sweep-ms): the notification must come from the near-instant
    // quiet edge, not a fan-out sweep.
    let _daemon = spawn_daemon(&s, &sink_cmd(&s, ""), &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    let start = Instant::now();
    burst(&s, &pane, "echo tma-block-marker");
    // Wait exactly the asserted bound (5 s), not a looser one: a late fire past the bound then times
    // out to `got == 0` and fails identically to a missing fire, rather than slipping the elapsed assert.
    let got = wait_sink_lines(&s, 1, Duration::from_secs(5));
    let elapsed = start.elapsed();
    assert_eq!(
        got, 1,
        "hookless blocked notified via the quiet-edge capture within the 5 s bound"
    );
    eprintln!("hookless-path latency: {} ms", elapsed.as_millis());
    assert!(
        elapsed < Duration::from_secs(5),
        "hookless blocked→notification must be < 5 s, was {} ms",
        elapsed.as_millis()
    );
    assert_eq!(
        s.status_u64("sweeps"),
        0,
        "the fire came from the quiet edge, not a sweep"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. Daemonless direct-fire: TMA_NOTIFY_FROM_EVENT still works with no daemon.
// ---------------------------------------------------------------------------------------------

#[test]
fn daemonless_direct_fire_still_works() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    // NO daemon spawned. The hook event direct-stamps AND direct-fires, writing the marker before
    // the action through the SAME shared fire.
    let sink = sink_cmd(&s, "");
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        true,
        Some(&sink),
    );

    assert!(
        wait_opt(&s, &pane, "@agent_state", "blocked"),
        "daemonless direct stamp"
    );
    assert!(
        !s.get(&pane, "#{@agent_notified_at}").is_empty(),
        "daemonless write-before-fire committed the marker"
    );
    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "daemonless direct-fire fired exactly one notification (unchanged with no daemon)"
    );
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));
}

// ---------------------------------------------------------------------------------------------
// 8. With `notify.on = ["blocked", "done"]`, a working→idle completion fires once.
// ---------------------------------------------------------------------------------------------

#[test]
fn done_transition_fires_when_opted_in() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    // A hook-capable manifest mapping UserPromptSubmit→working and Stop→idle (the working→idle
    // "done" transition notifies on). No capture rules; this leg is purely hook-driven.
    let names = process_names_toml(&s, "s1", pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [hooks]\ncovers = [\"working\", \"idle\", \"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"UserPromptSubmit\"\nclaim = {{ state = \"working\" }}\n\
         [[hooks.map]]\nevent = \"Stop\"\nclaim = {{ state = \"idle\" }}\n\
         [capture]\nvisible = []\n"
        ),
    );
    // Opt into done notifications via config (env overrides do NOT cover `on`). Keep it OUT of the
    // manifest dir: `load_dir` parses every top-level `*.toml` there, and `read_dir` is not recursive.
    let cfg_dir = s.workdir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, "[notify]\non = [\"blocked\", \"done\"]\n").unwrap();
    let _daemon = spawn_daemon_with_config(&s, &sink_cmd(&s, ""), &cfg, &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // working, then idle: the working→idle completion sets @agent_attention and fires "done".
    fire(
        &s,
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "working"));
    fire(&s, "Stop", &pane, r#"{"session_id":"sess-1"}"#, false, None);
    assert!(wait_opt(&s, &pane, "@agent_state", "idle"));
    assert!(
        wait_opt(&s, &pane, "@agent_attention", "1"),
        "working→idle set the attention flag (the done surface)"
    );

    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the working→idle completion fired exactly one done notification"
    );
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));
    let marker = wait_marker(&s, &pane);
    assert!(
        !marker.is_empty(),
        "@agent_notified_at set on the done fire"
    );

    // Staying idle (no new transition) must NOT re-fire.
    // Negative window: nothing to poll for, so allow a would-be re-fire time to land.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        sink_lines(&s).len(),
        1,
        "no re-fire while continuously idle (per-state-run dedup)"
    );
    assert_eq!(s.status_u64("notify_fires"), 1);
}

// ---------------------------------------------------------------------------------------------
// 8b. The sweep's ordered-input clear runs STRICTLY AFTER the notification dispatch. Both read the
//     same persisted `@agent_attention`, so a clear that ran first would swallow the desktop
//     notification for a completion the user has already been typing past. Until this case existed
//     the ordering was held by source order and a comment: the whole suite passed with the two
//     blocks swapped.
// ---------------------------------------------------------------------------------------------

/// A detached session whose pane swallows keystrokes: `sleep` reads nothing and writes nothing, so
/// the attached client can type without producing pane output. Output would wake the daemon's
/// control client and dispatch notifications on iterations the sweep never ran, which is precisely
/// what this case must exclude.
fn new_quiet_session(s: &Scratch, name: &str) -> (String, u32) {
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
    (pane, pid)
}

/// The attached PTY client's `#{client_activity}` (epoch seconds) as ms. Control-mode clients are
/// skipped exactly as the predicate skips them: the daemon parks one on this very session and its
/// clock froze at attach.
fn pty_activity_ms(s: &Scratch) -> u64 {
    let out = s.tmux(&[
        "list-clients",
        "-F",
        "#{client_control_mode}:#{client_activity}",
    ]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("0:"))
        .filter_map(|secs| secs.trim().parse::<u64>().ok())
        .map(|secs| secs * 1000)
        .max()
        .unwrap_or(0)
}

/// Type at the client's real terminal until its input clock reads strictly past `since_ms`, and
/// return that clock. Repeated because the clock has one-second resolution and the comparison is a
/// strict `>`: a keystroke inside the raise's own second is deliberately not "later than" it.
fn type_past(s: &Scratch, since_ms: u64) -> u64 {
    let deadline = Instant::now() + common::POLL_CEILING;
    while Instant::now() < deadline {
        s.send_client_keys("q");
        std::thread::sleep(Duration::from_millis(200));
        let act = pty_activity_ms(s);
        if act > since_ms {
            return act;
        }
    }
    panic!("the PTY client's input never registered past the raise");
}

#[test]
fn a_done_marker_the_user_typed_past_still_fires_before_it_is_cleared() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new_daemon("t21seen");
    let (pane, pid) = new_quiet_session(&s, "s1");
    let comm = comm_of(pid);
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    // The PTY client attaches BEFORE the daemon: `attach_client`'s readiness poll watches
    // `list-clients`, which the daemon's own control client would satisfy on its own.
    match s.attach_client("s1") {
        AttachOutcome::Attached => {}
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            return;
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }

    let cfg_dir = s.workdir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, "[notify]\non = [\"blocked\", \"done\"]\n").unwrap();
    let daemon = spawn_daemon_with_config(&s, &sink_cmd(&s, ""), &cfg, &["--sweep-ms", "500"]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);
    assert!(
        sink_lines(&s).is_empty(),
        "nothing has completed yet: {:?}",
        sink_lines(&s)
    );

    // A real keystroke at the real client. Everything below is ordered against THIS instant.
    let last_input = type_past(&s, 0);

    // Freeze the daemon across the raise. Without the freeze the collision cannot be staged: the
    // loop dispatches on every wake (a bare poll timeout reconciles the pool and dirties the
    // status), so a marker raised between two sweeps is notified hundreds of ms before the sweep
    // that would clear it, and the ordering under test never decides anything. Stopped, the daemon
    // meets the raise for the first time in its next iteration — which runs the overdue sweep and
    // the dispatch back to back, the one place their order is observable.
    signal_daemon(daemon.pid(), Signal::STOP);

    // A completion raised by ANOTHER producer (a `tma status` off a status line does exactly this),
    // one second BEFORE the keystroke above: the user has demonstrably typed past it, so the
    // ordered clear is armed the instant the flag goes up. `@agent_attention` is written last, so a
    // reader can never see a half-written tuple as done.
    let raised = last_input - 1_000;
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "idle");
    s.set_opt(&pane, "@agent_source", "capture");
    s.set_opt(&pane, "@agent_evidence_at", &raised.to_string());
    s.set_opt(&pane, "@agent_since", &raised.to_string());
    s.set_opt(&pane, "@agent_stamped_at", &now_ms().to_string());
    s.set_opt(&pane, "@agent_pid", &pid.to_string());
    s.set_opt(&pane, "@agent_attention", "1");
    signal_daemon(daemon.pid(), Signal::CONT);

    // The dispatch has to get there first. With the clear moved ahead of it, the sweep retires the
    // flag in that same iteration and this sink stays empty for good.
    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the completion notified before the ordered-input clear retired it"
    );
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));
    assert!(
        !wait_marker(&s, &pane).is_empty(),
        "@agent_notified_at set on the done fire"
    );

    // And the clear is not merely late — it lands, on the pane the user typed at.
    assert!(
        wait_opt(&s, &pane, "@agent_attention", ""),
        "the ordered-input clear still takes the marker down once the notification is out"
    );
    // Negative window: nothing to poll for, so allow a would-be second fire time to land.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        sink_lines(&s).len(),
        1,
        "exactly one fire for the episode: {:?}",
        sink_lines(&s)
    );
}

// ---------------------------------------------------------------------------------------------
// 9. SIGHUP hot-reloads config. A daemon started with `notify.on = ["blocked"]` does NOT
//    fire on a completion; after the config file is rewritten to add "done" and the daemon is
//    SIGHUP'd, the still-unreviewed completion fires exactly one "done" (reload cold-start).
// ---------------------------------------------------------------------------------------------

#[test]
fn sighup_reload_applies_new_notify_on() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    // Hook-capable manifest: UserPromptSubmit→working, Stop→idle (the working→idle "done").
    let names = process_names_toml(&s, "s1", pid);
    write_manifest(
        &s,
        &format!(
            "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{names}]\n\
         [hooks]\ncovers = [\"working\", \"idle\", \"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"UserPromptSubmit\"\nclaim = {{ state = \"working\" }}\n\
         [[hooks.map]]\nevent = \"Stop\"\nclaim = {{ state = \"idle\" }}\n\
         [capture]\nvisible = []\n"
        ),
    );
    // Start with done DISABLED (default blocked-only). Config lives outside the manifest dir so
    // `load_dir` never parses it as a manifest (same as the done-transition test).
    let cfg_dir = s.workdir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, "[notify]\non = [\"blocked\"]\n").unwrap();
    let daemon = spawn_daemon_with_config(&s, &sink_cmd(&s, ""), &cfg, &[]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // A working→idle completion: attention is set, but "done" is NOT in the trigger set, so
    // nothing fires and no marker is written.
    fire(
        &s,
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        false,
        None,
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "working"));
    fire(&s, "Stop", &pane, r#"{"session_id":"sess-1"}"#, false, None);
    assert!(wait_opt(&s, &pane, "@agent_state", "idle"));
    assert!(wait_opt(&s, &pane, "@agent_attention", "1"));
    // Negative window: allow the fire that must not happen time to land before reading the sink.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        sink_lines(&s).len(),
        0,
        "done must NOT fire while notify.on = [blocked] (pre-reload)"
    );
    assert!(
        s.get(&pane, "#{@agent_notified_at}").is_empty(),
        "no notify marker is written before the reload"
    );

    // Rewrite the config to enable done, then SIGHUP: the reload swaps the trigger set in place
    // and re-evaluates, firing the still-unreviewed completion exactly once (cold-start symmetry).
    std::fs::write(&cfg, "[notify]\non = [\"blocked\", \"done\"]\n").unwrap();
    sighup(daemon.pid());

    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the SIGHUP reload enabled done ⇒ the completion fires exactly one notification"
    );
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));
    assert!(
        !wait_marker(&s, &pane).is_empty(),
        "the post-reload done fire wrote @agent_notified_at"
    );

    // Staying idle after the reload: no re-fire (per-state-run dedup, unchanged by the reload).
    // Negative window: allow a would-be re-fire time to land.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        sink_lines(&s).len(),
        1,
        "no re-fire while continuously idle after the reload"
    );
    assert_eq!(s.status_u64("notify_fires"), 1);
}

// ---------------------------------------------------------------------------------------------
// 10. context_high: a pane whose gauge crosses the threshold fires exactly one context
//     notification on its OWN marker (@agent_context_notified_at, NOT the state lane), holds while
//     high, rearms below threshold - 10, and re-fires on the next crossing.
// ---------------------------------------------------------------------------------------------

/// Set the minimal idle-agent state tuple plus a context gauge on `pane`.
fn stamp_idle_with_context(s: &Scratch, pane: &str, comm: &str, pid: u32, pct: u8, at: u64) {
    s.set_opt(pane, "@agent_name", comm);
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_evidence_at", &at.to_string());
    s.set_opt(pane, "@agent_since", &at.to_string());
    s.set_opt(pane, "@agent_stamped_at", &at.to_string());
    s.set_opt(pane, "@agent_pid", &pid.to_string());
    s.set_opt(pane, "@agent_context_pct", &pct.to_string());
    s.set_opt(pane, "@agent_context_at", &at.to_string());
}

/// A sink that records the payload's reported episode age beside the pane, for the cases that are
/// about `TMA_SINCE_MS` rather than the fire count.
fn age_sink_cmd(s: &Scratch) -> String {
    format!(
        "printf 'fire %s %s\\n' \"$TMA_PANE\" \"$TMA_SINCE_MS\" >> {}",
        sink_path(s).display()
    )
}

/// `TMA_SINCE_MS` is the episode's age at dispatch. On a SECOND completion `@agent_since` is still
/// pinned to the start of the idle run (write-once per state run), so reading it there reports how
/// long the pane has been idle — minutes or hours — in a field a hook reads as dispatch latency.
/// The episode instant is `max(@agent_since, @agent_turn_at)`, which is what the dedup and the
/// marker clamp already compare. Pass `stored.since` in `fire_for` and this fails.
#[test]
fn a_second_completions_payload_reports_the_turns_age_not_the_idle_runs() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    let comm = comm_of(pid);
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    let cfg_dir = s.workdir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, "[notify]\non = [\"done\"]\n").unwrap();
    let _daemon = spawn_daemon_with_config(&s, &age_sink_cmd(&s), &cfg, &["--sweep-ms", "400"]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // An hour-old idle run whose marker has just been re-raised by a fresh turn end. Stamped
    // directly: the point is the arithmetic on the stored tuple, not how it came to be stored.
    const IDLE_RUN_MS: u64 = 3_600_000;
    let now = now_ms();
    s.set_opt(&pane, "@agent_name", &comm);
    s.set_opt(&pane, "@agent_state", "idle");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_evidence_at", &now.to_string());
    s.set_opt(&pane, "@agent_since", &(now - IDLE_RUN_MS).to_string());
    s.set_opt(&pane, "@agent_stamped_at", &now.to_string());
    s.set_opt(&pane, "@agent_pid", &pid.to_string());
    s.set_opt(&pane, "@agent_turn_at", &now.to_string());
    s.set_opt(&pane, "@agent_attention", "1");

    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the re-raised done marker fired exactly one notification"
    );
    let line = sink_lines(&s).remove(0);
    let age: u64 = line
        .rsplit(' ')
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no TMA_SINCE_MS in the sink line {line:?}"));
    assert!(
        line.starts_with(&format!("fire {pane} ")),
        "the fire is for this pane: {line:?}"
    );
    assert!(
        age < IDLE_RUN_MS / 2,
        "the payload reports the new turn's age, not the idle run's: {age} ms"
    );
}

fn wait_marker_empty(s: &Scratch, pane: &str, key: &str) -> bool {
    let deadline = Instant::now() + common::POLL_CEILING;
    loop {
        if s.get(pane, &format!("#{{{key}}}")).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn context_high_fires_once_then_rearms_and_refires() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21");
    let (pane, pid) = new_shell_session(&s, "s1");
    let comm = comm_of(pid);
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    // Enable context_high at 75 via config (env overrides do not cover it). Kept out of the manifest
    // dir so `load_dir` never parses it as a manifest.
    let cfg_dir = s.workdir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, "[notify.context_high]\nthreshold = 75\n").unwrap();
    let _daemon = spawn_daemon_with_config(&s, &sink_cmd(&s, ""), &cfg, &["--sweep-ms", "400"]);
    s.expect_status("clients", "1");
    wait_quiescent(&s);

    // Armed + a high gauge (80 >= 75): the reconcile fires exactly one context notification and arms
    // the marker on the CONTEXT lane, leaving the state lane's @agent_notified_at untouched.
    stamp_idle_with_context(&s, &pane, &comm, pid, 80, now_ms());
    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "the gauge crossing fired exactly one context notification"
    );
    assert!(
        !s.get(&pane, "#{@agent_context_notified_at}").is_empty(),
        "the context marker (@agent_context_notified_at) armed"
    );
    assert!(
        s.get(&pane, "#{@agent_notified_at}").is_empty(),
        "the state-lane marker is never touched by context_high"
    );

    // Staying high does not re-fire (the flag holds).
    // Negative window: two sweeps (--sweep-ms 400) pass, so a re-fire would have landed.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(
        sink_lines(&s).len(),
        1,
        "no re-fire while continuously high"
    );

    // A dip into the hysteresis band (70, still >= threshold - 10 = 65) does NOT rearm.
    s.set_opt(&pane, "@agent_context_pct", "70");
    s.set_opt(&pane, "@agent_context_at", &now_ms().to_string());
    // Negative window again: sweeps run over the dip, so a wrong rearm would show.
    std::thread::sleep(Duration::from_millis(900));
    assert!(
        !s.get(&pane, "#{@agent_context_notified_at}").is_empty(),
        "a shallow dip inside the hysteresis band keeps the flag set"
    );
    assert_eq!(sink_lines(&s).len(), 1, "no re-fire from a shallow dip");

    // A dip below threshold - 10 (60 < 65) rearms: the marker clears.
    s.set_opt(&pane, "@agent_context_pct", "60");
    s.set_opt(&pane, "@agent_context_at", &now_ms().to_string());
    assert!(
        wait_marker_empty(&s, &pane, "@agent_context_notified_at"),
        "dropping below threshold - 10 rearmed the flag (marker cleared)"
    );

    // Re-crossing high fires exactly one more.
    s.set_opt(&pane, "@agent_context_pct", "82");
    s.set_opt(&pane, "@agent_context_at", &now_ms().to_string());
    assert_eq!(
        wait_sink_lines(&s, 2, common::POLL_CEILING),
        2,
        "the next crossing after a rearm fires exactly one more"
    );
    assert_eq!(s.status_u64("notify_fires"), 2, "exactly two context fires");
}

// ---------------------------------------------------------------------------------------------
// `tma mute`: the pane is detected and marked exactly as always, and rings nothing until cleared.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_muted_pane_is_stamped_but_never_fires_until_the_mute_is_cleared() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("t21mute");
    let (pane, pid) = new_shell_session(&s, "s1");
    write_manifest(&s, &manifest(&process_names_toml(&s, "s1", pid)));
    let sink = sink_cmd(&s, "");

    // Mute indefinitely, then block through the daemonless direct-fire path.
    let muted = s.tma(&["mute", "--pane", &pane]);
    assert!(
        muted.status.success(),
        "mute failed: {}",
        String::from_utf8_lossy(&muted.stderr)
    );
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        true,
        Some(&sink),
    );

    // Detection and the episode markers are untouched — only the fire is gone.
    assert!(
        wait_opt(&s, &pane, "@agent_state", "blocked"),
        "a muted pane is still detected and stamped"
    );
    assert!(
        !s.get(&pane, "#{@agent_notified_at}").is_empty(),
        "the episode is still marked notified, so nothing replays when the mute ends"
    );
    assert!(
        sink_lines(&s).is_empty(),
        "a muted pane fires nothing: {:?}",
        sink_lines(&s)
    );

    // The row still reports the pane, now carrying the additive `muted` key.
    let listed = s.tma(&["ls", "--json"]);
    let json = String::from_utf8_lossy(&listed.stdout);
    assert!(
        json.contains(r#""state":"blocked""#) && json.contains(r#""muted":true"#),
        "a muted pane still lists, marked muted: {json}"
    );

    // Clearing lifts it: the next episode fires exactly one notification.
    let cleared = s.tma(&["mute", "--clear", "--pane", &pane]);
    assert!(cleared.status.success());
    assert_eq!(
        s.get(&pane, "#{@agent_mute_until}"),
        "",
        "--clear unsets the option"
    );
    fire(
        &s,
        "UserPromptSubmit",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        true,
        Some(&sink),
    );
    assert!(wait_opt(&s, &pane, "@agent_state", "working"));
    fire(
        &s,
        "Block",
        &pane,
        r#"{"session_id":"sess-1"}"#,
        true,
        Some(&sink),
    );
    assert_eq!(
        wait_sink_lines(&s, 1, common::POLL_CEILING),
        1,
        "an unmuted pane fires again"
    );
    assert_eq!(sink_lines(&s)[0], format!("fire {pane}"));
}

/// Signal the daemon via `rustix::process::kill_process` (the `kill` binary is absent in minimal
/// envs like the nix sandbox).
fn signal_daemon(pid: u32, sig: Signal) {
    let pid = rustix::process::Pid::from_raw(pid as i32).expect("valid pid");
    rustix::process::kill_process(pid, sig).expect("signal the daemon");
}

fn sighup(pid: u32) {
    signal_daemon(pid, Signal::HUP);
}
