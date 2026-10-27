//! `tma subscribe` acceptance on a scratch server. `subscribe` is a long-running stream — one JSON
//! document per line — so every test spawns it with piped stdout, reads timestamped lines off a
//! background thread, and kills it on drop. The cases pin the stream's contract: a live daemon pushes a
//! change well under the poll interval; a daemon killed mid-stream degrades to polling with no error
//! line and no EOF; an older daemon that NAKs the subscribe magic degrades to polling; an emitted
//! line is byte-identical to what `ls --json` prints (the own-cycle / source-guard proof); plus the
//! two emission modifiers, `--changes-only` (poll mode stops repeating) and `--events` (a silent
//! baseline, then one edge per transition). Scratch `tmux -L` server, killed on drop.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tma_test_support::{
    empty_config_path, poll_until, wait_capture_contains, wait_status_eq, AttachOutcome,
    DaemonTestGuard, Scratch, POLL_CEILING,
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

/// A capture-only manifest with an `idle` rule (`READY`): the pane resolves to agent `agent` and
/// stays idle, so the stream's steady-state document is deterministic.
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
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();
}

/// A hook manifest mapping a synthetic `Block` event to `blocked` (so `tma event Block` stamps
/// through the daemon), with `[hooks] covers` so the pane is hook-driven and quiet-edge capture
/// cannot fight the hook stamp. The `READY` idle rule keeps it idle until the event fires.
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

/// Launch a static detached agent pane (`printf READY`, then a long-lived `sleep`) and return its
/// pane id, once the `READY` marker proves the chrome rendered and the shell reached `exec`.
fn static_agent(s: &Scratch, sess: &str) -> String {
    let cmd = "printf 'READY\\n'; exec sleep 100000";
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
            cmd,
        ])
        .status
        .success());
    assert!(
        wait_capture_contains(&s.socket, sess, "READY", POLL_CEILING),
        "agent pane chrome (READY) did not render"
    );
    s.display(sess, "#{pane_id}")
}

/// Epoch milliseconds, the unit every stamp instant is written in.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A long-running `tma subscribe` child: its stdout lines (timestamped) and stderr lines are read on
/// background threads into shared vecs, and the child is SIGKILL-reaped on drop (subscribe exits only
/// on a signal or when its stdout closes, so a test never waits for it to finish on its own).
struct SubscribeChild {
    child: Child,
    lines: Arc<Mutex<Vec<(Instant, String)>>>,
    errs: Arc<Mutex<Vec<String>>>,
    readers: Vec<JoinHandle<()>>,
}

impl SubscribeChild {
    fn spawn(s: &Scratch, interval: u64) -> SubscribeChild {
        SubscribeChild::spawn_with(s, interval, &[])
    }

    /// [`SubscribeChild::spawn`] plus the emission-mode flags (`--changes-only`, `--events`).
    fn spawn_with(s: &Scratch, interval: u64, extra: &[&str]) -> SubscribeChild {
        Self::spawn_with_tmux(s, interval, extra, None)
    }

