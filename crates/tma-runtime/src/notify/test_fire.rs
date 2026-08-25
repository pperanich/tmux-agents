//! `tma debug notify-test`: run the notify command a given trigger resolves to, against a
//! representative payload, and report what happened. The one notify path that WAITS and keeps the
//! child's stderr — a real fire is spawn-and-forget with its output discarded, which is exactly why a
//! broken hook is hard to diagnose. It reads no tmux state, so it runs outside a session too; the
//! outcome updates the same failure marker a real fire does.

use std::io::Read;
use std::process::Stdio;

use tma_core::stamp::opt;
use tma_tmux::tmux::PaneRecord;

use super::{
    failure, hook_command, notification_for, payload_json, TitlePolicy, CONTEXT_HIGH_WORD,
};
use crate::config::{NotifyCommands, NotifySinks, NotifyTrigger};

/// Which trigger a test fire impersonates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestTrigger {
    Blocked,
    Done,
    ContextHigh,
}

impl std::str::FromStr for TestTrigger {
    type Err = String;

    fn from_str(s: &str) -> Result<TestTrigger, String> {
        match s {
            "blocked" => Ok(TestTrigger::Blocked),
            "done" => Ok(TestTrigger::Done),
            "context_high" => Ok(TestTrigger::ContextHigh),
            other => Err(format!(
                "unknown trigger {other:?} (blocked|done|context_high)"
            )),
        }
    }
}

impl TestTrigger {
    /// The payload's `state` word for this trigger.
    fn word(self) -> &'static str {
        match self {
            TestTrigger::Blocked => NotifyTrigger::Blocked.word(),
            TestTrigger::Done => NotifyTrigger::Done.word(),
            TestTrigger::ContextHigh => CONTEXT_HIGH_WORD,
        }
    }

    /// The command this trigger resolves to under the configured routing.
    fn command(self, commands: &NotifyCommands) -> Option<&str> {
        match self {
            TestTrigger::Blocked => commands.for_trigger(NotifyTrigger::Blocked),
            TestTrigger::Done => commands.for_trigger(NotifyTrigger::Done),
            TestTrigger::ContextHigh => commands.for_context_high(),
        }
    }
}

/// What a test fire produced.
pub struct NotifyTest {
    /// The command the trigger resolved to, `None` when nothing is configured for it.
    pub command: Option<String>,
    /// The exact JSON handed to the command on stdin.
    pub payload: String,
    /// The child's exit code, `None` when it was killed by a signal or could not be spawned.
    pub code: Option<i32>,
    /// Why the fire failed, `None` when it exited cleanly (or there was nothing to run).
    pub error: Option<String>,
    /// Whatever the command wrote to stderr.
    pub stderr: String,
}

impl NotifyTest {
    /// Did the fire deliver? `false` for an unconfigured trigger and for any non-clean exit.
    pub fn delivered(&self) -> bool {
        self.command.is_some() && self.error.is_none()
    }
}

