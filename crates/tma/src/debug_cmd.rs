use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::{DebugArgs, DebugCommand, ReadOpts, StampArgs};
use crate::{cli_support, config, debug, redact, stamp, tmux};

pub(crate) fn run_debug(
    args: DebugArgs,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    config: &config::Config,
) -> ExitCode {
    match args.command {
        DebugCommand::Redact { file, pattern } => match fs::read_to_string(&file) {
            Ok(text) => match redact::redact(&text, &pattern) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("tma: {err}");
                    ExitCode::FAILURE
                }
            },
            Err(err) => {
                eprintln!("tma: cannot read {}: {err}", file.display());
                ExitCode::FAILURE
            }
        },
        DebugCommand::Capture { read } => run_observe(read, server, manifest_dir, config, |obs| {
            print!("{}", debug::render_capture(obs));
        }),
        DebugCommand::Explain { read, json } => {
            run_observe(read, server, manifest_dir, config, move |obs| {
                if json {
                    println!("{}", debug::render_explain_json(obs));
                } else {
                    print!("{}", debug::render_explain_text(obs));
                }
            })
        }
        DebugCommand::Transitions { json } => run_transitions(json, server),
        DebugCommand::NotifyTest { trigger } => run_notify_test(&trigger, config),
        DebugCommand::Stamp(args) => run_stamp(args, server),
    }
}

/// `tma debug transitions`: read the running daemon's transition ring and print it. The ring is
/// daemon-only memory, so with no daemon there is nothing to read (the durable per-notification
/// record is `[notify] log`).
fn run_transitions(json: bool, server: &tmux::Server) -> ExitCode {
    use tma_runtime::ipc::HistoryOutcome;

    let tmux = tmux::Tmux::connect(server);
    match tma_runtime::ipc::fetch_transitions(&tmux) {
        HistoryOutcome::Document(doc) => {
            let t = tma_runtime::transitions::parse_document(&doc);
            if json {
                println!("{}", tma_runtime::transitions::render_json(&t));
            } else {
                print!("{}", tma_runtime::transitions::render_text(&t));
            }
            ExitCode::SUCCESS
        }
        HistoryOutcome::NoServer => cli_support::no_server(),
        HistoryOutcome::NotRunning => {
            eprintln!(
                "tma: no daemon is running for this server, and the transition history lives in \
                 the daemon (start one with `tma daemon`)"
            );
            ExitCode::FAILURE
        }
        HistoryOutcome::Unsupported => {
            eprintln!(
                "tma: the running daemon predates `debug transitions`; restart it to pick up this \
                 build (a reload cannot add a protocol verb)"
            );
            ExitCode::FAILURE
        }
    }
}

