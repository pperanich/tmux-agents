//! Control-mode client pool acceptance: one `tmux -C` client per session, pool membership tracking
//! `%sessions-changed`/lifecycle, per-pane active→quiet activity edges, the behavior probe +
//! faster-sweep degrade, pane-close clear, and zero-member recovery, all on a scratch tmux server.
//!
//! Every daemon is a FOREGROUND `tma daemon` child reaped on drop ([`DaemonGuard`]); every
//! daemon targets ONLY the scratch server (`--socket-name <scratch>` + a private
//! `XDG_RUNTIME_DIR`). The control-client `tmux -C` children the daemon spawns are its direct
//! children, reaped when the daemon dies and again when the scratch server is killed on drop:
//! so no control client leaks. Introspection is the daemon's `--status-file` (control-pool
//! membership, probe verdict, sweep interval, edge + recovery counts).

use std::process::Command;
use std::time::{Duration, Instant};

use common::{DaemonGuard, Scratch};
use tma_test_support as common;

/// A detached session running an idle `sleep` (no output), returning its pane id.
fn new_idle_session(s: &Scratch, name: &str) -> String {
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
    pane
}

/// A detached session running a real shell (so `send-keys` produces output), pane id.
fn new_shell_session(s: &Scratch, name: &str) -> String {
    assert!(s
        .tmux(&["new-session", "-d", "-s", name, "-x", "80", "-y", "24"])
        .status
        .success());
    let pane = s.get(name, "#{pane_id}");
    assert!(pane.starts_with('%'), "got pane {pane:?}");
    pane
}

/// Spawn a foreground daemon with a status file; `extra` adds flags (e.g. `--probe-cross-session`).
/// Suite-specific CLI shape over the shared [`Scratch::command`]; reaped on drop by [`DaemonGuard`].
fn spawn_daemon(s: &Scratch, extra: &[&str]) -> DaemonGuard {
    let status = s.status_path();
    let child = s
        .command()
        .args(["daemon", "--socket-name", &s.socket])
        .args(["--status-file", status.to_str().unwrap()])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(s.daemon_log_stdio())
        .spawn()
        .expect("spawn daemon");
    DaemonGuard::new(child)
}

