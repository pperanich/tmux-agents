//! Daemon acceptance: per-server socket keying, `flock` single instance, the `EventSink` wire
//! protocol, and graceful lifecycle, exercised end to end on a scratch tmux server.
//!
//! Every daemon this file spawns is a FOREGROUND `tma daemon` child whose `Child` is stored in
//! a [`DaemonGuard`] and SIGKILL-reaped on drop, so no daemon can leak across the suite. Each
//! daemon targets ONLY the scratch server: it is spawned with `--socket-name <scratch>` and a
//! private `XDG_RUNTIME_DIR`, so the socket key is derived from the scratch server's
//! `#{socket_path}` and the user's real tmux server is never touched. `tma event` is fired
//! with the same `--socket-name` + `XDG_RUNTIME_DIR`, so client and daemon land on the
//! identical keyed socket path (the socket-targeting property).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{DaemonGuard, Scratch};
use tma_runtime::ipc::{encode_frame, parse_lock, ACK, VERSION};
use tma_test_support as common;

/// The `@agent_*` pane options a stamp writes, for map comparison.
const AGENT_OPTS: &[&str] = &[
    "@agent_name",
    "@agent_state",
    "@agent_detail",
    "@agent_source",
    "@agent_evidence_at",
    "@agent_since",
    "@agent_stamped_at",
    "@agent_attention",
    "@agent_notified_at",
    "@agent_hash",
    "@agent_pid",
    "@agent_session",
    "@agent_subagents",
    "@agent_summary",
    "@agent_session_summary",
];

/// Wall-clock epoch fields: normalized before a cross-run comparison (the two paths may stamp at
/// different seconds). Everything else must be byte-identical, proving the same stamp adapter.
const EPOCH_OPTS: &[&str] = &[
    "@agent_evidence_at",
    "@agent_since",
    "@agent_stamped_at",
    "@agent_notified_at",
];

const SESSION: &str = "65ced290-2a08-43de-aa80-d0b049d7ce30";

/// A fresh detached session running an idle `sleep`, returning its pane id. Suite-specific free
/// helpers over the shared [`Scratch`] (daemon-flavoured: [`Scratch::new_daemon`] +
/// [`Scratch::command`] + [`DaemonGuard`]).
fn new_pane(s: &Scratch, name: &str) -> String {
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

/// The private `tma/` runtime dir the daemon keys its socket under (under the scratch workdir,
/// which doubles as `XDG_RUNTIME_DIR`).
fn tma_dir(s: &Scratch) -> PathBuf {
    s.workdir.join("tma")
}

/// The single `*.sock` under the runtime dir (there is exactly one: one scratch server).
fn socket_file(s: &Scratch) -> Option<PathBuf> {
    let dir = tma_dir(s);
    let entries = std::fs::read_dir(&dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "sock").unwrap_or(false))
}

/// The single `*.lock` sibling of the socket, where the daemon records its pid (`write_pid`).
fn lock_file(s: &Scratch) -> Option<PathBuf> {
    let dir = tma_dir(s);
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "lock").unwrap_or(false))
}