    fn spawn_with_tmux(
        s: &Scratch,
        interval: u64,
        extra: &[&str],
        binary: Option<&std::path::Path>,
    ) -> SubscribeChild {
        let mut command = s.command();
        if let Some(binary) = binary {
            command.env("TMA_TMUX_BIN", binary);
        }
        let mut child = command
            .args(["subscribe", "--json", "--interval", &interval.to_string()])
            .args(extra)
            .args(["--socket-name", &s.socket])
            .arg("--manifest-dir")
            .arg(&s.workdir)
            .env("TMA_CONFIG", empty_config_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tma subscribe");

        let lines = Arc::new(Mutex::new(Vec::new()));
        let errs = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let (lines2, errs2) = (lines.clone(), errs.clone());
        let r1 = std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                lines2.lock().unwrap().push((Instant::now(), line));
            }
        });
        let r2 = std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                errs2.lock().unwrap().push(line);
            }
        });
        SubscribeChild {
            child,
            lines,
            errs,
            readers: vec![r1, r2],
        }
    }

    fn events_after_baseline(s: &Scratch, pane: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let polls = s.workdir.join("subscriber-polls");
        std::fs::write(&polls, "").unwrap();
        let wrapper = s.workdir.join("observe-subscriber-polls");
        // Exactly one LAST_POLL read begins each cycle. Starting the third proves both
        // the silent baseline and one unchanged render have completed in this child.
        std::fs::write(&wrapper, format!("#!/bin/sh\nread_options=0\nread_poll=0\nfor arg do\n[ \"$arg\" != show-options ] || read_options=1\n[ \"$arg\" != '@tma_last_poll' ] || read_poll=1\ndone\nif [ $read_options -eq 1 ] && [ $read_poll -eq 1 ]; then printf x >> '{}'; fi\nexec tmux \"$@\"\n", polls.display())).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut sub = SubscribeChild::spawn_with_tmux(s, 1, &["--events"], Some(&wrapper));
        assert!(
            tma_test_support::wait_until(POLL_CEILING, || sub.is_running()
                && std::fs::metadata(&polls).is_ok_and(|meta| meta.len() >= 3)),
            "subscriber never completed its baseline: stderr={:?}{}",
            sub.errors(),
            s.forensics(&[pane])
        );
        sub
    }

    fn snapshot(&self) -> Vec<(Instant, String)> {
        self.lines.lock().unwrap().clone()
    }

    fn line_count(&self) -> usize {
        self.lines.lock().unwrap().len()
    }

    fn errors(&self) -> Vec<String> {
        self.errs.lock().unwrap().clone()
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Block until a stdout line satisfies `pred` (returning it with its arrival instant) or
    /// `timeout` elapses.
    fn wait_for_line(
        &self,
        pred: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> Option<(Instant, String)> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(hit) = self.snapshot().into_iter().find(|(_, l)| pred(l)) {
                return Some(hit);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for SubscribeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
    }
}

/// A foreground `tma daemon` child, reaped on drop.
struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn a foreground daemon for the scratch server, writing the status file so a test can gate on
/// `clients` (daemon up) and `wait_subscribers` (a stream subscribed). Shares the scratch's socket +
/// manifest-dir, so it keys the SAME per-server socket the stream probes.
fn spawn_daemon(s: &Scratch, status: &std::path::Path) -> DaemonChild {
    let child = s
        .command()
        .arg("daemon")
        .args(["--socket-name", &s.socket])
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .arg("--status-file")
        .arg(status)
        .env("TMA_CONFIG", empty_config_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(s.daemon_log_stdio())
        .spawn()
        .expect("spawn tma daemon");
    DaemonChild(child)
}

/// Fire `tma event --agent agent --kind Block` at the daemon (as a hook would): the daemon applies
/// the `blocked` stamp and pushes its subscribers.
fn fire_block(s: &Scratch, pane: &str) -> std::process::Output {
    let mut child = s
        .command()
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
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s"}"#)
        .unwrap();
    let output = child.wait_with_output().expect("wait tma event");
    assert!(
        output.status.success(),
        "event intake: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// (a) drift / own-cycle: with no daemon (poll mode), a stream line is byte-identical to what `ls
/// --json` prints for the same pane. This is the source-guard proof — the emission is built from the
/// subscriber's OWN cycle, never from a socket payload.
#[test]
fn emitted_line_matches_ls_json() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("sub-drift");
    let pane = static_agent(&s, "work");
    write_manifest(&s, &pane_process_names(&s, "work"));
    // Stamp the pane idle so both the stream's entry cycle and `ls --json` see the same steady state.
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "idle");

    let sub = SubscribeChild::spawn(&s, 1);
    let (_, line) = sub
        .wait_for_line(
            |l| l.contains(&format!("\"pane\":\"{pane}\"")),
            POLL_CEILING,
        )
        .expect("the stream emitted a document naming the pane");

    // The same document `ls --json` prints right now (the pane is settled idle in both).
    let ls = s.tma(&["ls", "--json"]);
    assert!(ls.status.success());
    let ls_json = String::from_utf8_lossy(&ls.stdout).trim_end().to_string();
    assert_eq!(
        line, ls_json,
        "a subscribe line is byte-identical to `ls --json` (its own cycle, not the socket)"
    );
    assert!(line.contains("\"schema\":1"), "schema-1 document: {line}");
}

/// (b) live-daemon push latency: with a daemon running and the stream subscribed, a hook edge lands
/// a `blocked` line well under the poll interval. `--interval 10` and the 5 s push-mode belt both
/// exceed the < 3 s assertion, so a fast `blocked` line is provably a push, not a poll tick or belt.
#[test]
fn live_daemon_pushes_change_under_poll_interval() {
    let _gate = DaemonTestGuard::acquire();
    if !have_tmux() {
        return;
    }
    let s = Scratch::new_daemon("sub-push");
    let pane = static_agent(&s, "work");
    write_hook_manifest(&s, &pane_process_names(&s, "work"));
    let status = s.workdir.join("daemon-status");
    let _daemon = spawn_daemon(&s, &status);
    assert!(
        wait_status_eq(&status, "clients", "1", POLL_CEILING),
        "daemon came up with one control client{}",
        s.forensics(&[&pane])
    );

    let sub = SubscribeChild::spawn(&s, 10);
    assert!(
        wait_status_eq(&status, "wait_subscribers", "1", POLL_CEILING),
        "the stream subscribed to the daemon's edge pushes"
    );
    // Let the entry snapshot land and the stream park on the push, so the clock measures the wake.
    std::thread::sleep(Duration::from_millis(500));

    let start = Instant::now();
    fire_block(&s, &pane); // → daemon stamps blocked → PUSH → stream wakes, its cycle observes it
    let hit = sub.wait_for_line(
        |l| l.contains("\"state\":\"blocked\""),
        Duration::from_secs(8),
    );
    let (at, line) = hit.expect("a blocked document arrived");
    let elapsed = at.duration_since(start);
    eprintln!("push edge→blocked line: {} ms", elapsed.as_millis());
    assert!(
        line.contains(&format!("\"pane\":\"{pane}\"")),
        "the blocked document names the pane: {line}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the push delivered under the 10 s interval and the 5 s belt, was {} ms",
        elapsed.as_millis()
    );
}

/// (c) degrade: a daemon killed mid-stream drops the subscription (EOF); the stream falls back to the
/// unconditional `--interval` poll, so it keeps emitting, prints NO error line, and does NOT exit
/// (no EOF on its own stdout). Killing the daemon changes only latency.
#[test]
fn daemon_killed_mid_stream_continues_via_polling() {
    let _gate = DaemonTestGuard::acquire();
    if !have_tmux() {
        return;
    }
    let s = Scratch::new_daemon("sub-degrade");
    let pane = static_agent(&s, "work");
    write_manifest(&s, &pane_process_names(&s, "work"));
    let status = s.workdir.join("daemon-status");
    let mut daemon = spawn_daemon(&s, &status);
    assert!(
        wait_status_eq(&status, "clients", "1", POLL_CEILING),
        "daemon came up with one control client{}",
        s.forensics(&[&pane])
    );

    let mut sub = SubscribeChild::spawn(&s, 1);
    assert!(
        wait_status_eq(&status, "wait_subscribers", "1", POLL_CEILING),
        "the stream subscribed before the daemon is killed"
    );
    // In push mode the stream is quiet after the entry snapshot; baseline the line count, then kill.
    std::thread::sleep(Duration::from_millis(400));
    let baseline = sub.line_count();

    let _ = daemon.0.kill();
    let _ = daemon.0.wait();

    // After the death the stream degrades to the 1 s poll, which emits every interval unconditionally.
    poll_until(
        "the stream to keep emitting on the poll cadence after the daemon died",
        || sub.line_count().saturating_sub(baseline) >= 2,
    );
    assert!(
        sub.is_running(),
        "the stream did not exit when the daemon died (no EOF on its own stdout)"
    );
    assert!(
        sub.errors().is_empty(),
        "the degrade printed no error line: {:?}",
        sub.errors()
    );
}

/// (d) version skew: a daemon predating push support (faked by a listener that NAKs the subscribe
/// frame) makes the stream fall back to the poll loop. It still emits on the `--interval` cadence and
/// never errors — a silent degrade, latency only.
#[test]
fn version_skew_nak_falls_back_to_poll() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new_daemon("sub-skew");
    let _pane = static_agent(&s, "work");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success()); // populate stamps (pane is idle)

    // Bind a fake pre-push daemon at the keyed socket. Read `#{socket_path}` straight from tmux (the
    // EXACT value the client's `resolve_socket_path` sees), then derive the keyed path.
    let socket_path = s.display("work", "#{socket_path}");
    assert!(!socket_path.is_empty(), "resolved a server socket path");
    let socket = s.workdir.join("tma").join(format!(
        "{}.sock",
        tma_runtime::ipc::socket_key(&socket_path)
    ));
    let fake = tma_test_support::NakServer::new(socket);

    let mut sub = SubscribeChild::spawn(&s, 1);
    // The NAK degrades the stream to the poll loop, which emits every second: two lines prove it
    // fell through to polling despite the fake daemon.
    poll_until("the stream to poll through the NAK", || {
        sub.line_count() >= 2
    });
    assert!(
        sub.is_running(),
        "the stream is still running (a silent degrade)"
    );
    assert!(
        sub.errors().is_empty(),
        "version-skew degrade printed no error line: {:?}",
        sub.errors()
    );

    assert!(fake.hits() >= 1, "a real subscribe frame was NAKed");
    drop(sub);
    drop(fake);
}

