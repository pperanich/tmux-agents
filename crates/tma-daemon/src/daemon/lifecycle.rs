//! Daemonization and process lifecycle: the `--ensure` launcher, the two detach stages, and the
//! foreground lock/socket/signal setup that hands off to [`serve`]. Cleanup runs on every exit path.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

use rustix::io::Errno;

use tma_runtime::ipc::{self, Paths};
use tma_runtime::manifests;
use tma_tmux::tmux::Tmux;

use super::serve::serve;
use super::sys::{cleanup, flock_nb, install_signal_pipe, set_mode_0600};
use super::{log_manifest_failures, DaemonOpts};

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
            }
            // else: lock held ⇒ a daemon is already up ⇒ idempotent no-op.
            ExitCode::SUCCESS
        }
        // Best-effort launcher: an unwritable lock path is not worth failing a hook over.
        Err(_) => ExitCode::SUCCESS,
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
    // Single-instance lock. Held for the process's life; released automatically on exit
    // or crash, so a stale lock is always reclaimable.
    let lock = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!("tma: cannot open daemon lock: {err}");
            return ExitCode::FAILURE;
        }
    };
    if !flock_nb(&lock) {
        // Another daemon owns this server. Not an error: exit cleanly (matches `--ensure`).
        eprintln!("tma: daemon already running for this server");
        return ExitCode::SUCCESS;
    }
    // Record our pid in the flock-held lock file so `tma reload` (`ipc::reload_daemon`) can find
    // this daemon to send it SIGHUP. Best-effort: reload degrades to a message if unreadable.
    write_pid(&lock);

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

    // Explicit cleanup; `lock` drops here, releasing the flock.
    cleanup(&paths.socket);
    drop(lock);
    ExitCode::SUCCESS
}

/// Record the daemon's pid and build version in the flock-held lock file: the pid so `tma reload`
/// can find it to send SIGHUP, the version so `tma doctor` can spot a resident daemon older than the
/// CLI talking to it. Best-effort; truncates first so a shorter body never leaves stale bytes.
fn write_pid(lock: &std::fs::File) {
    let _ = lock.set_len(0);
    let body = ipc::render_lock(std::process::id(), ipc::VERSION);
    let _ = (&mut &*lock).write_all(body.as_bytes());
}