/// Poll the lock file for the pid the daemon writes at startup (after `setsid` + `flock`), so a
/// readable pid proves both already ran. Parsed through the shared decoder, not a bare `parse`, so
/// the test reads the lock exactly the way `tma reload` and `tma doctor` do.
fn daemon_pid(s: &Scratch, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(lock) = lock_file(s) {
            if let Some(info) = std::fs::read_to_string(&lock)
                .ok()
                .and_then(|body| parse_lock(&body))
            {
                return Some(info.pid as u32);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One `ps -o <field>= -p <pid>` field (`ppid`/`pgid`), parsed. Works on macOS and Linux.
fn ps_field(pid: u32, field: &str) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Spawn the foreground daemon targeting the scratch server; the guard reaps it on drop.
fn spawn_daemon(s: &Scratch) -> DaemonGuard {
    spawn_daemon_args(s, &[])
}

/// Spawn the foreground daemon with extra CLI args appended (e.g. `--manifest-dir`).
fn spawn_daemon_args(s: &Scratch, extra: &[&str]) -> DaemonGuard {
    let child = s
        .command()
        .args(["daemon", "--socket-name", &s.socket])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    DaemonGuard::new(child)
}

/// Run `tma daemon --ensure` and return its exit success.
fn run_ensure(s: &Scratch) -> bool {
    s.command()
        .args(["daemon", "--ensure", "--socket-name", &s.socket])
        .output()
        .expect("run --ensure")
        .status
        .success()
}

/// Run `tma reload` and return its exit success.
fn run_reload(s: &Scratch) -> bool {
    s.command()
        .args(["reload", "--socket-name", &s.socket])
        .output()
        .expect("run reload")
        .status
        .success()
}

/// Fire `tma event` as a hook would: `$TMUX_PANE` set, scratch server pinned, payload on stdin.
/// With a daemon up this connects and delivers; with none it direct-stamps.
fn fire(s: &Scratch, agent: &str, kind: &str, pane: &str, payload: &str) {
    let mut child = s
        .command()
        .args(["event", "--agent", agent, "--kind", kind, "--payload", "-"])
        .args(["--socket-name", &s.socket])
        .env("TMUX_PANE", pane)
        .env("TMA_NOTIFY_FROM_EVENT", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn tma event");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    assert!(child.wait().expect("wait tma event").success());
}

/// Read all `@agent_*` options for a pane into a map, normalizing epoch fields so two runs at
/// different seconds still compare equal on the deterministic fields.
fn agent_opts(s: &Scratch, pane: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for key in AGENT_OPTS {
        let val = s.get(pane, &format!("#{{{key}}}"));
        let val = if EPOCH_OPTS.contains(key) && !val.is_empty() {
            "<epoch>".to_string()
        } else {
            val
        };
        map.insert((*key).to_string(), val);
    }
    map
}

/// Poll a format value until it equals `want` or the deadline passes.
fn wait_for(s: &Scratch, pane: &str, fmt: &str, want: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if s.get(pane, fmt) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_socket(s: &Scratch, timeout: Duration) -> Option<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(p) = socket_file(s) {
            return Some(p);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The headline acceptance: a daemon-applied stamp is byte-for-byte identical (modulo the injected
/// wall clock) to a direct-stamp of the same event, proving the same guarded-stamp adapter.
#[test]
fn daemon_stamp_matches_direct_stamp() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");

    // --- daemon path: pane in s1, stamped by a running daemon ---
    let pane1 = new_pane(&s, "s1");
    let via_daemon = {
        let _daemon = spawn_daemon(&s);
        assert!(
            wait_for_socket(&s, common::POLL_CEILING).is_some(),
            "daemon must bind its socket"
        );
        fire(
            &s,
            "claude",
            "SessionStart",
            &pane1,
            &payload("SessionStart", SESSION),
        );
        assert!(
            wait_for(&s, &pane1, "#{@agent_state}", "idle", common::POLL_CEILING),
            "daemon must stamp the pane through the stamp adapter"
        );
        assert_eq!(s.get(&pane1, "#{@agent_source}"), "hook");
        agent_opts(&s, &pane1)
        // daemon guard drops here → daemon reaped
    };

    // --- direct path: a second pane, no daemon, same event ---
    let pane2 = new_pane(&s, "s2");
    // Sanity: no daemon is listening now (socket gone or refuses).
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane2,
        &payload("SessionStart", SESSION),
    );
    assert_eq!(
        s.get(&pane2, "#{@agent_state}"),
        "idle",
        "direct stamp must still work with no daemon (additive invariant)"
    );
    let via_direct = agent_opts(&s, &pane2);

    assert_eq!(
        via_daemon, via_direct,
        "daemon stamp must match the direct stamp field-for-field (epoch fields normalized)"
    );
    // Spot-check the load-bearing fields are actually present, not both-empty.
    assert_eq!(via_daemon.get("@agent_state").unwrap(), "idle");
    assert_eq!(via_daemon.get("@agent_source").unwrap(), "hook");
    assert_eq!(via_daemon.get("@agent_session").unwrap(), SESSION);
    assert_eq!(via_daemon.get("@agent_summary").unwrap(), "idle:1");
    assert_eq!(via_daemon.get("@agent_session_summary").unwrap(), "idle:1");
}

/// `flock` single instance: a second foreground daemon and a `--ensure` both no-op while
/// one daemon holds the lock, and the running daemon keeps serving.
#[test]
fn single_instance_flock() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    let _daemon = spawn_daemon(&s);
    let sock = wait_for_socket(&s, common::POLL_CEILING).expect("daemon must bind");

    // A second foreground daemon finds the lock held and exits 0 promptly (does not run).
    let mut second = DaemonGuard::new(
        s.command()
            .args(["daemon", "--socket-name", &s.socket])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn second daemon"),
    );
    assert!(
        second.wait_exit(common::POLL_CEILING),
        "a second daemon must exit (lock held), not run alongside the first"
    );

    // `--ensure` is likewise a no-op: it detects the held lock and returns 0 without spawning.
    assert!(run_ensure(&s), "--ensure exits 0 when a daemon is running");
    // Still exactly one socket, unchanged.
    assert_eq!(socket_file(&s).as_deref(), Some(sock.as_path()));

    // The one surviving daemon still serves.
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane,
        &payload("SessionStart", SESSION),
    );
    assert!(
        wait_for(&s, &pane, "#{@agent_state}", "idle", common::POLL_CEILING),
        "the single daemon instance keeps serving after the no-op launches"
    );
}

/// Auto-start: with `[daemon] autostart = true`, a surface (`tma ls`) brings the daemon up via the
/// `--ensure` spawn before its own work. The control run (`autostart = false`) proves the surface
/// leaves the daemon down, so auto-start is opt-in and strictly additive.
#[test]
fn autostart_brings_daemon_up_for_a_surface() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    // The scratch server must exist for `--ensure` to key its socket on `#{socket_path}`.
    let _pane = new_pane(&s, "as");

    // Run `tma ls` against the scratch server with an explicit autostart config; returns success.
    let run_ls = |cfg: &Path| -> bool {
        s.command()
            .args(["ls", "--socket-name", &s.socket])
            .args(["--config", cfg.to_str().unwrap()])
            .output()
            .expect("run ls")
            .status
            .success()
    };

    // Control: autostart off ⇒ the surface never spawns a daemon (no socket appears).
    let off = s.workdir.join("off.toml");
    std::fs::write(&off, "[daemon]\nautostart = false\n").unwrap();
    assert!(run_ls(&off), "ls succeeds with autostart off");
    assert!(
        wait_for_socket(&s, Duration::from_millis(750)).is_none(),
        "autostart off: the surface leaves the daemon down (strictly additive)"
    );

    // Opt in: autostart on ⇒ the surface brings the daemon up (its socket appears). The surface
    // still returns success: a spawn never fails or blocks the invoking command.
    let on = s.workdir.join("on.toml");
    std::fs::write(&on, "[daemon]\nautostart = true\n").unwrap();
    assert!(run_ls(&on), "ls succeeds and never blocks on the spawn");
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "autostart on: the surface auto-started the daemon"
    );
}

/// Detach reparent + session lead: `tma daemon --ensure` re-execs the daemon through the two-stage
/// double-SPAWN (launcher → intermediate → daemon). Once the launcher returns, the daemon must have
/// reparented OFF this process (the intermediate is gone → init adopts it, so no defunct daemon
/// accrues under a long-lived launcher), and the startup `setsid` must have made it its own
/// session/group leader (`pgid == pid`, so the launcher's shell exiting cannot signal it).
#[test]
fn ensure_detaches_reparented_and_session_led() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    // The scratch server must exist for `--ensure` to key its socket on `#{socket_path}`.
    let _pane = new_pane(&s, "d");

    // The launcher waits the intermediate, which exits the instant it has re-exec'd the daemon.
    assert!(
        run_ensure(&s),
        "--ensure spawns the detached daemon and exits 0"
    );
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "the detached daemon binds its socket"
    );
    let pid =
        daemon_pid(&s, common::POLL_CEILING).expect("daemon records its pid in the lock file");

    // Reparented: the intermediate that spawned it is gone, so init/a subreaper adopted it — never
    // this test process, the launcher lineage `--ensure` ran under.
    let ppid = ps_field(pid, "ppid").expect("daemon ppid readable");
    assert_ne!(
        ppid,
        std::process::id(),
        "the daemon must reparent off the launcher lineage (no zombie under a launcher)"
    );

    // Session leader: the detached-path `setsid` put the daemon in a fresh session + group, so its
    // group id equals its own pid.
    let pgid = ps_field(pid, "pgid").expect("daemon pgid readable");
    assert_eq!(
        pgid, pid,
        "setsid: the detached daemon leads its own process group (pgid == pid)"
    );

    // No leak: the daemon reparented away (no `DaemonGuard` holds it), so SIGKILL it directly.
    if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
    }
}

