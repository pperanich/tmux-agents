//! `tma watch --toggle`: open or close the acting client's `tma watch` sidebar, the action behind
//! the status line's `tma:sidebar` click segment (and usable from a key binding or a shell).
//!
//! Detection reuses the pid a running sidebar already advertises in the pane-scoped
//! `@tma_watch_pid` (the same option [`crate::nudge`] signals): a pane in the client's session
//! carrying a live pid IS the sidebar, so no separate registry can go stale. A pid that no longer
//! exists is a residue of a sidebar killed without unsetting it — the option is cleared and the
//! toggle opens a fresh pane, the same tolerance the nudge sender applies.
//!
//! It lives in tier-2 runtime rather than `tma-ui` for the reason the nudge does: it needs a
//! process-liveness syscall, and it is not a display surface (nothing is drawn).

use rustix::process::{test_kill_process, Pid};
use tma_core::stamp::opt;
use tma_tmux::tmux::{Server, Tmux, TmuxError};

use crate::nudge::parse_watch_pid;

/// Columns the opened sidebar gets. Matches the `prefix W` binding's `-l 32` intent (a compact list
/// pane) with room for the branch label the rows carry.
const SIDEBAR_WIDTH: u32 = 40;

/// What [`toggle`] did, for the CLI to report and the tests to assert on.
#[derive(Debug, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// A live sidebar was found in the client's session and its pane killed.
    Closed(String),
    /// No sidebar was running, so one was split beside the client's active pane.
    Opened,
    /// No acting client resolved (outside tmux, or a `--client` naming nobody): nowhere to toggle.
    NoClient,
}

/// Toggle the sidebar for the session `client` is viewing (`None` = the most-recently-active
/// client, the same targetless fallback the picker and jump use). `exe` is the `tma` binary the
/// opened pane runs; `server` is forwarded to it so the sidebar lands on the same tmux server.
pub fn toggle(
    tmux: &Tmux,
    server: &Server,
    client: Option<&str>,
    exe: &str,
) -> Result<ToggleOutcome, TmuxError> {
    let session = tmux
        .display_active_client(client, "#{session_name}")
        .unwrap_or_default();
    if session.is_empty() {
        return Ok(ToggleOutcome::NoClient);
    }

    if let Some(pane) = find_sidebar(tmux, &session)? {
        tmux.kill_pane(&pane)?;
        return Ok(ToggleOutcome::Closed(pane));
    }

    // Split the client's own active pane, so the sidebar opens in the window being looked at. An
    // unreadable pane id falls back to the session, whose active pane is the same target by another
    // name; `-d` keeps the focus where the user left it.
    let target = tmux
        .display_active_client(client, "#{pane_id}")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| session.clone());
    tmux.split_beside(&target, SIDEBAR_WIDTH, &watch_command(exe, server))?;
    Ok(ToggleOutcome::Opened)
}

/// The live sidebar's pane in `session`, if any. Panes advertising a pid that is gone are cleaned as
/// they are passed, so a residual option cannot wedge the toggle into never opening one.
fn find_sidebar(tmux: &Tmux, session: &str) -> Result<Option<String>, TmuxError> {
    for (pane, raw) in tmux.list_session_pane_option(session, opt::WATCH_PID)? {
        // `parse_watch_pid` keeps the `pid > 0` filter `Pid::from_raw` needs, as the nudge does.
        if parse_watch_pid(&raw)
            .and_then(Pid::from_raw)
            .is_some_and(pid_alive)
        {
            return Ok(Some(pane));
        }
        let _ = tmux.unset_pane_option(&pane, opt::WATCH_PID);
    }
    Ok(None)
}

/// Whether `pid` names a live process (`kill(pid, 0)`). `EPERM` means it exists under another user,
/// which here counts as alive; only `ESRCH` says gone.
fn pid_alive(pid: Pid) -> bool {
    !matches!(test_kill_process(pid), Err(rustix::io::Errno::SRCH))
}

/// The shell command the opened pane runs: `'<exe>' watch` plus the server selector. Single-quoted
/// because `exe` comes from `current_exe()` and may hold a space; no tmux format appears in it, so
/// the string means the same wherever tmux hands it to a shell.
fn watch_command(exe: &str, server: &Server) -> String {
    format!("'{exe}' watch{}", server.shell_flag())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_command_quotes_the_binary_and_forwards_the_server() {
        assert_eq!(
            watch_command("/usr/local/bin/tma", &Server::default()),
            "'/usr/local/bin/tma' watch"
        );
        assert_eq!(
            watch_command("/opt/my tools/tma", &Server::named(Some("scratch".into()))),
            "'/opt/my tools/tma' watch --socket-name scratch"
        );
        assert!(
            !watch_command("/usr/local/bin/tma", &Server::default()).contains("#{"),
            "the command carries no tmux format: split-window does not expand one"
        );
    }

    /// Our own pid is alive; pid 1 belongs to root, so an unprivileged run sees EPERM and must
    /// still read it as alive. A reaped high pid is gone.
    #[test]
    fn pid_alive_reads_the_process_table() {
        let me = Pid::from_raw(std::process::id() as i32).expect("our own pid is positive");
        assert!(pid_alive(me));
        assert!(pid_alive(Pid::from_raw(1).unwrap()), "EPERM is still alive");
        // Above the platform pid_max on both Linux (4194304) and macOS (99998): never a live pid.
        assert!(!pid_alive(Pid::from_raw(0x7fff_fff0).unwrap()));
    }
}
