use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::{ClearAttentionArgs, JumpArgs, LsArgs, StatusArgs, StatusFormat, SuperviseArgs};
use crate::{cli_support, config, cycle, jump, manifests, surfaces, tmux};

/// Clear the `@agent_attention` flag on the pane you arrive at AND on the one you just left, then
/// nudge any resident `tma watch`. The `after-select-*` hooks call this with `#{pane_id}` on every
/// focus change (as does the picker's Enter-jump): one binary call does all three jobs.
///
/// Arrival alone left the larger residue: an agent finishes while you watch it, you move to another
/// window, and the flag survives on the pane you were just looking at — counted by `tma status` and
/// offered by `prefix-j` until you happen to navigate back. Departure closes that. Walk-away is
/// preserved structurally rather than by a heuristic: walking away means you do not navigate, so no
/// hook fires and nothing clears.
///
/// A focus hook must never error into the user's face, so every failure here is a silent no-op: an
/// empty pane argument, a gone server, an unreadable departed pane.
pub(crate) fn run_clear_attention(args: ClearAttentionArgs, server: &tmux::Server) -> ExitCode {
    let tmux = tmux::Tmux::connect(server);
    // Skipped on an empty pane argument (nothing to target, and nothing to scope the departure
    // query to), but the focus change still happened, so the watcher nudge below runs regardless.
    if !args.pane.is_empty() {
        let _ = tmux.unset_pane_option(&args.pane, tma_core::stamp::opt::ATTENTION);
        // Seen-on-leave. The kind arrives by environment variable, so an older binary reached
        // through the late-bound hook string simply does not find it and keeps its arrival-only
        // behaviour instead of failing to parse.
        if let Some(kind) = std::env::var(crate::install::HOOK_KIND_ENV)
            .ok()
            .and_then(|h| tmux::DepartureKind::from_hook_name(&h))
        {
            if let Ok(Some(departed)) = tmux.departed_pane(&args.pane, kind) {
                let _ = tmux.unset_pane_option(&departed, tma_core::stamp::opt::ATTENTION);
            }
        }
    }
    tma_runtime::nudge::nudge_watchers(&tmux);
    ExitCode::SUCCESS
}

/// `tma supervise` (INTERNAL): drive one detached action to completion via the runtime broker's
/// supervisor. The broker forwards the notify command, but `config.notify.command` is the fallback for
/// a manual invocation. Always exits 0 — a detached action is fire-and-forget; its outcome rides the
/// completion notification, not this exit code.
pub(crate) fn run_supervise(
    args: SuperviseArgs,
    server: &tmux::Server,
    config: config::Config,
) -> ExitCode {
    let tmux = tmux::Tmux::connect(server);
    let notify_sinks = config.notify.sinks();
    tma_runtime::broker::supervise(
        &tmux,
        tma_runtime::broker::SuperviseParams {
            pane_id: args.pane,
            nonce: args.nonce,
            expiry_ms: args.expiry_ms,
            action: args.name,
            agent: args.agent,
            command: args.command,
            detach_timeout_ms: args.detach_timeout_ms,
            notify_command: args.notify_command.or(config.notify.command),
            notify_sinks,
        },
    );
    ExitCode::SUCCESS
}

