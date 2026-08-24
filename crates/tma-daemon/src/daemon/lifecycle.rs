//! Daemonization and process lifecycle: the `--ensure` launcher, the two detach stages, and the
//! foreground lock/socket/signal setup that hands off to [`serve`]. Cleanup runs on every exit path.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rustix::io::Errno;

use tma_runtime::ipc::{self, Paths, RestartDecision, StopOutcome};
use tma_runtime::manifests;
use tma_tmux::tmux::Tmux;

use super::serve::serve;
use super::sys::{cleanup, flock_nb, install_signal_pipe, set_mode_0600};
use super::{log_manifest_failures, DaemonOpts};

/// How long the restart paths wait for a freshly spawned daemon to answer before saying so. The
/// spawn is otherwise fire-and-forget, but a verb called "restart" that reports success while
/// nothing is listening is a lie the user has no way to check.
const UP_TIMEOUT: Duration = Duration::from_secs(2);

/// `--ensure`: if a daemon already holds the lock, no-op; else spawn a detached daemon. The child
/// re-acquires the lock (the authoritative guard); this probe only avoids a doomed spawn when running.
pub(super) fn ensure_running(paths: &Paths, opts: &DaemonOpts) -> ExitCode {
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
    {
        Ok(lock) => {
            if flock_nb(&lock) {
                // We got the lock ⇒ no daemon is running. Release it (dropping the fd), then
                // spawn the detached daemon, which takes the lock for real.
                drop(lock);
                if !spawn_detached(opts) {
                    // The intermediate stage returned nonzero: the daemon never spawned. Surface it
                    // to a manual `--ensure` (autostart discards the code, so it stays best-effort).
                    eprintln!("tma: failed to spawn the detached daemon");
                    return ExitCode::FAILURE;
                }
            } else if opts.config.daemon.restart_on_upgrade {
                // A daemon holds the lock. Opt-in only: replace it if this build is strictly newer.
                drop(lock);
                evict_older(paths, opts);
            }
            // else: lock held ⇒ a daemon is already up ⇒ idempotent no-op.
            ExitCode::SUCCESS
        }
        // Best-effort launcher: an unwritable lock path is not worth failing a hook over.
        Err(_) => ExitCode::SUCCESS,
    }
}

/// `[daemon] restart_on_upgrade`: on the branch where a daemon already holds the lock, replace it if
/// and only if this build is STRICTLY NEWER than the one it recorded. The whole rule (and the
/// reasoning behind its asymmetry) lives in [`ipc::restart_decision`]; this is the effecting half.
///
/// Best-effort and silent when it declines: `--ensure` runs on every hook and, with autostart on,
/// before every surface, so it must stay a no-op that costs one lock read.
fn evict_older(paths: &Paths, opts: &DaemonOpts) {
    let RestartDecision::Evict { pid } =
        ipc::upgrade_restart_decision(paths, ipc::VERSION, tma_runtime::now_ms())
    else {
        return;
    };
    // Recorded before the signal: an eviction that fails to bring a daemon back still counts
    // against the cooldown, which is exactly the case the cooldown exists for.
    ipc::note_restart(paths, tma_runtime::now_ms());
    match ipc::stop_daemon_at(paths) {
        StopOutcome::Failed(err) => {
            eprintln!("tma: cannot replace the older daemon (pid {pid}): {err}");
        }
        // Stopped, or it had already gone on its own; either way nothing holds this server now.
        StopOutcome::Stopped | StopOutcome::NotRunning => {
            if !spawn_detached(opts) {
                eprintln!("tma: stopped the older daemon but could not spawn its replacement");
            }
        }
    }
}

/// What a wedged daemon leaves behind, said plainly. [`ipc::stop_daemon_at`] reports a timeout only
/// AFTER its SIGTERM went out, so "cannot stop the running daemon" on its own reads as "nothing
/// changed" when in fact the signal stands and the daemon will exit the moment it unwedges. With
/// `autostart` off (the default) nothing then brings one back, so the user needs the follow-up.
fn report_stop_timeout(err: &str) {
    eprintln!("tma: cannot stop the running daemon: {err}");
    eprintln!(
        "     The SIGTERM has been delivered and stands, so it may still exit once it unwedges — \
         this is not\n     \"nothing changed\". Once it is gone, `tma daemon --ensure` starts one."
    );
}