/// Poll the status file until `key` is present, returning its value (empty string on timeout).
fn wait_status_present(s: &Scratch, key: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = s.status().get(key) {
            return v.clone();
        }
        if Instant::now() >= deadline {
            return String::new();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One control client per session: a new session grows the pool, a killed session drops it.
#[test]
fn pool_grows_and_shrinks_with_sessions() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    let _p1 = new_idle_session(&s, "s1");
    let _daemon = spawn_daemon(&s, &[]);

    // Startup: exactly one client for the one session.
    s.expect_status("clients", "1");

    // A new session ⇒ `%sessions-changed` ⇒ the pool grows to two.
    let _p2 = new_idle_session(&s, "s2");
    s.expect_status("clients", "2");

    // Killing a session ⇒ its client is dropped ⇒ back to one.
    assert!(s.tmux(&["kill-session", "-t", "s2"]).status.success());
    s.expect_status("clients", "1");
}

/// Pane-close clears that pane's published state promptly with no `SessionEnd`: a blocked agent
/// pane in a two-pane window is killed, and the window `@agent_summary` rollup clears.
#[test]
fn pane_close_clears_published_state() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    let pane_a = new_idle_session(&s, "s1");
    // A second pane in the same window; pane A is the "agent".
    let pane_b = {
        let out = s.tmux(&[
            "split-window",
            "-t",
            &pane_a,
            "-P",
            "-F",
            "#{pane_id}",
            "exec sleep 100000",
        ]);
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    assert!(pane_b.starts_with('%'));

    // Publish a blocked stamp on pane A plus the window rollup (as a stamp would).
    assert!(s
        .tmux(&["set-option", "-p", "-t", &pane_a, "@agent_state", "blocked"])
        .status
        .success());
    assert!(s
        .tmux(&["set-option", "-p", "-t", &pane_a, "@agent_name", "claude"])
        .status
        .success());
    assert!(s
        .tmux(&[
            "set-option",
            "-w",
            "-t",
            &pane_a,
            "@agent_summary",
            "blocked:1"
        ])
        .status
        .success());
    assert_eq!(s.get(&pane_b, "#{@agent_summary}"), "blocked:1");

    let _daemon = spawn_daemon(&s, &[]);
    s.expect_status("clients", "1");

    // The agent dies without SessionEnd: kill pane A. The daemon's lifecycle event ⇒ the
    // window-summary reconcile ⇒ the rollup clears (no surviving agent pane).
    assert!(s.tmux(&["kill-pane", "-t", &pane_a]).status.success());

    common::poll_until(
        "pane close clears the window rollup promptly, no SessionEnd",
        || s.get(&pane_b, "#{@agent_summary}").is_empty(),
    );
}

/// Activity on a pane yields exactly ONE active→quiet edge past the quiet threshold, not one
/// per output line in the burst.
#[test]
fn one_quiet_edge_per_output_burst() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    let pane = new_shell_session(&s, "s1");
    let _daemon = spawn_daemon(&s, &[]);
    s.expect_status("clients", "1");

    // Let attach-time shell output settle, then read the edge count. Poll the `active` gauge (not a
    // blind sleep): quiescent means every attach-noise edge has drained, so the baseline is stable.
    assert!(
        common::wait_daemon_quiescent(
            &s.status_path(),
            Duration::from_millis(1200),
            common::POLL_CEILING,
        ),
        "daemon did not reach quiescence (attach-noise never drained)"
    );
    let baseline: u64 = wait_status_present(&s, "edges", common::POLL_CEILING)
        .parse()
        .unwrap_or(0);

    // One output burst: five rapid lines, all inside the 1 s quiet threshold. The pane's own shell
    // loop emits them, from a SINGLE `send-keys`. Five separate `send-keys` calls would put five
    // process spawns inside the window being measured, and one spawn slower than ~940 ms (routine
    // on a saturated runner, where spawn measures p50 3.8 s) would split the burst in two and fail
    // the count below — a harness artifact indistinguishable from the collapse regressing.
    assert!(s
        .tmux(&[
            "send-keys",
            "-t",
            &pane,
            "i=0; while [ $i -lt 5 ]; do echo tma-burst; i=$((i+1)); done",
            "Enter",
        ])
        .status
        .success());

    // Past the quiet threshold, exactly one new edge fires for the burst.
    common::poll_until("the burst produced an edge", || {
        s.status()
            .get("edges")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(baseline)
            > baseline
    });
    // The negative window: wait for the pool to fall quiescent rather than sleeping 500 ms. A
    // spurious second edge can only appear a full quiet threshold (1 s) after more output, so the
    // old fixed sleep was half as long as the event it was meant to exclude; quiescence is
    // `active == 0` held past that threshold, which is the condition itself.
    assert!(
        common::wait_daemon_quiescent(
            &s.status_path(),
            Duration::from_millis(1200),
            common::POLL_CEILING,
        ),
        "the pool never fell quiescent after the burst{}",
        s.forensics(&[&pane])
    );
    let final_edges: u64 = s
        .status()
        .get("edges")
        .and_then(|v| v.parse().ok())
        .unwrap_or(baseline);
    assert_eq!(
        final_edges - baseline,
        1,
        "exactly one active→quiet edge per burst (baseline {baseline}, final {final_edges}){}",
        s.forensics(&[&pane])
    );
}

/// Behavior probe: on this tmux (3.6a) session-scoped push works, so the probe reports
/// available and the daemon keeps the normal (long) sweep cadence.
#[test]
fn behavior_probe_reports_available_on_target() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    let _p1 = new_idle_session(&s, "s1");
    let _daemon = spawn_daemon(&s, &[]);

    // control-mode activity push is available on the target tmux
    s.expect_status("probe", "available");
    let st = s.status();
    assert_eq!(st.get("degraded").map(String::as_str), Some("0"));
    assert_eq!(
        st.get("sweep_ms").map(String::as_str),
        Some("45000"),
        "available ⇒ normal reconciliation sweep cadence"
    );
}

/// Degrade path: forced into the useless cross-session subscribe (silently dropped), the probe
/// reports unavailable and the daemon degrades to the faster sweep, restating the interval.
#[test]
fn behavior_probe_degrades_on_useless_cross_session() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    // Two sessions must exist BEFORE the daemon so the forced probe has a foreign target.
    let _p1 = new_idle_session(&s, "s1");
    let _p2 = new_idle_session(&s, "s2");
    let _daemon = spawn_daemon(&s, &["--probe-cross-session"]);

    // a silently-useless cross-session subscribe ⇒ probe reports unavailable
    s.expect_status("probe", "unavailable");
    let st = s.status();
    assert_eq!(st.get("degraded").map(String::as_str), Some("1"));
    assert_eq!(
        st.get("sweep_ms").map(String::as_str),
        Some("5000"),
        "degrade ⇒ faster reconciliation sweep, interval restated"
    );
    // The pool still attaches its real per-session clients regardless of the probe verdict.
    s.expect_status("clients", "2");
}