/// Server-gone terminates the daemon cleanly: killing the scratch server makes the daemon
/// exit within one liveness tick, removing its socket and releasing the lock, with no zombie.
#[test]
fn server_gone_terminates_daemon() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let _pane = new_pane(&s, "s1");
    let mut daemon = spawn_daemon(&s);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );

    // Kill the tmux server out from under the daemon.
    assert!(s.tmux(&["kill-server"]).status.success());

    assert!(
        daemon.wait_exit(common::POLL_CEILING),
        "daemon must exit when its tmux server is gone"
    );
    // Socket removed and lock released on the clean shutdown path.
    common::poll_until(
        "daemon must remove its socket on server-gone shutdown",
        || socket_file(&s).is_none(),
    );
}

/// A graceful SIGTERM shuts the daemon down and removes the socket (clean lifecycle).
#[test]
fn sigterm_shuts_down_cleanly() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let _pane = new_pane(&s, "s1");
    let mut daemon = spawn_daemon(&s);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );

    signal(daemon.pid(), rustix::process::Signal::TERM);

    assert!(
        daemon.wait_exit(common::POLL_CEILING),
        "daemon must exit on SIGTERM"
    );
    assert!(
        socket_file(&s).is_none(),
        "SIGTERM shutdown must remove the socket"
    );
}