/// `tma daemon --stop`: stop the daemon for this server and leave it stopped. The counterpart to
/// `--restart` for the case where you want the daemon gone rather than replaced — detection falls
/// back to the poll tier, which is strictly additive, so nothing breaks. Nothing running is a clean
/// exit 0, the same no-op discipline `reload` keeps (`reload` puts its no-op line on stderr; both
/// no-ops are exit 0, which is the part that matters to a script).
pub(super) fn stop_running(paths: &Paths) -> ExitCode {
    match ipc::stop_daemon_at(paths) {
        StopOutcome::Failed(err) => {
            report_stop_timeout(&err);
            ExitCode::FAILURE
        }
        StopOutcome::NotRunning => {
            println!("tma: no daemon was running for this server");
            ExitCode::SUCCESS
        }
        StopOutcome::Stopped => {
            println!("tma: stopped the running daemon; detection is on the poll tier until one starts again");
            ExitCode::SUCCESS
        }
    }
}

/// `--restart`: stop whatever daemon this server has and bring THIS build up in its place.
///
/// Unconditional and direction-free, unlike the automatic path: an older binary being asked to
/// restart obviously wants the older daemon, which is how a deliberate downgrade is served. Not
/// starting one at all would be a second verb for a job `--ensure` already does, so a restart with
/// nothing running just starts one.
pub(super) fn restart_running(paths: &Paths, opts: &DaemonOpts) -> ExitCode {
    match ipc::stop_daemon_at(paths) {
        StopOutcome::Failed(err) => {
            report_stop_timeout(&err);
            return ExitCode::FAILURE;
        }
        StopOutcome::NotRunning => println!("tma: no daemon was running for this server"),
        StopOutcome::Stopped => println!("tma: stopped the running daemon"),
    }
    if !spawn_detached(opts) {
        eprintln!("tma: failed to spawn the detached daemon");
        return ExitCode::FAILURE;
    }
    if await_daemon_up(paths) {
        println!("tma: daemon restarted ({})", ipc::VERSION);
        ExitCode::SUCCESS
    } else {
        // A failure, not a slow start. The control-mode behaviour probe that can take seconds runs
        // INSIDE `serve`, after `UnixListener::bind`, and `daemon_answers` is a connect that
        // succeeds off the listen backlog within tens of milliseconds — so nothing answering after
        // [`UP_TIMEOUT`] means the replacement did not come up (a failed bind is the usual cause).
        // Exit 0 here would tell `offer_daemon_restart` the skew was resolved, and
        // `tma install-hooks --yes` would report a clean install over a dead daemon.
        eprintln!("tma: the daemon was launched but never answered on its socket; nothing is running for this server");
        ExitCode::FAILURE
    }
}

/// Wait out [`UP_TIMEOUT`] for a daemon to start answering on this server's socket.
fn await_daemon_up(paths: &Paths) -> bool {
    let deadline = Instant::now() + UP_TIMEOUT;
    loop {
        if ipc::daemon_answers(paths) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Build `tma daemon <forwarded args>` on THIS exe (socket/manifest/status/probe/sweep/config): the
/// shared spine both detach stages spawn. The caller sets stdio and the stage flag. `None` iff
/// `current_exe` is unreadable (best-effort; the launcher retries on the next hook).
fn build_daemon_command(opts: &DaemonOpts) -> Option<std::process::Command> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon");
    opts.server.forward_to(&mut cmd);
    if let Some(dir) = &opts.manifest_dir {
        cmd.arg("--manifest-dir").arg(dir);
    }
    if let Some(path) = &opts.status_file {
        cmd.arg("--status-file").arg(path);
    }
    if opts.probe_cross_session {
        cmd.arg("--probe-cross-session");
    }
    if let Some(ms) = opts.sweep_ms {
        cmd.arg("--sweep-ms").arg(ms.to_string());
    }
    if let Some(path) = &opts.config_path {
        cmd.arg("--config").arg(path);
    }
    if let Some(version) = &opts.fake_version {
        cmd.arg("--fake-version").arg(version);
    }
    Some(cmd)
}

/// Launcher (`tma daemon --ensure`): spawn the intermediate detach stage (this exe, `--detach-stage2`,
/// null stdio) and `wait` it. The intermediate re-execs the daemon and exits at once, so this returns
/// promptly with no defunct child. Its exit status is the daemon-spawn verdict: `true` iff status 0.
/// The daemon takes the flock for real in `run_foreground`.
fn spawn_detached(opts: &DaemonOpts) -> bool {
    let Some(mut cmd) = build_daemon_command(opts) else {
        return false;
    };
    cmd.arg("--detach-stage2")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => child.wait().map(|s| s.success()).unwrap_or(false),
        Err(_) => false,
    }
}

