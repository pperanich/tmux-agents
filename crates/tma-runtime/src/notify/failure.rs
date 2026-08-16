//! The notify-command failure marker. [`fire`](super::fire) is spawn-and-forget with the child's
//! output discarded, so a hook command that cannot start (a typo'd path, a non-executable file) or
//! that exits non-zero is otherwise completely invisible: the notification simply never arrives. The
//! smallest honest record is one small file in the runtime dir both fire paths already own; `tma
//! doctor` reads it and the next clean fire removes it, so a fixed sink stops being reported.

use std::path::PathBuf;
use std::process::ExitStatus;

/// The marker's filename under [`crate::ipc::runtime_dir`]. One per user rather than per server: the
/// notify command is a user-level config, and the last failure is what a report needs.
const MARKER: &str = "notify-error";

/// A recorded notify-command failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyFailure {
    /// Wall-clock epoch (ms) the failure was recorded at.
    pub at: u64,
    /// Why it failed: `spawn failed: ...`, `exited 127`, or `killed by signal 9`.
    pub reason: String,
    /// The command that failed, as configured.
    pub command: String,
}

/// Record a failure, replacing any earlier one (the newest failure is the useful one). Best-effort:
/// a runtime dir that cannot be created or written is simply not reported.
pub fn record(command: &str, reason: &str, at: u64) {
    let Some(path) = marker_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        render(&NotifyFailure {
            at,
            reason: reason.to_string(),
            command: command.to_string(),
        }),
    );
}

/// Drop the marker after a clean fire, so a sink that has been fixed stops being reported.
pub fn clear() {
    if let Some(path) = marker_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Record or clear the marker from a command's finished status: a clean exit clears, anything else
/// records. The one place an awaited notify child's outcome is judged, so the daemon's reap, the
/// daemonless fire, and `tma debug notify-test` cannot disagree on what counts as a failure.
pub fn record_exit(command: &str, status: &ExitStatus, at: u64) {
    match exit_reason(status) {
        Some(reason) => record(command, &reason, at),
        None => clear(),
    }
}

/// The last recorded failure, `None` when the marker is absent or unreadable.
pub fn last() -> Option<NotifyFailure> {
    parse(&std::fs::read_to_string(marker_path()?).ok()?)
}

/// Why a finished child counts as a failure, or `None` when it exited cleanly. A signal death is
/// distinguished because it usually means the command hung and something (the daemon's in-flight cap)
/// killed it, which is a different fix than a non-zero exit.
pub(crate) fn exit_reason(status: &ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    if status.success() {
        return None;
    }
    match (status.code(), status.signal()) {
        (Some(code), _) => Some(format!("exited {code}")),
        (None, Some(sig)) => Some(format!("killed by signal {sig}")),
        (None, None) => Some("failed".to_string()),
    }
}

fn marker_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = private_dir() {
        return Some(dir.join(MARKER));
    }
    Some(crate::ipc::runtime_dir().join(MARKER))
}

// Test seam: the marker is one file per user, so two tests firing a failing command in parallel
// otherwise overwrite (or clear) each other's record. A thread-local redirect gives each test its
// own copy, since libtest runs every test on its own thread. Never set outside tests.
#[cfg(test)]
thread_local! {
    static PRIVATE_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn private_dir() -> Option<PathBuf> {
    PRIVATE_DIR.with(|d| d.borrow().clone())
}

/// Redirects this thread's marker into a private directory until dropped, then removes it.
#[cfg(test)]
pub(crate) struct PrivateMarker(PathBuf);

#[cfg(test)]
impl PrivateMarker {
    pub(crate) fn new(tag: &str) -> PrivateMarker {
        let dir = std::env::temp_dir().join(format!(
            "tma-notify-marker-{}-{tag}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("the private marker dir");
        PRIVATE_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
        PrivateMarker(dir)
    }
}

#[cfg(test)]
impl Drop for PrivateMarker {
    fn drop(&mut self) {
        PRIVATE_DIR.with(|d| *d.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Render the marker body: `key=value` lines, so it stays greppable and a future key is additive.
/// The command goes last, since it is the only field that can carry anything.
fn render(f: &NotifyFailure) -> String {
    format!(
        "at={}\nreason={}\ncommand={}\n",
        f.at,
        f.reason.replace('\n', " "),
        f.command.replace('\n', " ")
    )
}

/// Parse a marker body. `None` unless it carries all three keys, so a truncated write (a crash
/// mid-write) reports nothing rather than half a failure.
fn parse(body: &str) -> Option<NotifyFailure> {
    let mut at = None;
    let mut reason = None;
    let mut command = None;
    for line in body.lines() {
        match line.split_once('=') {
            Some(("at", v)) => at = v.trim().parse().ok(),
            Some(("reason", v)) => reason = Some(v.to_string()),
            Some(("command", v)) => command = Some(v.to_string()),
            _ => {}
        }
    }
    Some(NotifyFailure {
        at: at?,
        reason: reason?,
        command: command?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_body_round_trips() {
        let f = NotifyFailure {
            at: 1_700_000_000_000,
            reason: "exited 127".to_string(),
            command: "~/.local/bin/tma-notify".to_string(),
        };
        assert_eq!(parse(&render(&f)), Some(f));
    }

    #[test]
    fn a_multiline_command_stays_one_record() {
        // A `command` spanning lines would otherwise produce a body that parses back as a truncated
        // record; newlines are folded to spaces on the way in.
        let f = NotifyFailure {
            at: 1,
            reason: "spawn failed: No such file or directory".to_string(),
            command: "printf a\nprintf b".to_string(),
        };
        let back = parse(&render(&f)).expect("still one record");
        assert_eq!(back.command, "printf a printf b");
        assert_eq!(back.at, 1);
    }

    #[test]
    fn a_truncated_marker_reports_nothing() {
        assert!(parse("at=5\nreason=exited 1\n").is_none());
        assert!(parse("").is_none());
        assert!(parse("at=not-a-number\nreason=x\ncommand=y\n").is_none());
    }

    #[test]
    fn a_clean_exit_is_not_a_failure() {
        use std::process::Command;
        let ok = Command::new("sh").args(["-c", "exit 0"]).status().unwrap();
        assert_eq!(exit_reason(&ok), None);
        let bad = Command::new("sh")
            .args(["-c", "exit 127"])
            .status()
            .unwrap();
        assert_eq!(exit_reason(&bad).as_deref(), Some("exited 127"));
    }
}