/// A malformed frame is dropped and the daemon keeps serving: garbage bytes on the
/// socket, then a real `tma event` still stamps.
#[test]
fn malformed_frame_does_not_crash_daemon() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    let _daemon = spawn_daemon(&s);
    let sock = wait_for_socket(&s, common::POLL_CEILING).expect("daemon must bind");

    // Garbage: bad magic + random bytes, then close. The daemon must drop it, not crash.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        stream
            .write_all(b"NOPEnot a real frame\xff\x00\x01")
            .unwrap();
        // A second connection that sends a valid magic but a truncated body.
        drop(stream);
        let mut stream2 = UnixStream::connect(&sock).expect("connect2");
        stream2.write_all(b"TMA1\xff\xff\xff\xff").unwrap();
        drop(stream2);
    }

    // The daemon still serves a subsequent valid event.
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane,
        &payload("SessionStart", SESSION),
    );
    assert!(
        wait_for(&s, &pane, "#{@agent_state}", "idle", common::POLL_CEILING),
        "daemon must keep serving after a malformed frame"
    );
}

/// The additive invariant, re-asserted at the daemon layer: with no daemon at all, `tma
/// event` direct-stamps unchanged.
#[test]
fn daemonless_event_direct_stamps() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    // No daemon spawned; the runtime dir has no socket, so the sink connect fails and the
    // event falls through to a direct stamp.
    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", SESSION),
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "working");
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    assert!(
        socket_file(&s).is_none(),
        "no daemon ⇒ no socket was ever created"
    );
}