/// (e) `--changes-only` in poll mode: a settled server goes quiet after the entry snapshot instead
/// of repeating the same document every interval. The plain stream beside it is the control — same
/// server, same cadence, and it keeps re-emitting.
#[test]
fn changes_only_silences_the_poll_mode_repeat() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("sub-quiet");
    let _pane = static_agent(&s, "work");
    write_manifest(&s, &pane_process_names(&s, "work"));
    // Settle the pane to idle before either stream starts, so neither counts a startup transition.
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "idle");

    let quiet = SubscribeChild::spawn_with(&s, 1, &["--changes-only"]);
    let loud = SubscribeChild::spawn(&s, 1);
    // The control stream re-emits every interval; three of its lines is the window over which the
    // quiet stream below must have stayed silent.
    poll_until("the control stream to re-emit on three intervals", || {
        loud.line_count() >= 3
    });
    // The entry snapshot always goes out; a second line is tolerated for a first cycle that landed
    // before the stamp settled. What must not happen is one line per interval.
    assert!(
        quiet.line_count() <= 2,
        "--changes-only emitted {} lines on an unchanging server: {:?}",
        quiet.line_count(),
        quiet.snapshot().iter().map(|(_, l)| l).collect::<Vec<_>>()
    );
    assert!(
        quiet.line_count() >= 1,
        "the entry snapshot is still emitted"
    );
    assert!(quiet.errors().is_empty(), "{:?}", quiet.errors());
}

