//! The middle-tier SIGUSR1 nudge. A resident `tma watch` sidebar advertises its pid in the
//! pane-scoped `@tma_watch_pid` option; the always-installed `after-select-pane`/`-window` hooks
//! walk panes for it and SIGUSR1 each pid, and the sidebar refreshes on its next input-poll tick.
//!
//! Pane scope (never server) narrows the pid-recycle kill hazard (SIGUSR1 default-terminates): the
//! option dies with its pane, so only a sidebar killed without unsetting it leaves a residual window.
//! Local-user-only, and the `pid > 0` filter blocks the process-group fan-out. It lives in tier-2
//! runtime (not `tma-ui`) because both halves need signal/process syscalls, which `tma-ui` avoids.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Once};

use rustix::process::{kill_process, Pid, Signal};
use tma_core::stamp::opt;
use tma_tmux::tmux::Tmux;

/// Set by signal-hook's SIGUSR1 handler, drained by [`take_nudge`]. Process-global `Arc<AtomicBool>`
/// so `install`'s registration and `take_nudge`'s check-and-clear share one flag.
static NUDGED: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));
static INSTALLED: Once = Once::new();

/// Install the SIGUSR1 handler for a resident sidebar (via signal-hook). Idempotent. signal-hook
/// registers with `SA_RESTART`, which does NOT keep the event wait alive across a nudge (it never
/// restarts epoll/kqueue); correctness rests on crossterm retrying on `ErrorKind::Interrupted`
/// (verified 0.29.0), so the interruption is swallowed and `NUDGED` is read next tick.
pub fn install_nudge_handler() {
    INSTALLED.call_once(|| {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR1, NUDGED.clone());
    });
}

/// Check-and-clear the nudge flag: `true` iff a SIGUSR1 arrived since the last call. Worst-case
/// nudge latency is one ~200 ms poll interval, the design's accepted latency.
pub fn take_nudge() -> bool {
    NUDGED.swap(false, Ordering::Relaxed)
}

/// Signal every resident `tma watch` sidebar to refresh (the sender): walk panes for
/// `@tma_watch_pid` and SIGUSR1 each pid that parses and is `> 0`. Best-effort; a gone server or a
/// stale option (ESRCH) is tolerated silently, and every advertiser is signalled (multiple sidebars
/// are legal).
pub fn nudge_watchers(tmux: &Tmux) {
    // One snapshot read, not a per-kill re-read: re-reading before each kill would not narrow the
    // recycle window (the option clears only when the pane closes, not when the process exits), only
    // adding a fork+exec per pane. See the module docs for the residual.
    let Ok(panes) = tmux.list_pane_option(opt::WATCH_PID) else {
        return;
    };
    for (_pane, raw) in panes {
        // `pid > 0` (parse_watch_pid + Pid::from_raw) never signals a process group (`kill(0/-1, …)`).
        // Pane scope makes a recycled-pid signal far rarer (the option dies with the pane); failures
        // (ESRCH/EPERM) are ignored as a clean no-op.
        if let Some(p) = parse_watch_pid(&raw).and_then(Pid::from_raw) {
            let _ = kill_process(p, Signal::USR1);
        }
    }
}

/// Parse a `@tma_watch_pid` value into a signalable pid: `Some(pid)` iff a positive `i32`. Rejects
/// zero/negatives (`kill(0/-1, …)` fans out to process groups) and non-numeric junk. Shared with
/// [`crate::sidebar`], which reads the same advertisement to find a running sidebar.
pub(crate) fn parse_watch_pid(raw: &str) -> Option<i32> {
    raw.trim().parse::<i32>().ok().filter(|p| *p > 0)
}

#[cfg(test)]
mod tests {
    use super::parse_watch_pid;

    #[test]
    fn accepts_a_positive_pid() {
        assert_eq!(parse_watch_pid("4242"), Some(4242));
        // tmux option values can carry trailing whitespace/newlines from the format read.
        assert_eq!(parse_watch_pid(" 4242\n"), Some(4242));
    }

    #[test]
    fn rejects_zero_and_negatives() {
        // `kill(0, …)`/`kill(-1, …)` fan out to whole process groups — never a nudge target.
        assert_eq!(parse_watch_pid("0"), None);
        assert_eq!(parse_watch_pid("-1"), None);
        assert_eq!(parse_watch_pid("-4242"), None);
    }

    #[test]
    fn rejects_non_numeric_junk() {
        assert_eq!(parse_watch_pid(""), None);
        assert_eq!(parse_watch_pid("nope"), None);
        assert_eq!(parse_watch_pid("42x"), None);
    }
}