/// Fix A: a tmux server that dies and RESTARTS at the same socket path must not be silently adopted.
/// The daemon re-checks the startup `#{pid}` on the reconcile path, so a same-path restart exits
/// (releasing socket + lock) rather than carrying stale id-keyed state onto the new instance.
///
/// Staged deterministically: SIGSTOP the daemon, kill + recreate the server underneath it (fresh
/// `#{pid}`, same `#{socket_path}`), then SIGCONT. On resume the next reconcile sees the LIVE new
/// server (never the gap), so only the `#{pid}` re-check stops it adopting the new instance.
#[test]
fn server_restart_is_not_adopted() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let _pane = new_pane(&s, "s1");
    // A status file so we can wait until the daemon is in its serve loop with a control client on
    // the OLD server; a too-early freeze would catch its pre-loop startup reconcile instead.
    let status = s.workdir.join("status");
    let mut daemon = spawn_daemon_args(&s, &["--status-file", status.to_str().unwrap()]);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );
    assert!(
        wait_for_clients(&status, 1, common::POLL_CEILING),
        "daemon must attach a control client to the old server before we restart it"
    );

    // Freeze the daemon so the kill+recreate is invisible to it: without this it might notice the
    // old server gone (a plain ServerGone exit) before the new one is up. On resume, the dead
    // client's EOF drives the reconcile where the `#{pid}` re-check fires against the LIVE new server.
    signal(daemon.pid(), rustix::process::Signal::STOP);
    assert!(s.tmux(&["kill-server"]).status.success());
    let _new_pane = new_pane(&s, "s2");
    signal(daemon.pid(), rustix::process::Signal::CONT);

    assert!(
        daemon.wait_exit(common::POLL_CEILING),
        "daemon must exit on a same-path server restart (Fix A), not adopt the live new server"
    );
    // Clean release: the socket is removed on the server-gone exit path.
    common::poll_until(
        "daemon must remove its socket on the restart-triggered exit",
        || socket_file(&s).is_none(),
    );
}

/// Poll the daemon's introspection status file until its `clients=` count reaches `want`.
fn wait_for_clients(path: &Path, want: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(body) = std::fs::read_to_string(path) {
            for line in body.lines() {
                if let Some(v) = line.strip_prefix("clients=") {
                    if v.trim()
                        .parse::<usize>()
                        .map(|n| n >= want)
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Send a signal to a pid via `rustix::process::kill_process` (the `kill` binary is absent in
/// minimal envs like the nix sandbox).
fn signal(pid: u32, sig: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(pid as i32).expect("valid pid");
    rustix::process::kill_process(pid, sig).unwrap_or_else(|e| panic!("kill {pid:?}: {e}"));
}

/// An event for an agent the daemon has no manifest for is NAKed, and the client falls through to a
/// direct stamp instead of losing it. The daemon runs with an empty `--manifest-dir` while `tma
/// event` uses the bundled corpus (has `claude`), the version-skew shape: without the ack the client
/// would treat the socket write as delivery and the event would vanish.
#[test]
fn unknown_agent_naks_and_client_direct_stamps() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    // An empty manifest dir: the daemon loads zero manifests, so it has no `claude`.
    let empty_manifests = s.workdir.join("empty-manifests");
    std::fs::create_dir_all(&empty_manifests).unwrap();
    let _daemon = spawn_daemon_args(&s, &["--manifest-dir", empty_manifests.to_str().unwrap()]);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );

    // The client HAS the bundled claude manifest (no --manifest-dir). The daemon NAKs (unknown
    // agent) and the client direct-stamps through the shared stamp adapter.
    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", SESSION),
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "a daemon NAK must make the client direct-stamp, not lose the event (Fix B)"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    // A daemon really was up for this: proves the NAK fall-through path, not the no-daemon path.
    assert!(
        socket_file(&s).is_some(),
        "the daemon stayed up; this exercised the NAK path, not daemonless direct-stamp"
    );
}

/// The daemon records its build version alongside its pid, and `tma doctor` reads it back off the
/// same lock file. That is the whole chain behind the version-skew warning: without it a resident
/// daemon older than the CLI is invisible, and `tma reload` (config + manifests only) will not fix it.
#[test]
fn the_daemon_records_its_version_and_doctor_reads_it_back() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let _pane = new_pane(&s, "s1");
    let _daemon = spawn_daemon(&s);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );
    // The pid must be readable first: the version is written in the same body.
    assert!(daemon_pid(&s, common::POLL_CEILING).is_some());

    let lock = lock_file(&s).expect("lock file exists");
    let info = parse_lock(&std::fs::read_to_string(&lock).unwrap()).expect("lock parses");
    assert_eq!(
        info.version.as_deref(),
        Some(VERSION),
        "the running daemon stamps its own build version"
    );

    let out = s
        .command()
        .args(["doctor", "--json", "--socket-name", &s.socket])
        .output()
        .expect("run doctor");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(&format!("\"version\":\"{VERSION}\"")),
        "doctor reports the running daemon's version: {json}"
    );
    assert!(
        json.contains("\"version_matches\":true"),
        "the daemon and this CLI are the same build: {json}"
    );
}