/// Resolve `trigger`'s command and run it to completion against a representative payload, capturing
/// stderr (the child's stdout is inherited, so a command that prints is visible). The outcome updates
/// the same failure marker a real fire does, so a passing test clears a stale report and a failing one
/// leaves the record `tma doctor` shows.
///
/// Takes `sinks` as well as `commands` so the printed payload is byte-for-byte what a real fire
/// sends under this configuration — including whether `[notify] include_title` let the pane title
/// out. A test fire that showed a title the real one redacts would be worse than no test at all.
pub fn notify_test(
    commands: &NotifyCommands,
    sinks: &NotifySinks,
    trigger: TestTrigger,
    now: u64,
) -> NotifyTest {
    let n = sample_notification(trigger, now);
    let title = TitlePolicy::from_include(sinks.include_title);
    let payload = payload_json(&n, title);
    let Some(cmd) = trigger.command(commands).map(str::to_string) else {
        return NotifyTest {
            command: None,
            payload,
            code: None,
            error: None,
            stderr: String::new(),
        };
    };

    let mut command = hook_command(&cmd, &n, title);
    command.stdin(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let reason = format!("spawn failed: {err}");
            failure::record(&cmd, &reason, now);
            return NotifyTest {
                command: Some(cmd),
                payload,
                code: None,
                error: Some(reason),
                stderr: String::new(),
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(payload.as_bytes());
    }
    // Read stderr before waiting: a command writing more than a pipe buffer would otherwise block on
    // a full pipe while we block on its exit.
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    match child.wait() {
        Ok(status) => {
            failure::record_exit(&cmd, &status, now);
            NotifyTest {
                command: Some(cmd),
                payload,
                code: status.code(),
                error: failure::exit_reason(&status),
                stderr,
            }
        }
        Err(err) => {
            let reason = format!("wait failed: {err}");
            failure::record(&cmd, &reason, now);
            NotifyTest {
                command: Some(cmd),
                payload,
                code: None,
                error: Some(reason),
                stderr,
            }
        }
    }
}

/// A representative notification: a synthetic pane carrying a plausible value for every payload
/// field, with the repo labels resolved from the current working directory so a hook sees the real
/// shape it will get. No tmux read, so this works outside a session.
fn sample_notification(trigger: TestTrigger, now: u64) -> super::Notification {
    let mut options = std::collections::HashMap::new();
    options.insert(opt::CONTEXT_PCT.to_string(), "80".to_string());
    let rec = PaneRecord {
        pane_id: "%0".to_string(),
        pane_pid: std::process::id(),
        session: "tma-notify-test".to_string(),
        window_index: 0,
        pane_index: 0,
        current_command: "claude".to_string(),
        window_activity: 0,
        alternate_on: false,
        scroll_position: None,
        pane_height: 40,
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string()),
        options,
        window_summary: None,
        session_summary: None,
        title: "notify-test".to_string(),
    };
    let detail = matches!(trigger, TestTrigger::Blocked).then(|| "permission".to_string());
    notification_for(
        &rec,
        "claude",
        trigger.word(),
        detail,
        Some("tma-notify-test".to_string()),
        now.saturating_sub(1_500),
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic wall-clock `now`, so the sample's `since` does not clamp at the epoch.
    const NOW: u64 = 1_700_000_000_000;

    fn commands(cmd: &str) -> NotifyCommands {
        NotifyCommands {
            global: Some(cmd.to_string()),
            ..NotifyCommands::default()
        }
    }

    #[test]
    fn trigger_parses_the_three_words() {
        assert_eq!("blocked".parse(), Ok(TestTrigger::Blocked));
        assert_eq!("done".parse(), Ok(TestTrigger::Done));
        assert_eq!("context_high".parse(), Ok(TestTrigger::ContextHigh));
        assert!("finished".parse::<TestTrigger>().is_err());
    }

    #[test]
    fn a_clean_command_reads_the_payload_and_reports_delivery() {
        // A clean fire CLEARS the shared marker, which would delete a sibling test's record.
        let _marker = failure::PrivateMarker::new("clean-fire");
        // The command consumes the payload and exits 0: delivered, no error, exit code 0.
        let out = notify_test(
            &commands("cat > /dev/null"),
            &NotifySinks::default(),
            TestTrigger::Blocked,
            NOW,
        );
        assert!(out.delivered(), "error: {:?}", out.error);
        assert_eq!(out.code, Some(0));
        assert!(out.payload.contains(r#""state":"blocked""#));
        assert!(out.payload.contains(r#""detail":"permission""#));
        // The episode age is representative, not zero, so a hook's formatting is exercised.
        assert!(out.payload.contains(r#""since_ms":1500"#));
    }

    #[test]
    fn a_failing_command_reports_its_code_and_stderr() {
        let _marker = failure::PrivateMarker::new("failing-fire");
        let out = notify_test(
            &commands("echo boom >&2; exit 3"),
            &NotifySinks::default(),
            TestTrigger::Done,
            NOW,
        );
        assert!(!out.delivered());
        assert_eq!(out.code, Some(3));
        assert_eq!(out.error.as_deref(), Some("exited 3"));
        assert_eq!(out.stderr.trim(), "boom");
        assert!(out.payload.contains(r#""state":"done""#));
    }

    #[test]
    fn an_unconfigured_trigger_runs_nothing() {
        let out = notify_test(
            &NotifyCommands::default(),
            &NotifySinks::default(),
            TestTrigger::ContextHigh,
            NOW,
        );
        assert!(out.command.is_none());
        assert!(!out.delivered());
        assert!(out.error.is_none(), "nothing ran, so nothing failed");
        // The payload is still built, so the user sees what a hook would receive.
        assert!(out.payload.contains(r#""state":"context_high""#));
        assert!(out.payload.contains(r#""context_pct":80"#));
    }
}
