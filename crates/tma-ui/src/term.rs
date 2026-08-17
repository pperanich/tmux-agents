//! One RAII terminal guard shared by the picker and `tma watch`.
//!
//! Both enter raw mode + the alternate screen, run a draw/input loop, and must restore the terminal
//! on *every* exit (quit, a `?` mid-loop, panic). Hand-rolled restore leaked three ways: a setup
//! failure after `enable_raw_mode` left it raw; the restore path `?`-short-circuited between steps;
//! and `watch` advertised `@tma_watch_pid` before setup, so an early failure stranded a stale pid.
//! [`TerminalGuard`] fixes all three: setup is all-or-nothing in [`TerminalGuard::enter`], the pid
//! is advertised only after setup succeeds, and [`Drop`] unsets it then restores every step (no `?`).

use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use tma_runtime::{ui, Tmux};

/// Owns the terminal's raw-mode + alternate-screen state (and, for `tma watch`, the advertised
/// `@tma_watch_pid`), restoring all of it in [`Drop`]. Keep it alive for the whole TUI session.
pub(crate) struct TerminalGuard<'a> {
    tmux: &'a Tmux,
    /// The pane whose `@tma_watch_pid` this guard advertised (`tma watch` only), unset in `Drop`.
    watch_pane: Option<String>,
}

impl<'a> TerminalGuard<'a> {
    /// Enter raw mode + the alternate screen, all-or-nothing (a failed alt-screen switch undoes raw
    /// mode first). The pid is advertised in `@tma_watch_pid` only after setup succeeds; unset on drop.
    pub(crate) fn enter(
        tmux: &'a Tmux,
        watch_pane: Option<String>,
        advertise_pid: Option<u32>,
    ) -> io::Result<TerminalGuard<'a>> {
        enable_raw_mode()?;
        // Mouse capture goes on with the alternate screen and comes off with it. It is what makes
        // click/hover/wheel reach the fold at all; tmux scopes the grab to this pane (or popup),
        // so every other pane keeps its native selection and copy-mode drag.
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(e);
        }
        let guard = TerminalGuard {
            tmux,
            watch_pane: watch_pane.clone(),
        };
        if let (Some(pane), Some(pid)) = (watch_pane.as_deref(), advertise_pid) {
            let _ = ui::advertise_watch_pid(tmux, pane, pid);
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard<'_> {
    fn drop(&mut self) {
        // Unset the advertised pid first, before the terminal restore, so a restore hiccup
        // can't leave a stale pid advertised. Best-effort — a tmux-killed pane already destroyed
        // the pane-scoped option with the pane.
        if let Some(pane) = &self.watch_pane {
            let _ = ui::unadvertise_watch_pid(self.tmux, pane);
        }
        // Best-effort full restore: every step runs regardless of the previous one's result, so a
        // failure in one does not strand the terminal in raw mode, the alternate screen, or (worse,
        // because it outlives the process visibly) mouse-reporting mode.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), crossterm::cursor::Show);
    }
}