/// Upgrade skew: a resident daemon whose manifests map FEWER events than the client's must NAK the
/// events it cannot resolve, so the client direct-stamps them. Here the daemon loads a `claude`
/// manifest that maps only `SessionStart`; the client uses the bundled corpus, which maps
/// `UserPromptSubmit` to `working`. Acking on the agent-name match alone (the old rule) would let
/// the daemon swallow the event and the pane would never go working until the daemon restarted.
#[test]
fn an_event_the_daemon_cannot_map_naks_and_the_client_direct_stamps() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");

    // The daemon's older-manifest stand-in: `claude` exists, but knows only SessionStart.
    let old_manifests = s.workdir.join("old-manifests");
    std::fs::create_dir_all(&old_manifests).unwrap();
    std::fs::write(
        old_manifests.join("claude.toml"),
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [\"claude\"]\n\
         [hooks]\ncovers = [\"lifecycle\"]\n\
         [[hooks.map]]\nevent = \"SessionStart\"\nclaim = { lifecycle = \"start\" }\n\
         [capture]\n",
    )
    .unwrap();
    let _daemon = spawn_daemon_args(&s, &["--manifest-dir", old_manifests.to_str().unwrap()]);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );

    fire(
        &s,
        "claude",
        "UserPromptSubmit",
        &pane,
        &payload("UserPromptSubmit", SESSION),
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "an event the daemon maps to nothing must NAK so the client applies its own mapping"
    );
    assert_eq!(s.get(&pane, "#{@agent_source}"), "hook");
    assert!(
        socket_file(&s).is_some(),
        "the daemon stayed up; this exercised the NAK path, not daemonless direct-stamp"
    );
}

/// The other half of ack honesty: a deliberate no-write verdict is still an ACK. A foreign session
/// firing while the pane has live subagents is refused by the ownership guard, and that refusal must
/// be acked — a NAK would send the client off to write the very state the daemon just protected the
/// pane from.
#[test]
fn the_subagent_ownership_guard_still_acks_its_refusal() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    let _daemon = spawn_daemon(&s);
    let sock = wait_for_socket(&s, common::POLL_CEILING).expect("daemon must bind");

    // A blocked pane owned by SESSION with one live subagent.
    let at = "1000000000000";
    for (key, val) in [
        ("@agent_name", "claude"),
        ("@agent_state", "blocked"),
        ("@agent_source", "hook"),
        ("@agent_pid", "4242"),
        ("@agent_session", SESSION),
        ("@agent_subagents", "sub-1"),
        ("@agent_since", at),
        ("@agent_evidence_at", at),
        ("@agent_stamped_at", at),
    ] {
        s.set_opt(&pane, key, val);
    }

    // The subagent's own UserPromptSubmit: a `working` claim from a session that does not own the
    // pane. Read the ack byte off the wire directly — the point is the ACK, not the (absent) write.
    let mut stream = UnixStream::connect(&sock).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(&encode_frame(
            &pane,
            "claude",
            "UserPromptSubmit",
            &payload("UserPromptSubmit", "sub-1"),
        ))
        .expect("write the event frame");
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).expect("the daemon answers");
    assert_eq!(
        ack[0], ACK,
        "the ownership guard's refusal is a verdict, so it acks"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "blocked",
        "and the parent pane's state is untouched by the foreign session"
    );
}