/// `tma debug notify-test`: resolve the trigger's command, fire it against a representative payload,
/// and report. Exits non-zero when the trigger has no command or the command failed, so it doubles as
/// a check; the same failure marker a real fire leaves is updated, so `tma doctor` agrees with it.
fn run_notify_test(trigger: &str, config: &config::Config) -> ExitCode {
    let trigger = match trigger.parse::<tma_runtime::notify::TestTrigger>() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let commands = config
        .notify
        .commands()
        .overridden_by(config::notify_cmd_env());
    let sinks = config.notify.sinks();
    let out = tma_runtime::notify::notify_test(&commands, &sinks, trigger, tma_runtime::now_ms());

    println!("payload   {}", out.payload);
    match &out.command {
        Some(cmd) => println!("command   {cmd}"),
        None => println!(
            "command   (none — set notify.command, or a command in the trigger's sub-table)"
        ),
    }
    if out.command.is_some() {
        match out.code {
            Some(code) => println!("exit      {code}"),
            None => println!("exit      (no exit code)"),
        }
    }
    if !out.stderr.trim().is_empty() {
        println!(
            "stderr    {}",
            out.stderr.trim_end().replace('\n', "\n          ")
        );
    }
    if let Some(err) = &out.error {
        eprintln!("tma: notify command failed: {err}");
    }
    if out.delivered() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Handle the internal `debug stamp` harness. Applies exactly one guarded chain.
fn run_stamp(args: StampArgs, server: &tmux::Server) -> ExitCode {
    use tma_core::render::{Guard, Publish};
    use tma_core::{AgentState, Detail, Provenance};

    let tmux = tmux::Tmux::connect(server);
    let panes = match tmux.list_panes() {
        Ok(p) => p,
        Err(tmux::TmuxError::ServerGone) => return cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };

    let parse_guard = |s: &str| -> Result<Guard, String> {
        match s {
            "unconditional" => Ok(Guard::Unconditional),
            "protect-hook" => Ok(Guard::ProtectHook),
            other => {
                if let Some(n) = other.strip_prefix("carveout:") {
                    n.parse()
                        .map(|capture_at| Guard::CarveOut { capture_at })
                        .map_err(|_| format!("bad carveout epoch: {n:?}"))
                } else if let Some(st) = other.strip_prefix("refresh:") {
                    st.parse::<AgentState>()
                        .map(|state| Guard::RefreshClaim { state })
                        .map_err(|_| format!("bad refresh state: {st:?}"))
                } else if let Some(n) = other.strip_prefix("arbitrate:") {
                    n.parse()
                        .map(|evidence_at| Guard::HookArbitrate { evidence_at })
                        .map_err(|_| format!("bad arbitrate epoch: {n:?}"))
                } else {
                    Err(format!("unknown guard: {other:?}"))
                }
            }
        }
    };

    // The conditional-write behaviour probe: degrade to advisory writes if `set -pF` does not
    // expand server-side. Exposed as its own mode for the acceptance test.
    if args.mode == "probe" {
        match stamp::probe_conditional_writes(&tmux, &args.pane) {
            Ok(ok) => {
                println!("conditional-writes: {}", if ok { "yes" } else { "no" });
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("tma: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let plan = match args.mode.as_str() {
        "remove" => stamp::StampPlan::Remove,
        "hold" => stamp::StampPlan::Hold {
            stamped_at: args.stamped_at.unwrap_or(0),
            hash: args.hash,
        },
        "publish" => {
            let state = match args.state.as_deref().map(str::parse::<AgentState>) {
                Some(Ok(s)) => s,
                _ => {
                    eprintln!("tma: publish requires a valid --state");
                    return ExitCode::FAILURE;
                }
            };
            let guard = match parse_guard(&args.guard) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("tma: {e}");
                    return ExitCode::FAILURE;
                }
            };
            stamp::StampPlan::Publish(Publish {
                pane_id: args.pane.clone(),
                state,
                detail: args.detail.map(Detail::new),
                source: args
                    .source
                    .as_deref()
                    .and_then(|s| s.parse::<Provenance>().ok())
                    .unwrap_or(Provenance::Capture),
                evidence_at: args.evidence_at.unwrap_or(0),
                since: args.since.unwrap_or(0),
                stamped_at: args.stamped_at.unwrap_or(0),
                hash: args.hash,
                pid: args.pid.unwrap_or(0),
                name: args.name.unwrap_or_default(),
                set_attention: args.attention,
                episode_reset: args.episode_reset,
                guard,
            })
        }
        other => {
            eprintln!("tma: unknown --mode {other:?} (publish|hold|remove)");
            return ExitCode::FAILURE;
        }
    };

    // The debug harness exercises the guarded write path explicitly (guards are passed on the
    // command line); it never degrades. The advisory path is driven by the poll cycle.
    match stamp::apply(&tmux, &panes, &args.pane, &plan, true) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tma: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Load manifests, read + detect the pane, and hand the observation to `emit`.
fn run_observe(
    read: ReadOpts,
    server: &tmux::Server,
    manifest_dir: Option<PathBuf>,
    config: &config::Config,
    emit: impl FnOnce(&debug::Observation),
) -> ExitCode {
    let manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let tmux = tmux::Tmux::connect(server);
    match debug::observe(&tmux, &read.pane, &manifests, &config.fold_config()) {
        Ok(obs) => {
            emit(&obs);
            ExitCode::SUCCESS
        }
        Err(debug::DebugError::Tmux(tmux::TmuxError::ServerGone)) => cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            ExitCode::FAILURE
        }
    }
}
