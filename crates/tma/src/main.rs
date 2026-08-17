#![deny(rustdoc::broken_intra_doc_links)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod act;
mod cli;
mod cli_support;
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

// Re-import the tmux/stamp modules at the crate root so the bin's `crate::tmux` / `crate::stamp`
// paths keep resolving to the one crate that spawns tmux.
use tma_tmux::{stamp, tmux};
// Re-import the tier-2 pipeline modules the bin drives (`identity`/`capture` have no bin consumer);
// the tier-3 `tma daemon` subcommand dispatches into the separate `tma_daemon` crate.
use tma_runtime::{config, cycle, debug, event, json, manifests};
// The display layer (picker, jump, ls/status surfaces) lives in `tma-ui`; `ansi` is internal to it.
use tma_ui::{jump, picker, surfaces, watch};

use cli::{Cli, Command};
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
        let _ = ensure_daemon(
            &config,
            server.clone(),
            manifest_dir.clone(),
            cli.config.clone(),
        );
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
        Some(Command::Watch(args)) => {
            let start_table = args.table;
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
            server: server.clone(),
            manifest_dir,
            config_path: cli.config,
            config,
            status_file: args.status_file,
            probe_cross_session: args.probe_cross_session,
            sweep_ms: args.sweep_ms,
            detach_stage2: args.detach_stage2,
            detach_session: args.detach_session,
        }),
        Some(Command::Init(args)) => init::run(init::InitOpts {
            assume_yes: args.yes,
            daemon: args.daemon,
            no_daemon: args.no_daemon,
            config_dir: args.config_dir,
            conf: args.conf,
            server: server.clone(),
            manifest_dir,
            config_path: cli.config,
            config,
            ensure_daemon,
        }),
        Some(Command::InstallHooks(args)) => install::run(install::InstallOpts {
            statusline: match (args.statusline, args.no_statusline) {
                (true, _) => install::Statusline::Install,
                (_, true) => install::Statusline::Remove,
                _ => install::Statusline::Keep,
            },
            agent: args.agent,
            uninstall: args.uninstall,
            check: args.check,
            assume_yes: args.yes,
            server: server.clone(),
            manifest_dir,
            settings: args.settings,
            gemini_settings: args.gemini_settings,
            config_dir: args.config_dir,
            wrapper_path: args.wrapper_path,
            opencode_plugin: args.opencode_plugin,
            codex_config: args.codex_config,
            codex_hooks: args.codex_hooks,
            cursor_hooks: args.cursor_hooks,
            cursor_cli_config: args.cursor_cli_config,
            pi_extension: args.pi_extension,
            focus_events: config.focus.events,
            agents: config.agent_overrides,
        }),
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
        }),
        Some(Command::Reload) => run_reload(&server),
        Some(Command::ClearAttention(args)) => run_clear_attention(args, &server),
        Some(Command::Supervise(args)) => run_supervise(args, &server, config),
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

/// Run the idempotent `tma daemon --ensure` spawn, reusing the one launcher the `Daemon` arm
/// dispatches so it cannot diverge from a manual `tma daemon --ensure`. Auto-start discards the
/// exit code (a spawn failure must never reach the surface); `tma init --daemon` asked for the
/// daemon explicitly, so it reads it. The detached daemon re-reads config from `config_path`; the
/// cloned `config` only feeds the foreground loader the ensure branch skips.
fn ensure_daemon(
    config: &config::Config,
    server: tmux::Server,
    manifest_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> ExitCode {
    tma_daemon::run_cli(tma_daemon::DaemonOpts {
        ensure: true,
        server,
        manifest_dir,
        config_path,
        config: config.clone(),
        status_file: None,
        probe_cross_session: false,
        sweep_ms: None,
        detach_stage2: false,
        detach_session: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
