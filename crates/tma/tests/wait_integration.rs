//! `tma wait` acceptance on a scratch server. `wait` prints one row and exits (no PTY attach
//! needed), and its exit code is the contract, so every test pins `status.code()`: `0` observed,
//! `124` timeout, `3` a `--pane` vanish, `1` an ambiguous `--agent`. Static-chrome panes (`printf
//! '<chrome>'; exec sleep`) drive the deterministic cases; the transition test flips an interactive
//! shell pane idle → blocked mid-wait via `send-keys`. Scratch `tmux -L` server, killed on drop.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tma_test_support::{
    empty_config_path, wait_capture_contains, wait_status_eq, AttachOutcome, DaemonTestGuard,
    Scratch, POLL_CEILING,
};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Both process names a pane's agent could resolve to (`#{pane_current_command}` and the ps `comm`
/// of the pane pid), so identity works host-agnostically.
fn pane_process_names(s: &Scratch, target: &str) -> Vec<String> {
    let cc = basename(&s.display(target, "#{pane_current_command}"));
    let pid = s.display(target, "#{pane_pid}");
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pid])
            .output()
            .expect("ps")
            .stdout,
    ));
    let mut names = vec![cc, psc];
    names.sort();
    names.dedup();
    names
}

/// A shared manifest carrying an idle rule (`READY`) and a higher-priority blocked rule (the
/// permission-prompt chrome), matching `names` as the agent process. Its stem `agent.toml` makes
/// the agent name `agent`.
fn write_manifest(s: &Scratch, names: &[String]) {
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
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();
}

const BLOCKED_CHROME: &str = "\\n\\n\\n\\n\
    ╭──────────────────────────╮\\n\
    │ Do you want to proceed?  │\\n\
    │ ❯ 1. Yes                 │\\n\
    ╰──────────────────────────╯\\n";

/// Launch a static detached agent pane (`chrome`, then a long-lived `sleep`) and return its pane id.
/// The marker (`READY` or the prompt text) proves the chrome rendered and the shell reached `exec`.
fn static_agent(s: &Scratch, sess: &str, chrome: &str, marker: &str) -> String {
    let cmd = format!("printf '{chrome}'; exec sleep 100000");
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            sess,
            "-x",
            "100",
            "-y",
            "24",
            &cmd
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, sess, marker, POLL_CEILING),
        "agent pane chrome ({marker}) did not render"
    );
    s.display(sess, "#{pane_id}")
}

/// Spawn `tma wait <args>` against the scratch server + manifest dir, stdout/stderr piped so the
/// row (and the error line) can be read after exit.
fn spawn_wait(s: &Scratch, args: &[&str]) -> Child {
    spawn_wait_with_config(s, empty_config_path(), args)
}

/// [`spawn_wait`] for the tests that stage a change mid-wait and must know the waiter got there
/// first. The zero freshness window puts every cycle on the producer path, which is what makes the
/// waiter's progress visible to [`await_waiter_cycles`]; nothing else about the wait changes.
fn spawn_staged_wait(s: &Scratch, args: &[&str]) -> Child {
    spawn_wait_with_config(s, eager_config_path(), args)
}

fn spawn_wait_with_config(s: &Scratch, config: &Path, args: &[&str]) -> Child {
    Command::new(s.bin())
        .arg("wait")
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tma wait")
}

/// A process-shared config that turns the per-pane stamp freshness window off, so a waiter started
/// with it re-reads its panes every cycle instead of trusting a stamp for three seconds. A settled
/// pane otherwise leaves most cycles with nothing to do and nothing to show for them, which is what
/// [`await_waiter_cycles`] needs. FIXED name, like the harness's empty config: racing writes from
/// parallel tests write identical bytes.
fn eager_config_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let p = std::env::temp_dir().join("tma-test-eager-config.toml");
        let _ = std::fs::write(&p, b"[fold]\nfreshness_secs = 0\n");
        p
    })
    .as_path()
}

/// Block until the waiter has completed `n` whole poll cycles, so what its earlier cycles latched —
/// `observed_pane`, `observed_agent`, the `--agent` pin — is in place before the test stages the
/// next step.
///
/// This replaces a fixed staging sleep, which is a bet on how fast the box is: under a loaded one
/// the waiter had not reached its entry cycle when the staged change landed, and a `--pane` killed
/// before it was ever observed reads as never-launched (exit 124) rather than vanished (exit 3).
///
/// The marker is `@tma_last_poll`, the stampede guard's server option: a cycle writes it as its last
/// act, and only once it has produced (hence [`spawn_staged_wait`]). The waiter is single-threaded,
/// so a cycle can only begin after the previous one returned and its rows were evaluated — counting
/// two of them proves the first cycle's evaluation ran, not merely that a cycle started.
///
/// False when the waiter exits first or the ceiling lapses; the caller decides which of those its
/// own assertion should name.
fn await_waiter_cycles(s: &Scratch, child: &mut Child, n: usize, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    let mark = |s: &Scratch| {
        String::from_utf8_lossy(&s.tmux(&["show-options", "-sqv", "@tma_last_poll"]).stdout)
            .trim()
            .to_string()
    };
    let mut last = mark(s);
    let mut cycles = 0;
    while cycles < n {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }
        let now = mark(s);
        if now != last {
            last = now;
            cycles += 1;
            continue;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

/// Poll `child` until it exits or `deadline` elapses, returning its exit code (`None` on timeout —
/// a hung `wait`, which is itself a failure).
fn await_exit(child: &mut Child, deadline: Duration) -> Option<i32> {
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.code(),
            Ok(None) if Instant::now() >= end => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn read_stdout(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut buf);
    }
    buf
}

/// The waiter's stderr, for the exit-code assertions to quote. `wait` names every non-zero end on
/// stderr, and an unexpected code is unreadable without it: a scratch server that died under load
/// exits 1 and says so, which otherwise looks exactly like a logic regression.
fn read_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut buf);
    }
    buf.trim().to_string()
}

