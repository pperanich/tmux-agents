//! The event-hub daemon: socket, single-instance lock, wire protocol, control-mode client pool,
//! on-demand capture, and notification dispatch. Strictly additive: with none running `tma event`
//! direct-stamps and every one-shot works unchanged. `tma daemon` runs the loop; `--ensure` is the
//! idempotent launcher and `--restart` puts THIS build in a resident daemon's place (`[daemon]
//! restart_on_upgrade`, on by default, does that automatically, but only ever from a strictly newer
//! build). Socket + lock are keyed by a hash of the server's `#{socket_path}`, which
//! `tma event` recomputes identically, so two servers never share a daemon; an `flock` enforces
//! single-instance. Events apply through the *same* guarded-stamp adapter the direct path uses; a
//! same-`#{socket_path}` restart is caught by a startup-`#{pid}` recheck and exits rather than
//! adopting stale state. SIGHUP hot-reloads config + manifests. No async runtime: a blocking
//! `poll` over the listener, a signal self-pipe, and each client's stdout.
//!
//! The parent holds the `run_cli`/`DaemonOpts` CLI entry and the cross-module `SignalAction`; one
//! submodule per concern: `lifecycle` (the `--ensure` launcher, the detach stages, and foreground
//! lock/socket setup), `serve` (the poll loop, connection dispatch, and hook-event apply), `sys`
//! (rustix os plumbing: dirs, perms, flock, the signal self-pipe), `subscribers` (`tma wait` push
//! fan-out), `pending` (connections parked mid-frame in the poll set so a slow client never
//! serializes the accept path).

use std::path::PathBuf;
use std::process::ExitCode;

use tma_runtime::config::Config;
use tma_runtime::ipc;
use tma_tmux::tmux::Tmux;

mod lifecycle;
mod pending;
mod serve;
mod subscribers;
mod sys;

use lifecycle::{
    ensure_running, evict_only, restart_running, run_foreground, run_intermediate, stop_running,
};
use sys::ensure_dir;

/// Report the user manifests the load skipped. The daemon's log is its stderr (the detached stages
/// send it to `/dev/null`, a service manager captures it), so this is where a broken file surfaces
/// for a daemon that otherwise starts fine on the rest of the set.
fn log_manifest_failures(failures: &[tma_runtime::manifests::ManifestFailure]) {
    for f in failures {
        eprintln!("tma: skipping manifest {}: {}", f.path.display(), f.error);
    }
}

/// What the loop should do after draining the signal self-pipe.
enum SignalAction {
    /// SIGTERM/SIGINT: end the loop.
    Shutdown,
    /// SIGHUP only: reload config + manifests, swap the derived state, keep serving.
    Reload,
    /// A spurious wake with no flag set.
    None,
}

/// Options for `tma daemon [--ensure]`, threaded from the global CLI flags.
pub struct DaemonOpts {
    /// `true` for `--ensure`: spawn a detached daemon if none is running, then exit 0.
    pub ensure: bool,
    /// `true` for `--restart`: stop the daemon running for this server (if any) and start one from
    /// THIS binary. Unconditional in both directions — a deliberate downgrade is a restart from the
    /// older binary. Mutually exclusive with [`Self::ensure`] (clap rejects the pair).
    pub restart: bool,
    /// `true` for `--stop`: stop the daemon for this server and leave it stopped.
    pub stop: bool,
    /// Suppress the eviction path's stderr. Set on the `tma event` route only: a hook's stderr can
    /// surface inside the agent's own UI, so nothing may be written there.
    pub quiet: bool,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    /// The loaded config, used by the foreground loop for the fold + daemon knobs + notify command
    /// + agent set.
    pub config: Config,
    /// The `--config <path>` flag, forwarded to a spawned detached daemon so it loads the same
    /// config file (env `TMA_CONFIG` and the default path are inherited without forwarding).
    pub config_path: Option<PathBuf>,
    /// INTERNAL/TEST: control-pool introspection status file (acceptance probe).
    pub status_file: Option<PathBuf>,
    /// INTERNAL/TEST: force the behavior probe into the useless cross-session config.
    pub probe_cross_session: bool,
    /// INTERNAL/TEST: override the sweep cadence (ms) so the sweep acceptance runs fast without
    /// conflating with the probe degrade path. `None` uses the probe-derived cadence.
    pub sweep_ms: Option<u64>,
    /// INTERNAL: the intermediate detach stage (hidden `--detach-stage2`, set only by the launcher's
    /// spawn). Re-execs the daemon without waiting so it reparents to init, then exits. Never user-set.
    pub detach_stage2: bool,
    /// INTERNAL: the detached daemon (hidden `--detach-session`, set only by the intermediate stage).
    /// Triggers a startup `setsid`; a foreground/service-managed `tma daemon` leaves this false.
    pub detach_session: bool,
    /// INTERNAL/TEST: the build version this daemon stamps into its lock file, instead of its own.
    /// Changes nothing else, so the upgrade-restart guard is exercisable end to end from one build.
    pub fake_version: Option<String>,
    /// INTERNAL/TEST: milliseconds to hold the single-instance lock after unlinking the socket at
    /// shutdown. Widens a gap that is always there so `ipc::stop_daemon_at`'s lock-free half of the
    /// stop condition is observable. Deliberately NOT forwarded to a spawned daemon: the delay
    /// belongs to the instance a test starts by hand, never to its replacement.
    pub shutdown_delay_ms: Option<u64>,
}

/// The replace-only upgrade check (`[daemon] restart_on_upgrade`), against a server socket path the
/// CALLER has already resolved. Replaces a live, strictly older daemon with this build and starts
/// nothing when none is running.
///
/// It takes `socket_path` rather than resolving it because the bin runs this before `tma event`,
/// which needs the same path for its own daemon delivery: resolving is a `tmux display-message`
/// round trip, and a hook fires on every tool call. `run_cli`'s own resolution is deliberately not
/// reused here for that reason. The decision itself stays in one place ([`lifecycle::evict_only`],
/// which `--ensure` also reaches).
///
/// No `ensure_dir`: with no runtime dir there is no lock, and with no lock there is nothing to
/// replace. Best-effort throughout; the caller discards the code.
pub fn evict_older_daemon(socket_path: &str, opts: &DaemonOpts) -> ExitCode {
    evict_only(&ipc::paths_for(socket_path), opts)
}

/// Dispatch `tma daemon`. `--ensure` is the idempotent launcher; without it we run the
/// foreground loop.
pub fn run_cli(opts: DaemonOpts) -> ExitCode {
    // Intermediate detach stage: recognized before any tmux/socket work (it only re-execs the daemon).
    if opts.detach_stage2 {
        return run_intermediate(&opts);
    }
    let tmux = Tmux::connect(&opts.server);
    let Some(socket_path) = ipc::resolve_socket_path(&tmux) else {
        // No server to serve, nothing to do. `--ensure` from outside tmux is a clean no-op
        // rather than an error.
        return ExitCode::SUCCESS;
    };
    let paths = ipc::paths_for(&socket_path);
    if let Err(err) = ensure_dir(&paths.dir) {
        if !opts.quiet {
            eprintln!(
                "tma: cannot create daemon dir {}: {err}",
                paths.dir.display()
            );
        }
        return ExitCode::FAILURE;
    }

    if opts.stop {
        stop_running(&paths)
    } else if opts.restart {
        restart_running(&paths, &opts)
    } else if opts.ensure {
        ensure_running(&paths, &opts)
    } else {
        run_foreground(&tmux, &paths, opts)
    }
}
