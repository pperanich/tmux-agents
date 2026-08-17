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
    /// A live sidebar was already in the window being looked at, and its pane was killed.
    Closed(String),
    /// A live sidebar was in another window of the session and was moved here instead of a second
    /// one being opened. The click always ends with a sidebar you can see.
    Moved(String),
    /// No sidebar was running, so one was split beside the client's active pane.
    Opened,
    /// No acting client resolved (outside tmux, or a `--client` naming nobody): nowhere to toggle.
    NoClient,
}

/// Toggle the sidebar for the session `client` is viewing (`None` = the most-recently-active
/// client, the same targetless fallback the picker and jump use). `exe` is the `tma` binary the
/// opened pane runs; `server` is forwarded to it so the sidebar lands on the same tmux server.
///
/// Three outcomes, in the order a user reads them: a sidebar in *this* window closes, a sidebar in
/// another window of the session moves here, and no sidebar at all opens one. The middle case is
/// what keeps a click from spawning a second watcher you cannot see — before it, jumping away and
/// clicking `☰` again left the first one running in the window you left.
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
    // The client's own active pane, so the sidebar opens (or lands) in the window being looked at.
    // An unreadable pane id falls back to the session, whose active pane is the same target by
    // another name.
    let target = tmux
        .display_active_client(client, "#{pane_id}")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| session.clone());
    let here = tmux
        .display_active_client(client, "#{window_id}")
        .unwrap_or_default();

    if let Some(place) = find_sidebar(tmux, &session)? {
        if place.window == here {
            tmux.kill_pane(&place.pane)?;
            return Ok(ToggleOutcome::Closed(place.pane));
        }
        // Elsewhere in the session: bring it here rather than opening a second one — unless it has
        // its window to itself, which is what `tma watch --table` in its own window looks like.
        // Moving that pane would destroy the window, so it is left alone and a real sidebar opens.
        if !place.alone {
            tmux.join_beside(&place.pane, &target, SIDEBAR_WIDTH)?;
            return Ok(ToggleOutcome::Moved(place.pane));
        }
    }

    tmux.split_beside(&target, SIDEBAR_WIDTH, &watch_command(exe, server))?;
    Ok(ToggleOutcome::Opened)
}

/// Why a sidebar did or did not follow the jump it just made. Every non-`Moved` arm is a
/// deliberate refusal, not a failure: the sidebar stays where it is and the jump still happened.
#[derive(Debug, PartialEq, Eq)]
pub enum FollowOutcome {
    /// The pane was moved into the window just jumped to.
    Moved,
    /// The jump landed in the window the sidebar is already in; nothing to do.
    SameWindow,
    /// The sidebar has its window to itself (`tma watch --table` in its own window). Moving the
    /// last pane out of a window destroys it, so a full-window watcher never follows.
    OwnWindow,
    /// Another client is attached to the session the sidebar would leave: moving it would take the
    /// pane off that client's screen.
    OtherClientWatching,
    /// The surface does not know its own pane (`$TMUX_PANE` unset — running outside tmux).
    NoPane,
}

/// The facts the follow decision reads, split out so the policy is unit-testable without a server.
struct FollowFacts<'a> {
    self_window: &'a str,
    self_alone_in_window: bool,
    target_window: &'a str,
    /// Clients attached to the sidebar's current session, not counting the one that jumped.
    other_clients_on_self_session: usize,
}

/// The pure policy behind [`follow`].
fn decide_follow(f: &FollowFacts) -> FollowOutcome {
    if f.self_window == f.target_window {
        return FollowOutcome::SameWindow;
    }
    if f.self_alone_in_window {
        return FollowOutcome::OwnWindow;
    }
    if f.other_clients_on_self_session > 0 {
        return FollowOutcome::OtherClientWatching;
    }
    FollowOutcome::Moved
}