/// Re-run `tma ls` until the pane's `@agent_state` equals `want` (or `timeout` elapses). Under heavy
/// parallel load the first capture can precede the chrome, so entry-state pre-checks poll not assert.
fn poll_agent_state(s: &Scratch, sess: &str, want: &str, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let _ = s.tma(&["ls"]);
        if s.display(sess, "#{@agent_state}") == want {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Re-run `tma ls` until the pane carries an `@agent_name` (it resolved to an agent at all), for the
/// cases that care about the row's presence rather than its state.
fn poll_agent_identified(s: &Scratch, sess: &str, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let _ = s.tma(&["ls"]);
        if !s.display(sess, "#{@agent_name}").is_empty() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// SETUP-only settle: poll the pane to `want`, re-discovering the process names and rewriting the
/// manifest inside the loop. Under full parallelism the one-shot name discovery can race the pane's
/// process coming up — a shell still sourcing its rc, a `printf; exec sleep` not yet past the
/// `exec` — and leave a manifest that names something the pane no longer runs. The pane then either
/// resolves to no agent at all or matches only in the subtree, whose foreground cap stamps
/// `unknown`; re-discovery converges both. The tests' real assertions are untouched.
fn settle_agent_state(s: &Scratch, sess: &str, want: &str, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    let mut names = pane_process_names(s, sess);
    write_manifest(s, &names);
    loop {
        let _ = s.tma(&["ls"]);
        if s.display(sess, "#{@agent_state}") == want {
            return true;
        }
        if Instant::now() >= end {
            // A bare "never settled" says nothing about why, and this settle has cost the suite
            // whole sessions of guesswork. Print what the detector saw beside the manifest it was
            // matched against, which names the cause outright.
            let pane = s.display(sess, "#{pane_id}");
            let explain = s.tma(&["debug", "explain", &pane]);
            eprintln!(
                "settle_agent_state({sess}, {want}) lapsed; manifest process_names = {names:?}\n{}{}",
                String::from_utf8_lossy(&explain.stdout),
                String::from_utf8_lossy(&explain.stderr),
            );
            return false;
        }
        let fresh = pane_process_names(s, sess);
        if fresh != names && !fresh.iter().all(|n| n.is_empty()) {
            names = fresh;
            write_manifest(s, &names);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Epoch milliseconds, the unit every stamp instant is written in.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// (a) An already-blocked pane returns immediately: exit 0 with the row on stdout. The stamps are
/// populated first (a `tma ls`), so `wait`'s entry cycle observes `blocked` on tick 0.
#[test]
fn already_blocked_returns_immediately() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-now");
    let pane = static_agent(&s, "work", BLOCKED_CHROME, "Do you want to proceed?");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "blocked", POLL_CEILING),
        "the pane must settle to blocked before the wait"
    );

    let out = s.tma(&["wait", "--pane", &pane, "--until", "blocked"]);
    assert_eq!(out.status.code(), Some(0), "observed → exit 0");
    let row = String::from_utf8_lossy(&out.stdout);
    assert!(row.contains(&pane), "the row names the pane: {row:?}");
    assert!(
        row.contains("blocked"),
        "the row carries the state: {row:?}"
    );
}

/// (a′) `--json` prints one schema-1 object with the ls-row keys.
#[test]
fn already_blocked_json_object() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-json");
    let pane = static_agent(&s, "work", BLOCKED_CHROME, "Do you want to proceed?");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "blocked", POLL_CEILING),
        "the pane must settle to blocked before the wait"
    );

    let out = s.tma(&["wait", "--pane", &pane, "--until", "blocked", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("\"schema\":1"), "schema-1 object: {json}");
    assert!(
        json.contains(&format!("\"pane\":\"{pane}\"")),
        "pane key: {json}"
    );
    assert!(json.contains("\"state\":\"blocked\""), "state key: {json}");
}

/// (b) `--timeout` on a pane that never reaches the target exits 124 with nothing on stdout.
#[test]
fn timeout_on_never_blocked_exits_124() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-timeout");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "the pane must settle to idle before the wait"
    );

    let start = Instant::now();
    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "blocked",
        "--timeout",
        "2",
    ]);
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(124), "timeout → exit 124");
    assert!(out.stdout.is_empty(), "no row on a timeout");
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(8),
        "honored the 2 s timeout within tolerance, took {elapsed:?}"
    );
}