/// `tma reload`: with no daemon it exits 0 (clean no-op); with a daemon it finds the pid in the lock
/// file, sends SIGHUP, and the daemon keeps serving (reload path, NOT shutdown). Exercises the whole
/// `tma reload` → `ipc::reload_daemon` → pid-in-lock → SIGHUP chain.
#[test]
fn tma_reload_signals_running_daemon_which_keeps_serving() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");

    // No daemon yet: `tma reload` is a clean no-op success.
    assert!(
        run_reload(&s),
        "`tma reload` with no daemon exits 0 (nothing to reload)"
    );

    let _daemon = spawn_daemon(&s);
    assert!(
        wait_for_socket(&s, common::POLL_CEILING).is_some(),
        "daemon must bind"
    );

    // `tma reload` finds the live daemon (via the pid it wrote to the lock file) and SIGHUPs it.
    assert!(run_reload(&s), "`tma reload` signals the running daemon");

    // The daemon reloaded (not shut down): it still serves a subsequent event.
    fire(
        &s,
        "claude",
        "SessionStart",
        &pane,
        &payload("SessionStart", SESSION),
    );
    assert!(
        wait_for(&s, &pane, "#{@agent_state}", "idle", common::POLL_CEILING),
        "the daemon keeps serving after `tma reload` (SIGHUP is a reload, not a shutdown)"
    );
    // The socket is still bound: a shutdown would have removed it.
    assert!(
        socket_file(&s).is_some(),
        "`tma reload` must not tear down the daemon"
    );
}

fn payload(event: &str, session: &str) -> String {
    format!(r#"{{"session_id":"{session}","hook_event_name":"{event}"}}"#)
}

/// A client that connects and sends NOTHING must not delay another connection's hook event.
/// Before the parked-connection rework the accept loop read each connection synchronously, so K silent
/// clients serialized K x FRAME_DEADLINE (2 s each) before a real frame was even looked at. Here four
/// silent clients are held open, then a real event frame is sent on a separate connection: its
/// delivery ACK must arrive well inside a 1.5 s socket-read budget. Under the old synchronous accept
/// at least one 2 s blocking read sits ahead of the real frame, so the ACK would not arrive in time
/// (the read times out) and this fails. The generous margin (1.5 s vs a ~20 ms normal ACK) keeps it
/// robust on slow CI while still separating the two regimes.
#[test]
fn silent_connection_does_not_delay_a_hook_event() {
    let _gate = common::DaemonTestGuard::acquire();
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new_daemon("daemon");
    let pane = new_pane(&s, "s1");
    let _daemon = spawn_daemon(&s);
    let sock = wait_for_socket(&s, common::POLL_CEILING).expect("daemon must bind");

    // Four clients that connect and then sit silent, held open across the real event below. Under the
    // old synchronous accept each would block the loop for a full FRAME_DEADLINE before yielding.
    let mut silent = Vec::new();
    for _ in 0..4 {
        silent.push(UnixStream::connect(&sock).expect("connect a silent client"));
    }

    // A separate real connection sends a valid claude frame and waits for the daemon's delivery ACK
    // under a bounded read timeout. With parked connections the daemon completes it inline and ACKs
    // near-instantly; a regression to blocking reads would leave it unprocessed past the budget.
    let mut real = UnixStream::connect(&sock).expect("connect the real client");
    real.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    real.set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    let frame = encode_frame(
        &pane,
        "claude",
        "SessionStart",
        &payload("SessionStart", SESSION),
    );
    real.write_all(&frame).expect("write the event frame");

    let mut ack = [0u8; 1];
    let start = Instant::now();
    let got = real.read_exact(&mut ack);
    assert!(
        got.is_ok() && ack[0] == ACK,
        "the daemon must ACK the real frame within 1.5 s despite four silent connections \
         (got {got:?}, elapsed {:?}); a synchronous accept would serialize behind the silent reads",
        start.elapsed(),
    );

    // Keep the silent clients alive until here so they were genuinely pending during the event.
    drop(silent);
}