/// Move the sidebar's own pane (`self_pane`) into the window holding `target_pane`, the pane the
/// client was just jumped to. Called right after a successful focus, so the target window is
/// already the active one; the join is detached, leaving the focus on the agent, not the sidebar.
///
/// `self_session` is read fresh rather than passed: the sidebar may itself have been moved since it
/// started, so the session it lives in now is the only one that matters.
pub fn follow(
    tmux: &Tmux,
    client: Option<&str>,
    self_pane: Option<&str>,
    target_pane: &str,
) -> Result<FollowOutcome, TmuxError> {
    let Some(self_pane) = self_pane.filter(|p| !p.is_empty()) else {
        return Ok(FollowOutcome::NoPane);
    };
    let mine = tmux.pane_format(self_pane, "#{window_id}\t#{window_panes}\t#{session_name}")?;
    let mut mine = mine.split('\t');
    let (Some(self_window), Some(panes), Some(self_session)) =
        (mine.next(), mine.next(), mine.next())
    else {
        return Ok(FollowOutcome::NoPane);
    };
    let target_window = tmux.pane_format(target_pane, "#{window_id}")?;

    // The jumping client does not count against the move: it is the one asking for it.
    let acting = tmux
        .display_active_client(client, "#{client_name}")
        .unwrap_or_default();
    let others = tmux
        .list_session_clients(self_session)?
        .into_iter()
        .filter(|c| *c != acting)
        .count();

    let outcome = decide_follow(&FollowFacts {
        self_window,
        self_alone_in_window: panes.trim() == "1",
        target_window: &target_window,
        other_clients_on_self_session: others,
    });
    if outcome == FollowOutcome::Moved {
        tmux.join_beside(self_pane, target_pane, SIDEBAR_WIDTH)?;
    }
    Ok(outcome)
}

/// Where a live sidebar sits: its pane, the window holding it, and whether it has that window to
/// itself. The toggle needs all three — the window to tell "here" from "elsewhere", and `alone` to
/// leave a full-window `watch --table` where it is.
struct SidebarPlace {
    pane: String,
    window: String,
    alone: bool,
}

/// The live sidebar in `session`, if any. Panes advertising a pid that is gone are cleaned as they
/// are passed, so a residual option cannot wedge the toggle into never opening one.
fn find_sidebar(tmux: &Tmux, session: &str) -> Result<Option<SidebarPlace>, TmuxError> {
    for (pane, raw) in tmux.list_session_pane_option(session, opt::WATCH_PID)? {
        // `parse_watch_pid` keeps the `pid > 0` filter `Pid::from_raw` needs, as the nudge does.
        if parse_watch_pid(&raw)
            .and_then(Pid::from_raw)
            .is_some_and(pid_alive)
        {
            let place = tmux.pane_format(&pane, "#{window_id}\t#{window_panes}")?;
            let (window, panes) = place.split_once('\t').unwrap_or((place.as_str(), "0"));
            return Ok(Some(SidebarPlace {
                pane,
                window: window.to_string(),
                alone: panes.trim() == "1",
            }));
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

    /// The follow policy, arm by arm. Every refusal is deliberate: the jump still happened, the
    /// sidebar just stayed where it was.
    #[test]
    fn follow_moves_only_when_moving_costs_nothing() {
        let facts = |self_window: &'static str,
                     alone: bool,
                     target_window: &'static str,
                     others: usize| FollowFacts {
            self_window,
            self_alone_in_window: alone,
            target_window,
            other_clients_on_self_session: others,
        };
        assert_eq!(
            decide_follow(&facts("@1", false, "@2", 0)),
            FollowOutcome::Moved
        );
        assert_eq!(
            decide_follow(&facts("@1", false, "@1", 0)),
            FollowOutcome::SameWindow,
            "the jump landed beside it already"
        );
        assert_eq!(
            decide_follow(&facts("@1", true, "@2", 0)),
            FollowOutcome::OwnWindow,
            "a full-window watcher would take its window with it"
        );
        assert_eq!(
            decide_follow(&facts("@1", false, "@2", 1)),
            FollowOutcome::OtherClientWatching,
            "another client is looking at the session it would leave"
        );
        // The window check comes first: a same-window jump is a no-op even for a lone pane, so a
        // `--table` window that jumps within itself is not reported as a refusal.
        assert_eq!(
            decide_follow(&facts("@1", true, "@1", 2)),
            FollowOutcome::SameWindow
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