/// (c) Transition: the pane starts idle, the test flips it to blocked chrome mid-wait, and `wait`
/// unblocks on a later cycle. An interactive shell pane lets `send-keys` reprint the screen.
#[test]
fn transition_idle_to_blocked_unblocks() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-transition");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "work", "-x", "100", "-y", "24"])
        .status
        .success());
    // Show idle chrome and let it render, then discover the (shell) process names.
    s.tmux(&["send-keys", "-t", "work", "printf 'READY\\n'", "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "work", "READY", POLL_CEILING),
        "idle chrome did not render"
    );
    let pane = s.display("work", "#{pane_id}");
    // SETUP-only: settle via `settle_agent_state`, which re-discovers process names inside the loop
    // (a bare state poll flakes when a raced one-shot discovery leaves a never-matching manifest).
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "pane must settle to idle before the flip"
    );

    // Belt timeout well past both the transition and the staging gate below. The gate waits on the
    // waiter's own progress rather than a duration, so the belt has to outlast a loaded box's idea
    // of two poll cycles; `timeout_on_never_blocked_exits_124` is what pins the belt itself.
    let mut child = spawn_staged_wait(
        &s,
        &["--pane", &pane, "--until", "blocked", "--timeout", "60"],
    );
    // Flip only once the waiter has observed the pane idle, so this exercises a real idle→blocked
    // transition rather than an already-blocked entry return.
    assert!(
        await_waiter_cycles(&s, &mut child, 2, POLL_CEILING),
        "the waiter never got through its entry cycle"
    );

    let flip = format!("printf '{BLOCKED_CHROME}'");
    s.tmux(&["send-keys", "-t", "work", &flip, "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "work", "Do you want to proceed?", POLL_CEILING),
        "blocked chrome did not render after the flip"
    );

    let code = await_exit(&mut child, POLL_CEILING);
    let row = read_stdout(&mut child);
    assert_eq!(
        code,
        Some(0),
        "wait must unblock on the transition to blocked"
    );
    assert!(
        row.contains("blocked"),
        "the printed row is blocked: {row:?}"
    );
}

/// (d) A targeted `--pane` that vanishes while waiting exits 3 (distinct from a timeout). A second
/// session keeps the server alive, so killing the agent pane is a pane vanish, not a server death.
#[test]
fn pane_vanish_exits_3() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-vanish");
    // A keep-alive session so the server survives the agent pane's death.
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "keep",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000"
        ])
        .status
        .success());
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "the pane must settle to idle before the wait"
    );

    let mut child = spawn_staged_wait(
        &s,
        &["--pane", &pane, "--until", "blocked", "--timeout", "60"],
    );
    // The kill must land after the waiter has seen the pane alive, else the disappearance reads as a
    // never-appeared pane (timeout) rather than a vanish (exit 3).
    assert!(
        await_waiter_cycles(&s, &mut child, 2, POLL_CEILING),
        "the waiter never got through its entry cycle"
    );
    assert!(s.tmux(&["kill-pane", "-t", &pane]).status.success());

    let code = await_exit(&mut child, POLL_CEILING);
    assert_eq!(
        code,
        Some(3),
        "a vanished --pane is exit 3, not 124; wait said: {:?}",
        read_stderr(&mut child)
    );
}

/// (e) `--agent` matching more than one pane is an error (exit 1) naming both candidates — scripts
/// must be deterministic, never a silent first-match.
#[test]
fn ambiguous_agent_is_an_error() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-ambig");
    let pane_a = static_agent(&s, "a", "READY\\n", "READY");
    let pane_b = static_agent(&s, "b", "READY\\n", "READY");
    // One manifest matches both panes' sleep process, so both are the agent named `agent`.
    write_manifest(&s, &pane_process_names(&s, "a"));
    assert!(s.tma(&["ls"]).status.success());
    // Only A re-discovers: the panes run the same command, so the manifest A converges on is the
    // one B needs, and rewriting it again from B's names could only narrow it.
    assert!(
        settle_agent_state(&s, "a", "idle", POLL_CEILING),
        "pane A must settle to idle before the wait"
    );
    assert!(
        poll_agent_state(&s, "b", "idle", POLL_CEILING),
        "pane B must settle to idle before the wait"
    );

    let out = s.tma(&[
        "wait",
        "--agent",
        "agent",
        "--until",
        "blocked",
        "--timeout",
        "10",
    ]);
    assert_eq!(out.status.code(), Some(1), "ambiguous --agent → exit 1");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&pane_a), "names candidate {pane_a}: {err:?}");
    assert!(err.contains(&pane_b), "names candidate {pane_b}: {err:?}");
}