/// `tma reload`: SIGHUP the per-server daemon (via the tier-2 `ipc` surface) to hot-reload config +
/// manifests. No daemon running is a clean success — the daemon is strictly additive.
pub(crate) fn run_reload(server: &tmux::Server) -> ExitCode {
    use tma_runtime::ipc::{reload_daemon, ReloadOutcome};
    let tmux = tmux::Tmux::connect(server);
    match reload_daemon(&tmux) {
        ReloadOutcome::Signaled => {
            println!("tma: reloaded the daemon's config + manifests");
            ExitCode::SUCCESS
        }
        ReloadOutcome::NotRunning => {
            eprintln!(
                "tma: no daemon running for this server (nothing to reload; one-shots and the \
                 picker reload on their own)"
            );
            ExitCode::SUCCESS
        }
        ReloadOutcome::NoServer => cli_support::no_server(),
        ReloadOutcome::Failed(err) => {
            eprintln!("tma: reload failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Launch a live agent surface that owns config + manifests so its refresh tick can hot-reload them.
/// The picker (default) and `watch` share this shape, differing only in the surface fn and label.
pub(crate) fn run_dashboard_cmd(
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    config: config::Config,
    config_path: Option<PathBuf>,
    client: Option<String>,
    label: &str,
    run: impl FnOnce(
        &tmux::Tmux,
        &tmux::Server,
        config::Config,
        Vec<manifests::LoadedManifest>,
        Option<PathBuf>,
        Option<PathBuf>,
        Option<&str>,
    ) -> std::io::Result<()>,
) -> ExitCode {
    let manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let tmux = tmux::Tmux::connect(server);
    match run(
        &tmux,
        server,
        config,
        manifests,
        config_path,
        manifest_dir,
        client.as_deref(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tma: {label} error: {err}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn run_jump_cmd(
    args: JumpArgs,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    config: &config::Config,
    client: Option<String>,
) -> ExitCode {
    if args.menu {
        return run_jump_menu_cmd(args, server, manifest_dir, config, client);
    }
    let kind = if let Some(pane) = args.pane {
        jump::JumpKind::Pane(pane)
    } else if args.attention {
        jump::JumpKind::Attention
    } else if args.blocked {
        jump::JumpKind::Blocked
    } else if args.back {
        jump::JumpKind::Back
    } else if args.home {
        jump::JumpKind::Home
    } else {
        // `--next` is the default when no target flag is given.
        jump::JumpKind::Next
    };

    let manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let tmux = tmux::Tmux::connect(server);
    match jump::run_jump(
        &tmux,
        &manifests,
        &config.fold_config(),
        kind,
        &args.selector.selector(),
        client.as_deref(),
    ) {
        Ok(outcome) => {
            if outcome.jumped_to.is_none() {
                eprintln!("tma: {}", outcome.message);
            }
            ExitCode::SUCCESS
        }
        Err(tmux::TmuxError::ServerGone) => cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `tma jump --menu`: the tmux-native counterpart of the picker, for a mouse click or a keybinding
/// on a client with no room for a popup. Mirrors `act --menu`'s error shape: a menu needs an
/// attached client, and a render failure is a failure, not a silent no-op.
fn run_jump_menu_cmd(
    args: JumpArgs,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    config: &config::Config,
    client: Option<String>,
) -> ExitCode {
    let manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "tma".to_string());
    let tmux = tmux::Tmux::connect(server);
    match jump::run_jump_menu(
        &tmux,
        &manifests,
        &config.fold_config(),
        &config.picker,
        &args.selector.selector(),
        &bin,
        server,
        client.as_deref(),
    ) {
        Ok(jump::JumpMenuOutcome::Shown(_)) => ExitCode::SUCCESS,
        Ok(jump::JumpMenuOutcome::NoAgents) => {
            eprintln!("tma: no agents");
            ExitCode::SUCCESS
        }
        Ok(jump::JumpMenuOutcome::NoClient) => {
            eprintln!("tma: no attached client to show the jump menu on");
            ExitCode::FAILURE
        }
        Err(tmux::TmuxError::ServerGone) => cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: cannot show the jump menu: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Run one poll cycle for a surface, reporting `--debug-timing` to stderr. The handle comes back
/// with the report so a surface that needs one more read (the JSON rows' server identity) reuses it
/// instead of re-resolving the tmux binary.
fn run_cycle_for(
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    debug_timing: bool,
    config: &config::Config,
) -> Result<(tmux::Tmux, cycle::CycleReport), ExitCode> {
    let manifests =
        cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)?;
    let tmux = tmux::Tmux::connect(server);
    match cycle::run_cycle(&tmux, &manifests, &config.fold_config()) {
        Ok(report) => {
            if debug_timing {
                eprintln!(
                    "tma: cycle {:.1}ms — {} agents, {} produced, {} consumed, {} captures, \
                     {} capture-skipped, {} removed{}",
                    report.elapsed.as_secs_f64() * 1000.0,
                    report.rows.len(),
                    report.produced,
                    report.consumed,
                    report.captures,
                    report.skipped_quiet,
                    report.removed,
                    if report.stampede_skipped {
                        ", stampede-skipped"
                    } else {
                        ""
                    },
                );
            }
            Ok((tmux, report))
        }
        Err(tmux::TmuxError::ServerGone) => Err(cli_support::no_server()),
        Err(err) => {
            eprintln!("tma: {err}");
            Err(ExitCode::FAILURE)
        }
    }
}

pub(crate) fn run_ls(
    args: LsArgs,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    debug_timing: bool,
    config: &config::Config,
) -> ExitCode {
    match run_cycle_for(server, manifest_dir, debug_timing, config) {
        Ok((tmux, mut report)) => {
            // Resolve repo/branch/worktree for the displayed rows here, after the cycle and before
            // render (the memoized, bounded resolver never runs inside `run_cycle`). `tma status`
            // deliberately never annotates — it is the ambient status-line hot path.
            tma_runtime::repo::annotate_rows(&mut report.rows);
            // Narrow AFTER the cycle: every pane is stamped, only the printed set shrinks.
            if let Some(pane) = &args.pane {
                report.rows.retain(|r| &r.pane_id == pane);
            }
            args.selector.selector().retain(&mut report.rows);
            if args.json {
                // One resolve for the whole invocation, repeated onto every row.
                let origin = tma_runtime::origin::Origin::resolve(&tmux);
                println!("{}", surfaces::render_ls_json(&report, &origin));
            } else {
                print!("{}", surfaces::render_ls_text(&report));
            }
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

pub(crate) fn run_status(
    args: StatusArgs,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    debug_timing: bool,
    config: &config::Config,
) -> ExitCode {
    match run_cycle_for(server, manifest_dir, debug_timing, config) {
        // `status` never serializes rows, so it needs no server/host resolve and no tmux handle.
        Ok((_tmux, mut report)) => {
            // The counts are over the selected rows only; the cycle above still stamped every pane,
            // so a per-session status driver stays a full ambient driver. Repo labels are resolved
            // only when the selector needs them — an unscoped status stays the spawn-free hot path.
            let selector = args.selector.selector();
            if selector.needs_repo() {
                tma_runtime::repo::annotate_rows(&mut report.rows);
            }
            selector.retain(&mut report.rows);
            match args.format {
                // A trailing newline would widen the status segment; print the bare string. `plain`
                // shares that rule — it feeds the same kind of one-line bar segment.
                StatusFormat::Tmux => {
                    print!("{}", surfaces::render_status(&report, &config.status))
                }
                StatusFormat::Plain => {
                    print!("{}", surfaces::render_status_plain(&report, &config.status))
                }
                StatusFormat::Json => println!("{}", surfaces::render_status_json(&report)),
                // The exposition already ends in a newline (the format requires it).
                StatusFormat::Prom => print!(
                    "{}",
                    surfaces::render_status_prom(&report, tma_runtime::now_ms())
                ),
            }
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}