/// Zero-member recovery: force-drop every control client while the sessions survive; the daemon
/// re-enumerates via `list-sessions` and re-attaches.
#[test]
fn zero_member_recovery_reattaches() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    // A single session ⇒ killing its one client drops the pool to a true zero while the
    // session survives, deterministically exercising the zero-member recovery path.
    let _p1 = new_idle_session(&s, "s1");
    let daemon = spawn_daemon(&s, &[]);
    s.expect_status("clients", "1");
    let recoveries_before: u64 = s
        .status()
        .get("recoveries")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Kill every direct child of the daemon (its control clients); the session stays alive.
    // ps + kill rather than `pkill -P`: pgrep/pkill are absent in minimal environments
    // (the nix sandbox), and ps is already what the production walk requires.
    let ps = Command::new("ps")
        .args(["-eo", "pid=,ppid="])
        .output()
        .expect("ps");
    let daemon_pid = daemon.pid().to_string();
    let killed = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid = cols.next()?.parse::<i32>().ok()?;
            if cols.next()? != daemon_pid {
                return None;
            }
            rustix::process::Pid::from_raw(pid)
        })
        .filter(|pid| rustix::process::kill_process(*pid, rustix::process::Signal::TERM).is_ok())
        .count();
    assert!(killed > 0, "no control client found under the daemon");

    // The daemon sees the EOF, re-enumerates, and re-attaches. Wait for the recovery to be recorded:
    // `clients` is 1 before and after, so poll the monotone recovery counter instead.
    let mut recoveries_after = recoveries_before;
    common::poll_until("a zero-member recovery to be recorded", || {
        recoveries_after = s
            .status()
            .get("recoveries")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        recoveries_after > recoveries_before
    });
    assert!(
        recoveries_after > recoveries_before,
        "a zero-member recovery was recorded ({recoveries_before} → {recoveries_after})"
    );
    s.expect_status("clients", "1");
}

/// Server-gone terminates the daemon cleanly via `%exit`/EOF on the control clients (extends
/// the server-gone contract to the control-mode path).
#[test]
fn server_gone_terminates_daemon_via_control() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("control");
    let _p1 = new_idle_session(&s, "s1");
    let mut daemon = spawn_daemon(&s, &[]);
    s.expect_status("clients", "1");

    assert!(s.tmux(&["kill-server"]).status.success());
    assert!(
        daemon.wait_exit(common::POLL_CEILING),
        "server-gone (%exit/EOF on the control client) terminates the daemon"
    );
}

fn pump_protocol_pool(
    pool: &mut tma_tmux::control::ControlPool,
    tmux: &tma_tmux::tmux::Tmux,
) -> tma_tmux::control::ProgramDrain {
    let ready = tma_tmux::control::pollfds(pool)
        .into_iter()
        .map(|(session, _)| session)
        .collect();
    let effects = tma_tmux::control::dispatch_ready(pool, &ready, Instant::now());
    if effects.need_reconcile {
        pool.reconcile(tmux).unwrap();
    }
    tma_tmux::control::seed_attached(pool, tmux, Instant::now());
    pool.drain_program()
}

fn emit_synthetic_report_after_sync(s: &Scratch, pane: &str) {
    // Ordinary bytes establish the possible DCS phase before ST, even if the
    // first observed ESC was paired with one emitted before attach.
    let command = "printf 'framing\\033\\\\\\033]7501;state=working:app=pi\\033\\\\'";
    assert!(s
        .tmux(&["send-keys", "-t", pane, command, "Enter"])
        .status
        .success());
}

#[test]
fn protocol_pool_linked_window_rename_and_owner_transfer() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("protocol_owner");
    let pane = new_shell_session(&s, "first");
    let first_id = s.get("first", "#{session_id}");
    assert!(s
        .tmux(&["new-session", "-d", "-s", "second", "exec sleep 100000"])
        .status
        .success());
    assert!(s
        .tmux(&["link-window", "-s", "first:0", "-t", "second:1"])
        .status
        .success());
    let tmux =
        tma_tmux::tmux::Tmux::connect(&tma_tmux::tmux::Server::named(Some(s.socket.clone())));
    let mut pool = tma_tmux::control::ControlPool::default();
    pool.set_program_collection(true);
    pool.reconcile(&tmux).unwrap();
    // Rename before attach seeding. IDs must retain coverage and the activity seed.
    assert!(s
        .tmux(&["rename-session", "-t", &first_id, "renamed"])
        .status
        .success());
    common::poll_until("both protocol clients attach and seed by stable ID", || {
        pump_protocol_pool(&mut pool, &tmux);
        !pool.pending_seed() && tma_tmux::control::pollfds(&pool).len() == 2
    });
    // Await attach handshake explicitly through the seed's activity edge.
    let mut seeded = false;
    common::poll_until("renamed session's pane is seeded", || {
        pump_protocol_pool(&mut pool, &tmux);
        tma_tmux::control::tick(
            &mut pool,
            Instant::now() + Duration::from_secs(2),
            1,
            Duration::from_secs(45),
        );
        seeded |= pool.drain_edges().iter().any(|edge| edge.pane == pane);
        seeded
    });
    // Establish framing explicitly before the synthetic report, independent
    // of whether shell startup already emitted a safe delimiter.
    emit_synthetic_report_after_sync(&s, &pane);
    let mut events = Vec::new();
    common::poll_until("linked pane emits one owned report", || {
        events.extend(pump_protocol_pool(&mut pool, &tmux).observations);
        !events.is_empty()
    });
    let first_epoch = events[0].connection;
    // Drain both clients fully to expose any duplicate of the same write.
    for _ in 0..5 {
        events.extend(pump_protocol_pool(&mut pool, &tmux).observations);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        events.len(),
        1,
        "linked streams must yield one logical event"
    );
    assert!(s.tmux(&["kill-session", "-t", &first_id]).status.success());
    pool.reconcile(&tmux).unwrap();
    let gap = pool.drain_program();
    assert!(gap
        .invalidations
        .iter()
        .any(|event| event.pane == tma_core::program_status::PaneId::parse(&pane).unwrap()));
    assert!(!gap.all_invalidated);
    common::poll_until("surviving client remains attached", || {
        pump_protocol_pool(&mut pool, &tmux);
        tma_tmux::control::pollfds(&pool).len() == 1
    });
    emit_synthetic_report_after_sync(&s, &pane);
    let mut transferred = Vec::new();
    common::poll_until(
        "fresh report recovers transferred stream after safe ST",
        || {
            transferred.extend(pump_protocol_pool(&mut pool, &tmux).observations);
            !transferred.is_empty()
        },
    );
    assert!(transferred[0].connection > first_epoch);
}

