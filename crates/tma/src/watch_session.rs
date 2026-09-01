//! Dedicated-session launcher for the managed `prefix G` watcher. The launcher runs briefly under
//! tmux `run-shell`, creates one session per client, switches that client to it, and starts a child
//! `tma watch` configured to exit after a jump. The child pane's exit destroys the temporary
//! session; an origin pane id carries the return trail across that boundary.

use std::path::PathBuf;
use std::process::ExitCode;

use tma_runtime::ui;
use tma_tmux::tmux::{Server, Tmux};

use crate::cli::{SelectorArgs, WatchArgs};

pub(crate) struct WatchSessionOpts {
    pub args: WatchArgs,
    pub server: Server,
    pub manifest_dir: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub client: Option<String>,
}

pub(crate) fn run(opts: WatchSessionOpts) -> ExitCode {
    let tmux = Tmux::connect(&opts.server);
    let client = opts
        .client
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| ui::active_client_name(&tmux));
    if client.is_empty() {
        eprintln!("tma: `watch --temporary-session` needs an attached tmux client");
        return ExitCode::FAILURE;
    }

    // Resolve both while the client is still on the user's pane. Once it switches, a targetless
    // query would see the temporary watcher itself and poison `jump --back` with a dead origin.
    let origin_pane = opts
        .args
        .origin_pane
        .clone()
        .or_else(|| ui::active_pane_id(&tmux, Some(&client)));
    let Some(origin_pane) = origin_pane else {
        eprintln!("tma: could not resolve the pane that opened the temporary watcher");
        return ExitCode::FAILURE;
    };
    let client_pid = match tmux
        .display_active_client(Some(&client), "#{client_pid}")
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        Some(pid) => pid,
        None => {
            eprintln!("tma: could not resolve the tmux client for the temporary watcher");
            return ExitCode::FAILURE;
        }
    };

    let argv = child_argv(
        &opts.server,
        opts.manifest_dir.as_deref(),
        opts.config_path.as_deref(),
        &client,
        &origin_pane,
        &opts.args,
    );
    let command = shell_join(&argv);
    let session = format!("tma-watch-{client_pid}");
    match tmux.open_temporary_session(&client, &session, &command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tma: opening temporary watch session failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Re-invoke this exact binary against the same explicit inputs, dropping `--temporary-session`
/// and adding the two child-only lifecycle flags. Environment-selected config/socket values need no
/// argv entry: the new pane inherits the tmux server environment that launched this process.
fn child_argv(
    server: &Server,
    manifest_dir: Option<&std::path::Path>,
    config_path: Option<&std::path::Path>,
    client: &str,
    origin_pane: &str,
    args: &WatchArgs,
) -> Vec<String> {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tma".to_string());
    let mut argv = vec![exe];
    if let Some(name) = &server.socket_name {
        argv.extend(["--socket-name".to_string(), name.clone()]);
    } else if let Some(path) = &server.socket_path {
        argv.extend([
            "--socket-path".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(path) = manifest_dir {
        argv.extend([
            "--manifest-dir".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(path) = config_path {
        argv.extend(["--config".to_string(), path.to_string_lossy().into_owned()]);
    }
    argv.extend([
        "--client".to_string(),
        client.to_string(),
        "watch".to_string(),
    ]);
    if args.table {
        argv.push("--table".to_string());
    }
    append_selector(&mut argv, &args.selector);
    argv.extend([
        "--exit-on-jump".to_string(),
        "--origin-pane".to_string(),
        origin_pane.to_string(),
    ]);
    argv
}

fn append_selector(argv: &mut Vec<String>, selector: &SelectorArgs) {
    for (flag, value) in [
        ("--session", selector.session.as_ref()),
        ("--repo", selector.repo.as_ref()),
        ("--branch", selector.branch.as_ref()),
        ("--agent", selector.agent.as_ref()),
    ] {
        if let Some(value) = value {
            argv.extend([flag.to_string(), value.clone()]);
        }
    }
    if let Some(states) = &selector.state {
        argv.extend(["--state".to_string(), states.cli_value()]);
    }
}

/// Quote one argv as a POSIX shell command. tmux 3.2 accepts one `shell-command` string for
/// `new-session`, so this cannot rely on newer multi-argument command forms. The standard
/// single-quote splice keeps paths, client names, and selector values literal, including embedded
/// apostrophes.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| format!("'{}'", arg.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_spaces_and_apostrophes() {
        assert_eq!(
            shell_join(&["/tmp/my tma".into(), "it's".into(), "%3".into()]),
            "'/tmp/my tma' 'it'\"'\"'s' '%3'"
        );
    }
}