/// (e′) pin-to-first-observed: `--agent` locks onto the pane it sees FIRST and returns on ITS
/// transition; a second same-named pane appearing mid-wait must NOT flip the wait to the ambiguity
/// error. Without the pin, the cycle after the second pane appears would see two candidates and exit 1.
#[test]
fn agent_pins_to_first_observed_ignoring_a_later_second_pane() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-pin");
    // Pane A: an interactive shell (so it can be flipped to blocked mid-wait) showing idle chrome.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "a", "-x", "100", "-y", "24"])
        .status
        .success());
    s.tmux(&["send-keys", "-t", "a", "printf 'READY\\n'", "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "a", "READY", POLL_CEILING),
        "pane A idle chrome did not render"
    );
    // The manifest matches the interactive shell, so both panes (A here, B below — both the SAME
    // shell) resolve to agent `agent`. A second interactive pane (not a `sleep`-backed static one)
    // is used deliberately: its process names match A's, so ONE manifest covers both.
    let pane_a = s.display("a", "#{pane_id}");
    // Settle with in-loop name re-discovery (same flake class as the transition test's setup).
    assert!(
        settle_agent_state(&s, "a", "idle", POLL_CEILING),
        "A did not reach idle"
    );

    let mut child = spawn_staged_wait(
        &s,
        &["--agent", "agent", "--until", "blocked", "--timeout", "60"],
    );
    // Pane B may only appear once the waiter has observed A as the SOLE candidate and pinned to it —
    // the whole point of the test.
    assert!(
        await_waiter_cycles(&s, &mut child, 2, POLL_CEILING),
        "the waiter never got through its entry cycle, so it never pinned to A"
    );

    // Pane B: a second interactive shell (same shell ⇒ same agent name), also idle. Now `--agent
    // agent` matches TWO panes; a non-pinned waiter would exit 1 on its next cycle.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "b", "-x", "100", "-y", "24"])
        .status
        .success());
    s.tmux(&["send-keys", "-t", "b", "printf 'READY\\n'", "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "b", "READY", POLL_CEILING),
        "pane B idle chrome did not render"
    );
    let pane_b = s.display("b", "#{pane_id}");
    assert!(
        poll_agent_state(&s, "b", "idle", POLL_CEILING),
        "B did not reach idle"
    );
    assert_ne!(pane_a, pane_b, "A and B are distinct panes");
    // Whole cycles with BOTH panes present + idle: the pinned waiter must keep waiting, never error
    // on the ambiguity. A waiter that did error exits, which ends the gate early and lands on the
    // assertion that names it.
    let cycled = await_waiter_cycles(&s, &mut child, 2, POLL_CEILING);
    assert!(
        child.try_wait().unwrap().is_none(),
        "the pinned waiter must NOT exit (no ambiguity error) while a second same-named pane exists"
    );
    assert!(
        cycled,
        "the pinned waiter stopped cycling with both panes up"
    );

    // Flip the PINNED pane A to blocked; the waiter returns on it, naming A, not B.
    let flip = format!("printf '{BLOCKED_CHROME}'");
    s.tmux(&["send-keys", "-t", "a", &flip, "Enter"]);
    assert!(
        wait_capture_contains(&s.socket, "a", "Do you want to proceed?", POLL_CEILING),
        "pane A blocked chrome did not render after the flip"
    );

    let code = await_exit(&mut child, POLL_CEILING);
    let row = read_stdout(&mut child);
    assert_eq!(
        code,
        Some(0),
        "pinned --agent returns on the first pane's transition"
    );
    assert!(
        row.contains(&pane_a) && row.contains("blocked"),
        "the row is pinned pane A's blocked row: {row:?}"
    );
    assert!(
        !row.contains(&pane_b),
        "the waiter ignored the later second pane B: {row:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Fleet targets (`--all`, `--count`) and the `--since` floor. All poll-path, no daemon.
// ---------------------------------------------------------------------------------------------

/// (i) `--all` is a barrier over the panes in scope: two idle panes satisfy it and BOTH rows print,
/// while one straggler holds it open to the timeout. An empty scope is a usage error, not a vacuous
/// success.
#[test]
fn all_barrier_needs_every_pane_and_rejects_an_empty_scope() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-all");
    let pane_a = static_agent(&s, "a", "READY\\n", "READY");
    let pane_b = static_agent(&s, "b", "READY\\n", "READY");
    // One manifest matches both panes' `sleep`, so both are the agent `agent`.
    write_manifest(&s, &pane_process_names(&s, "a"));
    assert!(s.tma(&["ls"]).status.success());
    // Only A re-discovers: the panes run the same command, so the manifest A converges on is the
    // one B needs, and rewriting it again from B's names could only narrow it.
    assert!(
        settle_agent_state(&s, "a", "idle", POLL_CEILING),
        "pane A must settle to idle before the wait"
    );
    assert!(
        poll_agent_state(&s, "b", "idle", POLL_CEILING),
        "pane B must settle to idle before the wait"
    );

    let out = s.tma(&["wait", "--all", "--until", "idle"]);
    assert_eq!(out.status.code(), Some(0), "both panes are idle");
    let rows = String::from_utf8_lossy(&out.stdout);
    assert!(
        rows.contains(&pane_a) && rows.contains(&pane_b),
        "the barrier prints every member's row: {rows:?}"
    );
    assert_eq!(rows.lines().count(), 2, "one line per member: {rows:?}");

    // A state only one pane can be in: the barrier holds open until the timeout.
    let out = s.tma(&[
        "wait",
        "--all",
        "--until",
        "blocked",
        "--timeout",
        "2",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(124),
        "a straggler holds the barrier to the timeout"
    );
    assert!(out.stdout.is_empty(), "no document on a timeout");

    // Nothing in scope: a barrier over an empty fleet is exit 2, never a vacuous 0.
    let out = s.tma(&[
        "wait",
        "--all",
        "--session",
        "nosuchsession",
        "--until",
        "idle",
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an empty scope is a usage error"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("nothing to wait for"),
        "the message says why: {err:?}"
    );
}

/// (j) `--count <n>` is a quorum: one idle pane satisfies `--count 1`, two are needed for
/// `--count 2` (which times out with only one in scope), and `--json` emits the schema-1 `agents`
/// document rather than a single row object.
#[test]
fn count_quorum_returns_the_satisfied_set() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-count");
    let pane_a = static_agent(&s, "a", "READY\\n", "READY");
    let pane_b = static_agent(&s, "b", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "a"));
    assert!(s.tma(&["ls"]).status.success());
    // Only A re-discovers: the panes run the same command, so the manifest A converges on is the
    // one B needs, and rewriting it again from B's names could only narrow it.
    assert!(
        settle_agent_state(&s, "a", "idle", POLL_CEILING),
        "pane A must settle to idle before the wait"
    );
    assert!(
        poll_agent_state(&s, "b", "idle", POLL_CEILING),
        "pane B must settle to idle before the wait"
    );

    let out = s.tma(&["wait", "--count", "1", "--until", "idle"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().count(),
        2,
        "the quorum returns every satisfying row, not just the first n"
    );

    let out = s.tma(&["wait", "--count", "2", "--until", "idle", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"schema\":1") && json.contains("\"agents\":["),
        "a fleet wait emits the rows document: {json}"
    );
    assert!(json.contains(&format!("\"pane\":\"{pane_a}\"")));
    assert!(json.contains(&format!("\"pane\":\"{pane_b}\"")));

    // Only two panes exist, so a quorum of three never forms.
    let out = s.tma(&["wait", "--count", "3", "--until", "idle", "--timeout", "2"]);
    assert_eq!(out.status.code(), Some(124));
}

/// (l) An agent that dies under a wait while its pane survives ends the wait at exit 4, naming the
/// pane, instead of blocking to the timeout. A shell pane runs `sleep` as its "agent" (the manifest
/// matches that process only); `C-c` kills it, so the pane lives on with no agent row.
#[test]
fn agent_death_under_a_live_pane_exits_4() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-crash");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "work", "-x", "100", "-y", "24"])
        .status
        .success());
    s.tmux(&[
        "send-keys",
        "-t",
        "work",
        "printf 'READY\\n'; sleep 100000",
        "Enter",
    ]);
    assert!(
        wait_capture_contains(&s.socket, "work", "READY", POLL_CEILING),
        "idle chrome did not render"
    );
    let pane = s.display("work", "#{pane_id}");
    // The agent is the `sleep` child ALONE: matching the pane's shell too would keep the pane an
    // agent after the kill, which is exactly the state this test must escape.
    write_manifest(&s, &["sleep".to_string()]);
    // The wait is on `blocked`, so any agent row will do — only the row's presence matters here.
    assert!(
        poll_agent_identified(&s, "work", POLL_CEILING),
        "the sleep-backed pane never registered an agent"
    );

    let mut child = spawn_staged_wait(
        &s,
        &["--pane", &pane, "--until", "blocked", "--timeout", "60"],
    );
    // The agent may only die once the waiter has seen the pane CARRYING an agent row, else the
    // missing row reads as a not-yet-launched agent and the wait blocks on.
    assert!(
        await_waiter_cycles(&s, &mut child, 2, POLL_CEILING),
        "the waiter never got through its entry cycle"
    );
    s.tmux(&["send-keys", "-t", "work", "C-c"]);

    let code = await_exit(&mut child, POLL_CEILING);
    assert_eq!(
        code,
        Some(4),
        "a departed agent under a live pane is exit 4, not 124; wait said: {:?}",
        read_stderr(&mut child)
    );
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    assert!(
        err.contains(&pane) && err.contains("exit 4"),
        "the message names the pane: {err:?}"
    );
    assert!(
        !s.display("work", "#{pane_id}").is_empty(),
        "the pane itself outlived its agent"
    );
}