/// (f) `--events`: the first cycle is a silent baseline (no synthetic edges for panes that were
/// already running), then each state transition is one edge record. Daemonless, so the poll tick
/// drives the same diff a push would.
#[test]
fn events_emit_one_edge_per_transition_after_a_silent_baseline() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("sub-events");
    let pane = static_agent(&s, "work");
    write_hook_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "idle");

    let mut sub = SubscribeChild::events_after_baseline(&s, &pane);
    assert_eq!(
        sub.line_count(),
        0,
        "no synthetic edges for the initial snapshot: {:?}",
        sub.snapshot().iter().map(|(_, l)| l).collect::<Vec<_>>()
    );

    // No daemon is running, so `tma event` direct-stamps the pane blocked; the stream's next cycle
    // observes the transition.
    let intake = fire_block(&s, &pane);
    let intake_stamp = s.get(
        &pane,
        "#{@agent_state}|#{@agent_pid}|#{@agent_stamped_at}|#{@agent_session}",
    );
    let edge = sub.wait_for_line(|l| l.contains("\"to\":\"blocked\""), POLL_CEILING);
    assert!(edge.is_some(), "missing idle-to-blocked edge: child_alive={}, intake_status={:?}, intake_stderr={}, intake_stamp={intake_stamp:?}, lines={:?}, stream_errors={:?}{}", sub.is_running(), intake.status.code(), String::from_utf8_lossy(&intake.stderr), sub.snapshot(), sub.errors(), s.forensics(&[&pane]));
    let (_, line) = edge.unwrap();

    assert!(line.starts_with("{\"schema\":1,\"at_ms\":"), "{line}");
    assert!(line.contains("\"from\":\"idle\""), "{line}");
    assert!(line.contains(&format!("\"pane\":\"{pane}\"")), "{line}");
    assert!(line.contains("\"agent\":\"agent\""), "{line}");
    assert!(
        !line.contains("\"agents\":"),
        "an edge is not a snapshot document: {line}"
    );
    assert!(sub.errors().is_empty(), "{:?}", sub.errors());
}