#[test]
fn protocol_pool_real_output_tail_and_second_session_progress() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("protocol_fairness");
    let busy = new_shell_session(&s, "busy");
    let quiet = new_shell_session(&s, "quiet");
    let tmux =
        tma_tmux::tmux::Tmux::connect(&tma_tmux::tmux::Server::named(Some(s.socket.clone())));
    let mut pool = tma_tmux::control::ControlPool::default();
    pool.set_program_collection(true);
    pool.reconcile(&tmux).unwrap();
    // Observe both attach seeds before issuing either pane's controlled output.
    let mut seeded = std::collections::HashSet::new();
    common::poll_until("both sessions attach before output", || {
        pump_protocol_pool(&mut pool, &tmux);
        tma_tmux::control::tick(
            &mut pool,
            Instant::now() + Duration::from_secs(2),
            1,
            Duration::from_secs(45),
        );
        seeded.extend(pool.drain_edges().into_iter().map(|edge| edge.pane));
        seeded.contains(&busy) && seeded.contains(&quiet)
    });
    let command = "awk 'BEGIN { for (i=0; i<30000; i++) printf \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\" }'; printf '\\033\\\\\\033]7501;state=working:app=pi\\033\\\\'";
    assert!(s
        .tmux(&["send-keys", "-t", &busy, command, "Enter"])
        .status
        .success());
    emit_synthetic_report_after_sync(&s, &quiet);
    let mut observed = std::collections::HashSet::new();
    common::poll_until(
        "quiet session and long ordinary output tail both make progress",
        || {
            for event in pump_protocol_pool(&mut pool, &tmux).observations {
                observed.insert(event.pane);
            }
            observed.contains(&tma_core::program_status::PaneId::parse(&busy).unwrap())
                && observed.contains(&tma_core::program_status::PaneId::parse(&quiet).unwrap())
        },
    );
}

#[test]
fn native_tmux_dcs_keeps_cancel_and_escaped_escape_bytes_as_data() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        return;
    }
    let s = Scratch::new("protocol_dcs");
    let mut bytes = Vec::new();
    for (index, data) in [b"\x1b\x1b\\".as_slice(), b"\x18", b"\x1a"]
        .iter()
        .enumerate()
    {
        bytes.extend_from_slice(b"\x1bPtma;data");
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(format!("HIDDEN{index}\x1b\\VISIBLE{index}\n").as_bytes());
    }
    let payload = s.workdir.join("dcs.bin");
    let script = s.workdir.join("emit.sh");
    std::fs::write(&payload, bytes).unwrap();
    std::fs::write(&script, "cat \"$1\"; exec sleep 100000\n").unwrap();
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "dcs",
            "-x",
            "80",
            "-y",
            "24",
            "sh",
            script.to_str().unwrap(),
            payload.to_str().unwrap(),
        ])
        .status
        .success());
    let mut capture = String::new();
    common::poll_until(
        "native tmux consumes the DCS and renders each following marker",
        || {
            let output = s.tmux(&["capture-pane", "-p", "-t", "dcs"]);
            capture = String::from_utf8(output.stdout).unwrap();
            (0..3).all(|index| capture.contains(&format!("VISIBLE{index}")))
        },
    );
    assert!(
        !capture.contains("HIDDEN"),
        "DCS data escaped into terminal display: {capture:?}"
    );
}