/// (k) `--since` is the level-trigger escape hatch: the episode the pane is ALREADY in does not
/// satisfy a wait floored at its own transition time, while a floor one millisecond earlier does.
#[test]
fn since_floor_excludes_the_current_episode() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-since");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "the pane must settle to idle before the wait"
    );

    // The current episode's transition epoch, straight off the stamp the row reports.
    let since: u64 = s
        .display("work", "#{@agent_since}")
        .parse()
        .expect("a stamped @agent_since");

    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "idle",
        "--since",
        &since.to_string(),
        "--timeout",
        "2",
    ]);
    assert_eq!(
        out.status.code(),
        Some(124),
        "the episode that began AT the floor does not satisfy it"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!("since {since}")),
        "the timeout line names the floor: {err:?}"
    );

    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "idle",
        "--since",
        &(since - 1).to_string(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a floor below the transition lets the same episode through"
    );
}

/// (l) The supervisor loop's real lap, end to end: wait, read the floor out of the JSON row it
/// printed, feed it back. `wait` compares the EPISODE instant (the later of `@agent_since` and
/// `@agent_turn_at`), so once a second turn end has landed on a pane that never left `idle`, a loop
/// feeding `since_ms` back sets a floor the row already clears and every lap returns instantly.
/// `episode_ms` is the key that closes the loop, and the contrast is asserted both ways.
#[test]
fn a_fed_back_episode_floor_blocks_the_next_lap() {
    if !have_tmux() {
        return;
    }
    fn key(json: &str, name: &str) -> u64 {
        let at = json
            .find(&format!("\"{name}\":"))
            .unwrap_or_else(|| panic!("{name} is missing from the row: {json}"))
            + name.len()
            + 3;
        json[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .expect("a numeric floor")
    }

    let s = Scratch::new("wait-episode");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "the pane must settle to idle before the wait"
    );
    let since: u64 = s
        .display("work", "#{@agent_since}")
        .parse()
        .expect("a stamped @agent_since");

    // A turn end landing inside the idle run: `@agent_since` is write-once and cannot move, so the
    // second completion's instant lives only here. (The poll cycle never writes this key.)
    let turn_at = since + 5_000;
    s.set_opt(&pane, "@agent_turn_at", &turn_at.to_string());

    // Lap 1: floored at the idle run's start, the new turn end satisfies.
    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "idle",
        "--since",
        &since.to_string(),
        "--json",
        "--timeout",
        "5",
    ]);
    assert_eq!(out.status.code(), Some(0), "the fresh turn end satisfies");
    let row = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(key(&row, "since_ms"), since);
    assert_eq!(key(&row, "episode_ms"), turn_at);

    // Lap 2, floored at what lap 1 handed back: nothing newer has happened, so it must block.
    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "idle",
        "--since",
        &key(&row, "episode_ms").to_string(),
        "--timeout",
        "2",
    ]);
    assert_eq!(
        out.status.code(),
        Some(124),
        "the completion lap 1 handled must not re-satisfy lap 2"
    );

    // And the spin the key exists to prevent: `since_ms` is a floor this row already clears.
    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "idle",
        "--since",
        &key(&row, "since_ms").to_string(),
        "--timeout",
        "2",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "feeding since_ms back re-satisfies immediately, which is why the recipe reads episode_ms"
    );
}