/// Intermediate detach stage (`--detach-stage2`): re-exec the daemon (this exe, `--detach-session`,
/// null stdio) but NEVER wait it, then exit — dropping `Child` neither kills nor reaps, so when this
/// process exits the daemon reparents to init, which reaps it (the no-zombie property the second fork
/// gave). Exit 0 iff the daemon spawned; nonzero surfaces to the waiting launcher.
pub(super) fn run_intermediate(opts: &DaemonOpts) -> ExitCode {
    let Some(mut cmd) = build_daemon_command(opts) else {
        return ExitCode::FAILURE;
    };
    cmd.arg("--detach-session")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(_child) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// The foreground event loop: acquire the single-instance lock, bind the socket, and serve until a
/// signal or a gone server. Cleanup (socket removal, flock release) happens on every exit path.
pub(super) fn run_foreground(tmux: &Tmux, paths: &Paths, opts: DaemonOpts) -> ExitCode {
    // Detached path only (`--detach-session`, set by the intermediate stage), before any child or
    // socket: start a new session so the launcher's shell exiting cannot signal us. EPERM = already a
    // session leader (harmless). A foreground/service-managed `tma daemon` keeps its supervisor's session.
    if opts.detach_session {
        if let Err(err) = rustix::process::setsid() {
            if err != Errno::PERM {
                eprintln!("tma: setsid failed: {err}");
            }
        }
    }
    // Own the reload inputs before the config is moved into `serve`: a SIGHUP re-reads the SAME
    // config path + manifest dir the startup load used, so the paths must outlive the loop.
    let manifest_dir = opts.manifest_dir.clone();
    let config_path = opts.config_path.clone();
    let status_file = opts.status_file.clone();
    let probe_cross_session = opts.probe_cross_session;
    let sweep_ms = opts.sweep_ms;
    let shutdown_delay_ms = opts.shutdown_delay_ms;
    // Single-instance lock. Held for the process's life; released automatically on exit
    // or crash, so a stale lock is always reclaimable.
    let lock = match claim_lock(&paths.lock) {
        Ok(Some(f)) => f,
        Ok(None) => {
            // Another daemon owns this server. Not an error: exit cleanly (matches `--ensure`).
            eprintln!("tma: daemon already running for this server");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("tma: cannot open daemon lock: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Record our pid and build in the flock-held lock file so `tma reload` (`ipc::reload_daemon`)
    // can find this daemon to signal it and the upgrade check can rank it. Best-effort: both
    // degrade to a message if unreadable.
    write_pid(&lock, opts.fake_version.as_deref().unwrap_or(ipc::VERSION));

    let config = opts.config;
    let manifests = match manifests::load(manifest_dir.as_deref(), &config.agent_overrides) {
        Ok(set) => {
            log_manifest_failures(&set.failures);
            set.manifests
        }
        Err(err) => {
            eprintln!("tma: manifest load failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Bind the socket, clearing any stale file a crashed predecessor left behind (its flock
    // is already gone, so we legitimately own this server now).
    let _ = std::fs::remove_file(&paths.socket);
    let listener = match UnixListener::bind(&paths.socket) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("tma: cannot bind {}: {err}", paths.socket.display());
            return ExitCode::FAILURE;
        }
    };
    // Local user only: the containing dir is 0700, and the socket itself 0600 (events accepted
    // from the local user only). Non-fatal if the chmod fails.
    set_mode_0600(&paths.socket);
    if listener.set_nonblocking(true).is_err() {
        eprintln!("tma: cannot set socket non-blocking");
        cleanup(&paths.socket);
        return ExitCode::FAILURE;
    }

    let sig_read = match install_signal_pipe() {
        Some(fd) => fd,
        None => {
            eprintln!("tma: cannot install signal handler");
            cleanup(&paths.socket);
            return ExitCode::FAILURE;
        }
    };

    serve(
        tmux,
        &listener,
        manifests,
        sig_read,
        status_file.as_deref(),
        probe_cross_session,
        sweep_ms,
        config,
        config_path.as_deref(),
        manifest_dir.as_deref(),
    );

    // Explicit cleanup; `lock` drops here, releasing the flock. The order is load-bearing and is
    // why `ipc::stop_daemon_at` waits for the LOCK rather than for the socket to go quiet: between
    // the two lines this daemon is unreachable and still owns the server, so a replacement spawned
    // in the gap exits as a duplicate and leaves nothing running at all.
    cleanup(&paths.socket);
    // INTERNAL/TEST (`--shutdown-delay-ms`): widen exactly that gap so the wait is observable.
    if let Some(ms) = shutdown_delay_ms {
        std::thread::sleep(Duration::from_millis(ms));
    }
    drop(lock);
    ExitCode::SUCCESS
}

/// Open the per-server lock and take the single-instance flock, emptying the file the instant it is
/// ours. `Ok(None)` when another daemon holds it.
///
/// The truncation is not tidiness, it is the correctness half. A lock file keeps its body after the
/// daemon that wrote it exits — only the flock is released — so between taking the lock and
/// stamping our own pid the file still describes our PREDECESSOR. A reader that finds the lock held
/// and reads that body sees a stale version belonging to a dead (possibly recycled) pid, and the
/// upgrade check would act on it: evicting a brand-new, correct daemon, or signalling whatever now
/// owns that pid. Truncating here makes the window read as EMPTY instead, which every reader
/// already treats as "unknown, do nothing". Nothing may write to the lock between here and
/// [`write_pid`].
fn claim_lock(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    if !flock_nb(&lock) {
        return Ok(None);
    }
    let _ = lock.set_len(0);
    Ok(Some(lock))
}

/// Record the daemon's pid and build `version` in the flock-held lock file: the pid so `tma reload`
/// can find it to send a signal, the version so `tma doctor` and the upgrade check can tell a
/// resident daemon from the CLI talking to it. Best-effort. Writes from offset 0 into the file
/// [`claim_lock`] just emptied, which is the only state it is ever called in.
fn write_pid(lock: &std::fs::File, version: &str) {
    let body = ipc::render_lock(std::process::id(), version);
    let _ = (&mut &*lock).write_all(body.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_lock(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tma_lifecycle_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("server.lock")
    }

    /// The window the upgrade check would otherwise act on: a lock file still describing the
    /// PREVIOUS daemon while a new, correct one already holds the flock. Taking the lock must empty
    /// it, so a reader in that window parses nothing (⇒ unknown ⇒ leave the daemon alone) rather
    /// than a version that is a lie about a dead pid.
    #[test]
    fn taking_the_lock_empties_the_previous_daemons_body() {
        let path = scratch_lock("claim");
        // A dead predecessor's leftovers: the body survives its exit, only the flock is released.
        std::fs::write(&path, ipc::render_lock(4242, "0.0.1")).unwrap();
        assert!(ipc::parse_lock(&std::fs::read_to_string(&path).unwrap()).is_some());

        let lock = claim_lock(&path).expect("open the lock").expect("take it");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "the body must be gone the instant the flock is ours, before any pid is stamped"
        );
        assert!(
            ipc::parse_lock(&std::fs::read_to_string(&path).unwrap()).is_none(),
            "and an empty body reads as no lock info at all"
        );

        // The stamp then lands at offset 0: exactly the body, with nothing of the predecessor left.
        write_pid(&lock, "9.9.9");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, ipc::render_lock(std::process::id(), "9.9.9"));
        let info = ipc::parse_lock(&body).expect("the stamped body parses");
        assert_eq!(info.pid, std::process::id() as i32);
        assert_eq!(info.version.as_deref(), Some("9.9.9"));

        // Release the flock BEFORE unlinking: a lock held on a deleted inode outlives the file.
        drop(lock);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A second claim while the first is held declines instead of truncating: the running daemon's
    /// recorded pid must survive a would-be duplicate's attempt to start.
    #[test]
    fn a_second_claim_declines_and_leaves_the_body_alone() {
        let path = scratch_lock("dup");
        let held = claim_lock(&path).expect("open").expect("take");
        write_pid(&held, "1.2.3");
        let before = std::fs::read_to_string(&path).unwrap();

        assert!(
            claim_lock(&path).expect("open").is_none(),
            "the flock is held, so the second daemon must decline"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a declined claim must not empty the running daemon's body"
        );

        // Reclaim-after-release is deliberately NOT asserted here. Measured on macOS: an flock
        // released by closing the fd is not always visible as free to an immediate re-lock in the
        // same process — a retry microseconds later succeeds. That is why `ipc::stop_daemon_at`
        // waits for the lock in a bounded poll rather than probing once. `single_instance_flock`
        // covers reclaim across processes, which is the case that actually matters.
        drop(held);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
