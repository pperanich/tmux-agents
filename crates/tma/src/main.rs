#![deny(rustdoc::broken_intra_doc_links)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod act;
mod cli;
mod cli_support;
mod completions;
mod debug_cmd;
mod dispatch;
mod doctor;
mod init;
mod install;
mod install_keys;
mod mute;
mod redact;
mod subscribe;
mod wait;
mod watch_session;

// Re-import the tmux/stamp modules at the crate root so the bin's `crate::tmux` / `crate::stamp`
// paths keep resolving to the one crate that spawns tmux.
use tma_tmux::{stamp, tmux};
// Re-import the tier-2 pipeline modules the bin drives (`identity`/`capture` have no bin consumer);
// the tier-3 `tma daemon` subcommand dispatches into the separate `tma_daemon` crate.
use tma_runtime::{config, cycle, debug, event, ipc, json, manifests};
// The display layer (picker, jump, ls/status surfaces) lives in `tma-ui`; `ansi` is internal to it.
use tma_ui::{jump, picker, surfaces, watch};

use cli::{Cli, Command};
use cli_support::{DaemonLauncher, DaemonMode};
use debug_cmd::run_debug;
use dispatch::{
    run_clear_attention, run_dashboard_cmd, run_jump_cmd, run_ls, run_reload, run_status,
    run_supervise,
};

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The targeting/diagnostic flags are clap globals on `Cli` (one canonical field regardless of
    // position). Destructure once and thread into every handler; no per-subcommand copy shadows them
    // (a local `--client` on jump/watch silently shadowing the top-level one was the footgun this replaces).
    let manifest_dir = cli.manifest_dir;
    let debug_timing = cli.debug_timing;
    let client = normalize_client(cli.client);
    // Load the config once, then thread it everywhere. An absent/partial file is the zero-config
    // floor; a malformed one fails loudly — EXCEPT for `tma event`, whose `tma-hook` wrapper swallows
    // the exit, so a fail-fast there would let one typo silently disable all hook state tracking. For
    // `event` only we degrade to defaults with a stderr warning; every other subcommand fails loudly.
    let config = match config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(err) if matches!(cli.command, Some(Command::Event(_))) => {
            eprintln!("tma: {err}; using default config for this hook event");
            config::Config::default()
        }
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    // The tmux target: the socket selector from the globals, and the binary from config + env (both
    // resolved only now, since the config had to load first).
    let server = resolve_server(cli.socket_name, cli.socket_path, &config);
    // Opt-in auto-start (`[daemon] autostart = true`, default false): for a user-invoked surface,
    // bring the daemon up via the idempotent `--ensure` spawn before the surface runs. The spawn
    // result is discarded so a failed launch never fails or delays the command (the daemon is
    // strictly additive); `autostart_eligible` excludes `event` and the management/diagnostic verbs.
    if config.daemon.autostart && autostart_eligible(&cli.command) {
        let _ = run_daemon_verb(
            &config,
            server.clone(),
            manifest_dir.clone(),
            cli.config.clone(),
            DaemonMode::Ensure,
        );
    }
    // The target server's `#{socket_path}`, resolved AT MOST ONCE for this invocation. Two things
    // below key on it (the upgrade check, and `tma event`'s daemon delivery) and resolving it is a
    // `tmux display-message` round trip. A hook fires on every tool call, so the second one is a
    // cost worth removing; `None` here means no server, which both consumers already handle.
    let is_event = matches!(cli.command, Some(Command::Event(_)));
    // `tma event --kind context` is the exception among hook events: that lane stamps the gauge
    // directly and never speaks to the daemon, so it wants no socket path of its own.
    let event_delivers = matches!(
        &cli.command,
        Some(Command::Event(args)) if args.kind != event::CONTEXT_KIND
    );
    let runs_upgrade_check =
        config.daemon.restart_on_upgrade && upgrade_check_eligible(&cli.command);
    let server_socket = (runs_upgrade_check || event_delivers)
        .then(|| ipc::resolve_socket_path(&tmux::Tmux::connect(&server)))
        .flatten();
    // The upgrade check (`[daemon] restart_on_upgrade`, on by default), independent of `autostart`:
    // an upgraded tma replaces the older daemon it finds rather than leaving a stale build serving
    // until the tmux server next restarts. Replace-only (a user with no daemon running sees
    // nothing change) and best-effort, so the exit code is discarded like autostart's.
    if runs_upgrade_check {
        if let Some(socket_path) = server_socket.as_deref() {
            let _ = tma_daemon::evict_older_daemon(
                socket_path,
                &daemon_opts_for_check(&config, &server, &manifest_dir, &cli.config, is_event),
            );
        }
    }
    match cli.command {
        // No subcommand → the picker is the default.
        None => run_dashboard_cmd(
            &server,
            manifest_dir,
            config,
            cli.config,
            client,
            "picker",
            picker::run_picker,
        ),
        Some(Command::Version) => {
            println!("tma {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(Command::Ls(args)) => run_ls(args, &server, manifest_dir, debug_timing, &config),
        Some(Command::Status(args)) => {
            run_status(args, &server, manifest_dir, debug_timing, &config)
        }
        Some(Command::Jump(args)) => run_jump_cmd(args, &server, manifest_dir, &config, client),
        Some(Command::Wait(args)) => wait::run(wait::WaitOpts {
            target_pane: args.pane,
            target_any: args.any,
            selector: args.selector.selector(),
            target_all: args.all,
            target_count: args.count,
            until: args.until,
            since: args.since,
            timeout: args.timeout,
            json: args.json,
            server: server.clone(),
            manifest_dir,
            config,
            config_path: cli.config,
        }),
        Some(Command::Subscribe(args)) => subscribe::run(subscribe::SubscribeOpts {
            json: args.json,
            interval: args.interval,
            changes_only: args.changes_only,
            events: args.events,
            selector: args.selector.selector(),
            server: server.clone(),
            manifest_dir,
            config,
            config_path: cli.config,
        }),
        Some(Command::Act(args)) => act::run(act::ActOpts {
            name: args.name,
            pane: args.pane,
            selector: args.selector.selector(),
            all: args.all,
            dry_run: args.dry_run,
            args: args.args,
            force: args.force,
            yes: args.yes,
            json: args.json,
            list: args.list,
            menu: args.menu,
            server: server.clone(),
            manifest_dir,
            config,
        }),
        Some(Command::Mute(args)) => mute::run(mute::MuteOpts {
            pane: args.pane,
            selector: args.selector.selector(),
            for_ms: args.for_ms,
            clear: args.clear,
            server: server.clone(),
            manifest_dir,
            config,
        }),
        Some(Command::Watch(args)) if args.temporary_session => {
            watch_session::run(watch_session::WatchSessionOpts {
                args,
                server,
                manifest_dir,
                config_path: cli.config,
                client,
            })
        }
        Some(Command::Watch(args)) => {
            let start_table = args.table;
            let exit_on_jump = args.exit_on_jump;
            let origin_pane = args.origin_pane;
            let selector = args.selector.selector();
            run_dashboard_cmd(
                &server,
                manifest_dir,
                config,
                cli.config,
                client,
                "watch",
                move |tmux, server, config, manifests, config_path, manifest_dir, client| {
                    watch::run_watch(
                        tmux,
                        server,
                        config,
                        manifests,
                        config_path,
                        manifest_dir,
                        client,
                        start_table,
                        exit_on_jump,
                        origin_pane.as_deref(),
                        selector,
                    )
                },
            )
        }
        Some(Command::Event(args)) => event::run(event::EventArgs {
            agent: args.agent,
            kind: args.kind,
            pane: args.pane,
            payload: args.payload,
            server: server.clone(),
            server_socket,
            manifest_dir,
            notify_from_event: config.notify.from_event,
            notify_commands: config.notify.commands(),
            notify_sinks: config.notify.sinks(),
            notify_on: config.notify.on,
            notify_context_high: config.notify.context_high.as_ref().map(|c| c.threshold),
            agents: config.agent_overrides,
        }),
        Some(Command::Daemon(args)) => tma_daemon::run_cli(tma_daemon::DaemonOpts {
            ensure: args.ensure,
            restart: args.restart,
            stop: args.stop,
            // A typed `tma daemon` always has a terminal to report to.
            quiet: false,
            server: server.clone(),
            manifest_dir,
            config_path: cli.config,
            config,
            status_file: args.status_file,
            probe_cross_session: args.probe_cross_session,
            sweep_ms: args.sweep_ms,
            detach_stage2: args.detach_stage2,
            detach_session: args.detach_session,
            fake_version: args.fake_version,
            shutdown_delay_ms: args.shutdown_delay_ms,
        }),
        Some(Command::Init(args)) => {
            let launch_daemon = daemon_launcher(
                &config,
                server.clone(),
                manifest_dir.clone(),
                cli.config.clone(),
            );
            init::run(init::InitOpts {
                assume_yes: args.yes,
                daemon: args.daemon,
                no_daemon: args.no_daemon,
                config_dir: args.config_dir,
                conf: args.conf,
                server: server.clone(),
                manifest_dir,
                config,
                launch_daemon,
            })
        }
        Some(Command::InstallHooks(args)) => {
            let launch_daemon = daemon_launcher(
                &config,
                server.clone(),
                manifest_dir.clone(),
                cli.config.clone(),
            );
            install::run(install::InstallOpts {
                statusline: match (args.statusline, args.no_statusline) {
                    (true, _) => install::Statusline::Install,
                    (_, true) => install::Statusline::Remove,
                    _ => install::Statusline::Keep,
                },
                agent: args.agent,
                all: args.all,
                uninstall: args.uninstall,
                check: args.check,
                assume_yes: args.yes,
                server: server.clone(),
                manifest_dir,
                settings: args.settings,
                gemini_settings: args.gemini_settings,
                config_dir: args.config_dir,
                wrapper_path: args.wrapper_path,
                wrapper_ref: args
                    .wrapper_ref
                    .map_or(config.install.wrapper_ref, Into::into),
                opencode_plugin: args.opencode_plugin,
                codex_config: args.codex_config,
                codex_hooks: args.codex_hooks,
                cursor_hooks: args.cursor_hooks,
                cursor_cli_config: args.cursor_cli_config,
                pi_extension: args.pi_extension,
                focus_events: config.focus.events,
                agents: config.agent_overrides,
                launch_daemon: Some(launch_daemon),
            })
        }
        Some(Command::InstallKeys(args)) => install_keys::run(install_keys::InstallKeysOpts {
            uninstall: args.uninstall,
            check: args.check,
            mouse: args.mouse,
            daemon: !args.no_daemon,
            assume_yes: args.yes,
            conf: args.conf,
            config_dir: args.config_dir,
        }),
        Some(Command::Doctor(args)) => doctor::run(doctor::DoctorOpts {
            json: args.json,
            exit_code: args.exit_code,
            server: server.clone(),
            manifest_dir,
            focus_events: config.focus.events,
            windows: config.telemetry.windows.clone(),
            api: config.api.clone(),
            agents: config.agent_overrides,
            wrapper_ref: config.install.wrapper_ref,
        }),
        Some(Command::Reload) => run_reload(&server),
        Some(Command::ClearAttention(args)) => run_clear_attention(args, &server),
        Some(Command::Supervise(args)) => run_supervise(args, &server, config),
        Some(Command::Completions(args)) => completions::run(args),
        Some(Command::Debug(args)) => run_debug(args, &server, manifest_dir, &config),
    }
}

/// Resolve the target tmux server from the two global socket flags plus `TMA_SOCKET_PATH`.
///
/// Precedence mirrors `--config`/`TMA_CONFIG`: an explicit flag wins, the env var is only the
/// fallback when neither socket flag was given. clap has already rejected passing both flags (exit
/// 2), so at most one is set here; an empty `TMA_SOCKET_PATH` reads as unset rather than as a
/// nonsense empty path.
/// The binary comes from `TMA_TMUX_BIN` first, then `[tmux] bin`, then plain `tmux`: the env wins so
/// one shell can be pointed at another tmux without editing config.
fn resolve_server(
    socket_name: Option<String>,
    socket_path: Option<PathBuf>,
    config: &config::Config,
) -> tmux::Server {
    let bin = std::env::var("TMA_TMUX_BIN")
        .ok()
        .filter(|b| !b.is_empty())
        .or_else(|| config.tmux.bin.clone());
    if socket_name.is_some() || socket_path.is_some() {
        return tmux::Server {
            socket_name,
            socket_path,
            bin,
        };
    }
    tmux::Server {
        socket_name: None,
        socket_path: std::env::var_os("TMA_SOCKET_PATH")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
        bin,
    }
}

/// Normalize the global `--client`: empty or still-unexpanded reads as absent.
///
/// tmux format-expands a shell-command only in some contexts (`run-shell` does; `display-popup`
/// does not), so a binding written with `--client "#{client_name}"` can deliver the literal format
/// instead of a client name. `switch-client -c '#{client_name}'` can only fail, while the targetless
/// resolution the `None` path uses succeeds from inside the popup — so treat the literal as absent.
fn normalize_client(client: Option<String>) -> Option<String> {
    client.filter(|c| !c.trim().is_empty() && !c.contains("#{"))
}

/// Which subcommands trigger auto-start: the user-invoked surfaces only. `event` and the
/// management/diagnostic verbs are inert (they manage or inspect the daemon, not consume it).
fn autostart_eligible(command: &Option<Command>) -> bool {
    matches!(
        command,
        None | Some(
            Command::Ls(_)
                | Command::Status(_)
                | Command::Jump(_)
                | Command::Wait(_)
                | Command::Watch(_)
                | Command::Subscribe(_)
        )
    )
}

/// Which subcommands run the replace-only upgrade check: everything [`autostart_eligible`] covers,
/// plus `event`.
///
/// `event` is included precisely because it is not a surface. It is the one tma command a hook
/// fires unprompted, so on a machine where nobody types `tma` it is the only thing that ever
/// notices the daemon is a build behind. The management and diagnostic verbs stay out for the same
/// reason autostart excludes them: `tma daemon` owns the daemon's lifecycle explicitly, and
/// `doctor` must report what is running rather than change it.
fn upgrade_check_eligible(command: &Option<Command>) -> bool {
    autostart_eligible(command) || matches!(command, Some(Command::Event(_)))
}

/// Run one of the daemon management verbs, reusing the one launcher the `Daemon` arm dispatches so
/// a chained step cannot diverge from the same command typed by hand. Auto-start discards the exit
/// code (a spawn failure must never reach the surface); `tma init --daemon` and the restart offers
/// asked for the daemon explicitly, so they read it. The detached daemon re-reads config from
/// `config_path`; the cloned `config` only feeds the foreground loader these branches skip.
fn run_daemon_verb(
    config: &config::Config,
    server: tmux::Server,
    manifest_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    mode: DaemonMode,
) -> ExitCode {
    tma_daemon::run_cli(tma_daemon::DaemonOpts {
        ensure: mode == DaemonMode::Ensure,
        restart: mode == DaemonMode::Restart,
        // The injected launcher exists for the skew-restart offers; stopping is never delegated.
        stop: false,
        quiet: false,
        server,
        manifest_dir,
        config_path,
        config: config.clone(),
        status_file: None,
        probe_cross_session: false,
        sweep_ms: None,
        detach_stage2: false,
        detach_session: false,
        fake_version: None,
        shutdown_delay_ms: None,
    })
}

/// The [`tma_daemon::DaemonOpts`] the upgrade check spawns a replacement daemon from. Only the
/// fields [`tma_daemon::evict_older_daemon`] reaches are meaningful: the target server and the
/// forwarded `--manifest-dir`/`--config`, which the replacement re-reads for itself, and `quiet`.
///
/// `quiet` is set on the `tma event` route and nowhere else: a hook's stderr can surface inside the
/// agent's own UI, so a failed eviction there must say nothing. A surface keeps its message.
fn daemon_opts_for_check(
    config: &config::Config,
    server: &tmux::Server,
    manifest_dir: &Option<PathBuf>,
    config_path: &Option<PathBuf>,
    quiet: bool,
) -> tma_daemon::DaemonOpts {
    tma_daemon::DaemonOpts {
        ensure: false,
        restart: false,
        stop: false,
        quiet,
        server: server.clone(),
        manifest_dir: manifest_dir.clone(),
        config_path: config_path.clone(),
        config: config.clone(),
        status_file: None,
        probe_cross_session: false,
        sweep_ms: None,
        detach_stage2: false,
        detach_session: false,
        fake_version: None,
        shutdown_delay_ms: None,
    }
}

/// The [`DaemonLauncher`] handed to the chained steps (`init`, `install-hooks`), with the target
/// and config already bound. A closure rather than a fn pointer so those modules carry no daemon
/// plumbing at all: tier 3 is reachable only from this file (tests/tier_boundary.rs).
fn daemon_launcher(
    config: &config::Config,
    server: tmux::Server,
    manifest_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> DaemonLauncher {
    let config = config.clone();
    Box::new(move |mode| {
        run_daemon_verb(
            &config,
            server.clone(),
            manifest_dir.clone(),
            config_path.clone(),
            mode,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsed subcommand for an argv, so the eligibility tests key on what a real invocation
    /// produces rather than on hand-built variants.
    fn command_of(argv: &[&str]) -> Option<Command> {
        let mut args = vec!["tma"];
        args.extend_from_slice(argv);
        Cli::try_parse_from(args)
            .unwrap_or_else(|e| panic!("parse {argv:?}: {e}"))
            .command
    }

    /// The upgrade check runs everywhere autostart does, plus `event`, the one tma command a hook
    /// fires unprompted, and therefore the only thing that notices a stale daemon on a machine
    /// where nobody types `tma`. The verbs that manage or inspect the daemon stay out of both:
    /// `tma daemon` owns the lifecycle explicitly, and `doctor` reports rather than changes.
    #[test]
    fn the_upgrade_check_covers_the_surfaces_and_the_hook_path() {
        for argv in [
            vec![],
            vec!["ls"],
            vec!["status"],
            vec!["jump"],
            vec!["wait", "--any", "--until", "idle"],
            vec!["watch"],
            vec!["subscribe"],
        ] {
            let c = command_of(&argv);
            assert!(autostart_eligible(&c), "{argv:?} is a surface");
            assert!(upgrade_check_eligible(&c), "{argv:?} runs the check");
        }

        let event = command_of(&["event", "--agent", "claude", "--kind", "Notification"]);
        assert!(
            !autostart_eligible(&event),
            "a hook must never start a daemon"
        );
        assert!(
            upgrade_check_eligible(&event),
            "but it may replace one that is a build behind"
        );

        for argv in [
            vec!["daemon", "--ensure"],
            vec!["doctor"],
            vec!["init"],
            vec!["install-hooks", "--check"],
            vec!["install-keys", "--check"],
            vec!["completions", "bash"],
            vec!["version"],
            vec!["reload"],
        ] {
            let c = command_of(&argv);
            assert!(
                !upgrade_check_eligible(&c),
                "{argv:?} manages or inspects the daemon; it must not restart one"
            );
        }
    }

    /// A `--client` value that is empty or still carries an unexpanded tmux format is not a client
    /// name: it reaches us verbatim from a binding whose context does not format-expand (an
    /// installed `display-popup -E ... --client "#{client_name}"`), and only the targetless fallback
    /// can jump from there. A real name, including one with a `#` in it, is kept.
    #[test]
    fn client_normalizes_empty_and_unexpanded_formats_to_none() {
        for literal in ["#{client_name}", "/dev/ttys00#{client_name}", "", "   "] {
            assert_eq!(
                normalize_client(Some(literal.to_string())),
                None,
                "{literal:?} is not a usable client name"
            );
        }
        for name in ["/dev/ttys003", "client-1", "a#b"] {
            assert_eq!(
                normalize_client(Some(name.to_string())),
                Some(name.to_string()),
                "{name:?} is a client name"
            );
        }
        assert_eq!(normalize_client(None), None);
    }
}