/// (m) The ordered-input clear must never run before the goal it feeds. A human is parked on the
/// pane, an earlier cycle (the daemon's sweep, a status-line `tma status`) raised the completion
/// marker, and they have typed since — so the clear is armed, and `wait --until done` runs the very
/// cycle that fires it. `done` is `idle + @agent_attention`, so a clear landing inside the cycle
/// retracts the mark out of the rows the goal is then evaluated against and the waiter blocks to its
/// own timeout on a completion that was standing when it looked.
///
/// A real PTY client is the whole point: `client_activity` moves only for an attached terminal and
/// only on genuine input, never on `send-keys` and never on tma's own command clients.
#[test]
fn a_waiter_does_not_retract_the_done_mark_it_is_waiting_for() {
    if !have_tmux() {
        return;
    }
    let mut s = Scratch::new("wait-seen");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    assert!(
        settle_agent_state(&s, "work", "idle", POLL_CEILING),
        "the pane must settle to idle before the marker goes up"
    );
    match s.attach_client("work") {
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
    // `client_activity` can arm the clear — only the keystroke below can. `@agent_since` is
    // write-once per state run, so the idle run the pane is already in keeps this instant.
    let raised = now_ms();
    s.set_opt(&pane, "@agent_since", &raised.to_string());
    s.set_opt(&pane, "@agent_attention", "1");
    // The keystroke after it: this, and only this, is what arms the clear against this pane.
    s.type_client_input_past(raised);

    let mut child = spawn_wait(&s, &["--pane", &pane, "--until", "done", "--timeout", "8"]);
    let code = await_exit(&mut child, POLL_CEILING);
    let row = read_stdout(&mut child);
    let err = read_stderr(&mut child);
    assert_eq!(
        code,
        Some(0),
        "the waiter must return on the mark that was standing when its cycle read it, \
         not retract it and block: {err:?}"
    );
    assert!(
        row.contains(&pane),
        "the satisfied row names the waited-on pane: {row:?}"
    );
    // Ordered, not skipped: the waiter still applies the clear, it just does it after the read. If
    // this ever reads `1` the deferral has quietly turned `wait` into a non-clearer, and a marker on
    // a pane its owner is typing into would stand until something else happens to poll.
    assert_eq!(
        s.pane_option(&pane, "@agent_attention"),
        "",
        "the cycle that satisfied the wait still retires the marker it read"
    );
}

// ---------------------------------------------------------------------------------------------
// Daemon-assisted push. These three spawn (or fake) a daemon; the six above stay on the poll path
// (no daemon ⇒ `try_subscribe` returns None), so their semantics are unchanged.
// ---------------------------------------------------------------------------------------------

/// A foreground `tma daemon` child, reaped on drop.
struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn a foreground daemon for the scratch server, writing the status file so a test can gate on
/// `clients` (daemon up) and `wait_subscribers` (push mode active). It shares the scratch's socket +
/// manifest-dir, so it keys the SAME per-server socket the wait client probes.
fn spawn_daemon(s: &Scratch, status: &std::path::Path) -> DaemonChild {
    let child = Command::new(s.bin())
        .arg("daemon")
        .args(["--socket-name", &s.socket])
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .arg("--status-file")
        .arg(status)
        .env("TMA_CONFIG", empty_config_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tma daemon");
    DaemonChild(child)
}

/// A manifest mapping a synthetic `Block` event to `blocked` (so `tma event Block` stamps through the
/// daemon), with `[hooks] covers` so the pane is hook-driven and quiet-edge capture cannot fight the
/// hook stamp. The `READY` idle rule keeps the pane idle until the event, so `--until blocked` blocks.
fn write_hook_manifest(s: &Scratch, names: &[String]) {
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
             [hooks]\ncovers = [\"working\", \"idle\", \"blocked\", \"lifecycle\"]\n\
             [[hooks.map]]\nevent = \"Block\"\nclaim = {{ state = \"blocked\", detail = \"permission\" }}\n\
             [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();
}

/// Fire `tma event --agent agent --kind Block` at the daemon (as a hook would), delivering the frame
/// over the per-server socket. The daemon applies the `blocked` stamp and pushes its subscribers.
fn fire_block(s: &Scratch, pane: &str) {
    let mut child = Command::new(s.bin())
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
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMUX_PANE", pane)
        .env("TMA_CONFIG", empty_config_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s"}"#)
        .unwrap();
    assert!(child.wait().expect("wait tma event").success());
}

/// (f) push latency: with a daemon running and a `tma wait` subscribed, a hook edge wakes the waiter
/// well before the push-mode belt would. Gated on `wait_subscribers == 1` so the measurement is an
/// honest push wake. The bound is provably a push, not the belt: the waiter parks on `wait_edge` with
/// a 5 s cap (`PUSH_FALLBACK`), and it parked no later than the subscription registering, which is
/// `>= start - 500 ms`, so the belt's earliest possible re-cycle is `start + 4.5 s` (a `poll(5 s)`
/// never fires early). A `< 3.5 s` return therefore cannot be the belt, with ~1 s of slack below that
/// 4.5 s floor. The old `< 2 s` bound proved the same thing but had no slack for a load-slowed push
/// (each re-cycle tmux one-shot can stall up to `TMUX_TIMEOUT` = 3 s), so it tripped under load.
#[test]
fn push_edge_returns_well_under_the_poll_tick() {
    let _gate = DaemonTestGuard::acquire();
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-push");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_hook_manifest(&s, &pane_process_names(&s, "work"));
    let status = s.workdir.join("daemon-status");
    let _daemon = spawn_daemon(&s, &status);
    // Generous boot patience: daemon startup is the load-sensitive step (a fresh tmux server + control
    // client), unrelated to the latency proof, which only starts its clock after the push fires below.
    assert!(
        wait_status_eq(&status, "clients", "1", POLL_CEILING),
        "daemon came up with one control client"
    );

    // A 60 s `--timeout` (belt-floor unaffected: the push-fallback cap is min(5 s, remaining)) so a
    // load-delayed subscription still lands well before the waiter would give up.
    let mut child = spawn_wait(
        &s,
        &["--pane", &pane, "--until", "blocked", "--timeout", "60"],
    );
    assert!(
        wait_status_eq(&status, "wait_subscribers", "1", POLL_CEILING),
        "tma wait subscribed to the daemon's edge pushes"
    );
    // Let the waiter finish its entry cycle and park on the push, so the clock measures the push wake.
    std::thread::sleep(Duration::from_millis(500));

    let start = Instant::now();
    fire_block(&s, &pane); // → daemon stamps blocked → PUSH → wait wakes and its cycle observes it
    let code = await_exit(&mut child, POLL_CEILING);
    let elapsed = start.elapsed();
    let row = read_stdout(&mut child);
    assert_eq!(
        code,
        Some(0),
        "the push woke wait and its cycle observed blocked"
    );
    assert!(
        row.contains("blocked"),
        "the printed row is blocked: {row:?}"
    );
    eprintln!("push-latency edge→exit: {} ms", elapsed.as_millis());
    assert!(
        elapsed < Duration::from_millis(3500),
        "the push woke wait before the belt could (belt floor is ~4.5 s), was {} ms",
        elapsed.as_millis()
    );

    // The waiter exited, so its subscription fd hung up; the daemon reaps it and `wait_subscribers`
    // returns to 0 promptly (the reap marks the status dirty, not waiting on an edge or the sweep).
    assert!(
        wait_status_eq(&status, "wait_subscribers", "0", POLL_CEILING),
        "the waiter's subscription is reaped and the gauge returns to 0 after it exits"
    );
}

/// (g) degrade: a daemon killed mid-wait drops the subscription (EOF); the waiter falls back to the
/// poll loop, still honors `--timeout` and exits 124, never hangs or errors on the daemon's death.
#[test]
fn daemon_killed_mid_wait_degrades_and_still_times_out() {
    let _gate = DaemonTestGuard::acquire();
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-daemon-die");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    // Capture-only manifest: the pane stays idle and never reaches blocked, so the ONLY way the wait
    // ends is the timeout, which must still fire after the daemon dies.
    write_manifest(&s, &pane_process_names(&s, "work"));
    let status = s.workdir.join("daemon-status");
    let mut daemon = spawn_daemon(&s, &status);
    assert!(wait_status_eq(&status, "clients", "1", POLL_CEILING));

    let mut child = spawn_wait(
        &s,
        &["--pane", &pane, "--until", "blocked", "--timeout", "5"],
    );
    assert!(
        wait_status_eq(&status, "wait_subscribers", "1", POLL_CEILING),
        "tma wait subscribed before the daemon is killed"
    );
    // Kill the daemon mid-wait: the subscription EOFs, so wait degrades to the poll loop.
    let _ = daemon.0.kill();
    let _ = daemon.0.wait();

    let code = await_exit(&mut child, POLL_CEILING);
    assert_eq!(
        code,
        Some(124),
        "daemon death mid-wait degrades to poll and still honors --timeout (exit 124)"
    );
}

/// (h) version skew: a daemon predating push support (faked by a listener that NAKs the subscribe
/// frame) makes `try_subscribe` fall back to the poll loop; the pane never blocks, so the waiter
/// times out (124), a silent degrade. The fake holds the connection OPEN after the NAK so EOF cannot
/// be a second degrade trigger (a client that misread the ack would else pass vacuously), and we
/// assert it genuinely polled: accepted exactly once (`hits == 1`) and timed out on schedule.
#[test]
fn version_skew_subscribe_falls_back_to_poll() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("wait-skew");
    let pane = static_agent(&s, "work", "READY\\n", "READY");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success()); // populate stamps (pane is idle)

    // Bind a fake pre-push daemon at the keyed socket. Read `#{socket_path}` straight from tmux (the
    // EXACT value the client's `resolve_socket_path` sees, which differs from `scratch_socket_path` on
    // macOS `/tmp` vs `/private/tmp`), then derive the keyed path so both land on one socket.
    let socket_path = s.display("work", "#{socket_path}");
    assert!(!socket_path.is_empty(), "resolved a server socket path");
    let keyed = tma_runtime::ipc::paths_for(&socket_path);
    std::fs::create_dir_all(&keyed.dir).unwrap();
    let _ = std::fs::remove_file(&keyed.socket);
    let listener = UnixListener::bind(&keyed.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let (stop2, hits2) = (stop.clone(), hits.clone());
    let fake = std::thread::spawn(move || {
        // Hold every accepted connection OPEN for the thread's life: dropping `c` would
        // let EOF, not just the NAK, drive the client's degrade.
        let mut held: Vec<_> = Vec::new();
        while !stop2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut c, _)) => {
                    hits2.fetch_add(1, Ordering::Relaxed);
                    let mut buf = [0u8; 8];
                    let _ = c.read(&mut buf); // consume the subscribe frame
                    let _ = c.write_all(&[0x15]); // NAK: an old daemon rejects the unknown magic
                    held.push(c); // keep the socket open; do NOT close it
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let out = s.tma(&[
        "wait",
        "--pane",
        &pane,
        "--until",
        "blocked",
        "--timeout",
        "2",
    ]);
    let elapsed = start.elapsed();
    assert_eq!(
        out.status.code(),
        Some(124),
        "version-skew subscribe degrades to poll and still times out"
    );
    // Timed out ON SCHEDULE (polled to the 2 s deadline), not returned early: proves the client fell
    // through to the poll loop and honored --timeout there.
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(8),
        "the wait polled to the 2 s deadline (degrade is a poll fallback), took {elapsed:?}"
    );
    // Exactly one accept: the NAK degraded the ONE subscribe attempt to the poll loop, and a poll
    // loop never reconnects. `>= 1` would also admit a reconnect storm; `== 1` pins the honest path.
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "the wait subscribed exactly once and was NAKed (no reconnect, no vacuous absent-socket pass)"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = fake.join();
    let _ = std::fs::remove_file(&keyed.socket);
}
