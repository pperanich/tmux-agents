//! `tma attach --pane %N`: hand a fresh terminal to the tmux session a pane lives in.
//!
//! `tma jump --pane` is `switch-client` + `select-window` + `select-pane`, and `switch-client`
//! needs a client that is already attached. From a terminal that has none (a new ssh session, a
//! phone's terminal app) a jump has nothing to move. The way in is to *become* the client: select
//! the target's window and pane on its session, then replace this process with a real `tmux
//! attach-session`, so the tty the user is holding is the one tmux takes over.
//!
//! Inside tmux (`$TMUX` set) there IS a client, and a nested attach is never what anybody means,
//! so the command degrades to exactly `tma jump --pane` and says so.
//!
//! Exit codes: `0` attached (the exec does not return) or printed, `2` no terminal to hand over,
//! `3` the pane vanished, `1` a runtime failure.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use tma_core::Selector;

use crate::config::Config;
use crate::tmux::{self, Tmux};
use crate::{cli_support, dispatch, jump};

/// Everything `tma attach` needs, assembled by the bin's dispatch from the CLI args and config.
pub(crate) struct AttachOpts {
    pub pane: String,
    pub print: bool,
    pub server: tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    pub config: Config,
    pub client: Option<String>,
}

pub(crate) fn run(opts: AttachOpts) -> ExitCode {
    if inside_tmux() {
        if opts.print {
            eprintln!(
                "tma: already inside tmux, so this is a jump, not a handover; there is no \
                 attach-session to print"
            );
        }
        return dispatch::run_jump_kind(
            jump::JumpKind::Pane(opts.pane),
            &Selector::default(),
            &opts.server,
            opts.manifest_dir,
            &opts.config,
            opts.client,
        );
    }

    // The exec gives tmux this process's terminal. Without one there is nothing to give, and tmux
    // would fail with its own terse line after the selects had already moved somebody's focus.
    if !opts.print && !std::io::stdin().is_terminal() {
        eprintln!(
            "tma: stdin is not a terminal, so there is nothing to hand to tmux; run `tma attach` \
             from a terminal, or `tma attach --pane {} --print` to see the command it would run",
            opts.pane
        );
        return ExitCode::from(2);
    }

    let tmux = Tmux::connect(&opts.server);
    let (session, window_target) = match resolve(&tmux, &opts.pane) {
        Ok(target) => target,
        Err(code) => return code,
    };

    // The second vanish check: a select on a pane closed since the read above fails, and there is
    // no point attaching to a session whose target went with it.
    if let Err(err) = tmux.select_pane_in_window(&window_target, &opts.pane) {
        return vanished(&opts.pane, Some(&err));
    }

    if opts.print {
        match tmux.attach_argv(&session) {
            Ok(argv) => {
                println!("{}", render_argv(&argv));
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("tma: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Only ever returns an error: a successful exec never comes back here.
    match tmux.exec_attach(&session) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tma: cannot attach to session {session}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Whether this process is already a tmux client's child. An empty `$TMUX` is not inside tmux.
fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// The pane's session and its `session:window` target. Two reads rather than one packed format:
/// a session name may contain the separator any packing would pick, and this path execs straight
/// after, so the extra round trip costs nothing that matters.
fn resolve(tmux: &Tmux, pane: &str) -> Result<(String, String), ExitCode> {
    let session = read_pane(tmux, pane, "#{session_name}")?;
    let window = read_pane(tmux, pane, "#{window_index}")?;
    Ok((session.clone(), format!("{session}:{window}")))
}

/// One format read against the target. Anything but a non-empty answer is the pane being gone:
/// tmux fails the read outright for an unknown id, and an empty value resolved to nothing.
fn read_pane(tmux: &Tmux, pane: &str, format: &str) -> Result<String, ExitCode> {
    match tmux.pane_format(pane, format) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) => Err(vanished(pane, None)),
        Err(err) => Err(vanished(pane, Some(&err))),
    }
}

/// The one refusal for a target that is not there, whichever read found it. A gone server is
/// reported as itself: it is the difference between "that pane closed" and "nothing is running".
fn vanished(pane: &str, err: Option<&tmux::TmuxError>) -> ExitCode {
    match err {
        Some(tmux::TmuxError::ServerGone) => cli_support::no_server(),
        Some(err) => {
            eprintln!("tma: pane {pane} vanished (exit 3): {err}");
            ExitCode::from(3)
        }
        None => {
            eprintln!("tma: pane {pane} vanished (exit 3)");
            ExitCode::from(3)
        }
    }
}

/// Render an argv as one shell-safe line, so `--print`'s output can be read, pasted, or eval'd.
fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote an argv element the shell would otherwise re-split or expand (a socket path may
/// contain a space, a session name almost anything). Plain words are left bare.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=+,@%".contains(&b));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The printed line is the argv, not a re-parse of it: a socket path with a space and a session
    /// name with a quote both survive as single shell words.
    #[test]
    fn print_renders_a_shell_safe_line() {
        let argv = [
            "/usr/bin/tmux",
            "-S",
            "/tmp/tma sockets/x",
            "attach-session",
            "-t",
            "it's mine",
        ]
        .map(String::from);
        assert_eq!(
            render_argv(&argv),
            "/usr/bin/tmux -S '/tmp/tma sockets/x' attach-session -t 'it'\\''s mine'"
        );
    }

    /// The ordinary case stays readable: nothing a shell would touch is quoted.
    #[test]
    fn print_leaves_plain_words_bare() {
        let argv = [
            "/usr/bin/tmux",
            "-L",
            "work_1",
            "attach-session",
            "-t",
            "home",
        ]
        .map(String::from);
        assert_eq!(
            render_argv(&argv),
            "/usr/bin/tmux -L work_1 attach-session -t home"
        );
        // An empty element is still a word, so it has to survive as one.
        assert_eq!(shell_quote(""), "''");
    }
}