/// (g) The same read-then-clear ordering as `tma wait`, on the stream. `done` is idle +
/// `@agent_attention`, so `diff_rows` only ever sees the completion if the rows the renderer diffs
/// still carry the flag. With a real client parked on the pane and a keystroke already behind the
/// raise, the ordered-input clear is armed the instant the marker exists — and an inline clear
/// retires it inside the very cycle that would have reported it, so the `done` edge is never
/// emitted at all (not merely followed by its retraction). Both edges are asserted, in order.
#[test]
fn the_stream_reports_the_done_edge_before_retiring_it() {
    if !have_tmux() {
        return;
    }
    let mut s = Scratch::new("sub-seen");
    let pane = static_agent(&s, "work");
    write_manifest(&s, &pane_process_names(&s, "work"));
    assert!(s.tma(&["ls"]).status.success());
    assert_eq!(s.display("work", "#{@agent_state}"), "idle");

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

    // Type FIRST, then raise the marker behind that keystroke. Staged this way there is no window
    // in which the stream could see the flag before the clear is armed, so a pass cannot be the
    // stream merely winning a race with the user's typing.
    s.type_client_input_past(0);
    poll_until("the client's last input second to elapse", || {
        now_ms() > s.client_activity_ms() + 1_000
    });
    let last_input = s.client_activity_ms();

    let sub = SubscribeChild::events_after_baseline(&s, &pane);
    assert_eq!(
        sub.line_count(),
        0,
        "no synthetic edges for the initial snapshot: {:?}",
        sub.snapshot().iter().map(|(_, l)| l).collect::<Vec<_>>()
    );

    // The completion, raised behind the keystroke above. `@agent_since` is write-once per state run,
    // so the idle run the pane is already in keeps this instant.
    s.set_opt(&pane, "@agent_since", &(last_input - 1_000).to_string());
    s.set_opt(&pane, "@agent_attention", "1");

    let (_, done) = sub
        .wait_for_line(|l| l.contains("\"to\":\"done\""), POLL_CEILING)
        .unwrap_or_else(|| {
            panic!(
                "the completion was retired inside the cycle that should have reported it: {:?}",
                sub.snapshot().iter().map(|(_, l)| l).collect::<Vec<_>>()
            )
        });
    assert!(done.contains("\"from\":\"idle\""), "{done}");
    assert!(done.contains(&format!("\"pane\":\"{pane}\"")), "{done}");

    // Ordered, not skipped: the stream still applies the clear, one cycle later, and reports the
    // retraction as its own edge.
    assert!(
        sub.wait_for_line(
            |l| l.contains("\"from\":\"done\"") && l.contains("\"to\":\"idle\""),
            POLL_CEILING
        )
        .is_some(),
        "the stream still retires the marker after reporting it: {:?}",
        sub.snapshot().iter().map(|(_, l)| l).collect::<Vec<_>>()
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_attention"),
        "",
        "and the clear actually landed on the pane"
    );
    assert!(sub.errors().is_empty(), "{:?}", sub.errors());
}
