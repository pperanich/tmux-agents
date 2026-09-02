use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tma_core::ActionManifest;
use tma_tmux::lock::{self, LockValue};
use tma_tmux::tmux::Tmux;

use super::{Outcome, PaneFacts, SupervisorSpec};

// ---- context env assembly ----------------------------------------------------------------------

/// Assemble the `TMA_*` context environment. Values cross the exec boundary as env only, with
/// no interpolation, so hostile pane text (title, session id) stays inert here; the how-to tells
/// authors to quote every expansion in their own script. The caller's `--arg` values ride the same
/// transport for the same reason: `TMA_ARG` for the first, `TMA_ARG_1..N` for the whole list.
pub(super) fn assemble_env(
    action: &ActionManifest,
    facts: &PaneFacts,
    pane_id: &str,
    agent: &str,
    args: &[String],
) -> Vec<(String, String)> {
    let mut env = vec![
        ("TMA_PANE".to_string(), pane_id.to_string()),
        ("TMA_AGENT".to_string(), agent.to_string()),
        ("TMA_STATE".to_string(), facts.state.token().to_string()),
        (
            "TMA_DETAIL".to_string(),
            facts.detail.clone().unwrap_or_default(),
        ),
        (
            "TMA_SESSION_ID".to_string(),
            facts.session.clone().unwrap_or_default(),
        ),
        ("TMA_CWD".to_string(), facts.cwd.clone()),
        ("TMA_PID".to_string(), facts.pid.clone().unwrap_or_default()),
        ("TMA_LOCATOR".to_string(), facts.locator.clone()),
        ("TMA_TITLE".to_string(), facts.title.clone()),
        ("TMA_ACTION".to_string(), action.name.clone()),
    ];
    // `TMA_ARG` is the first value even when several were passed, so the one-value case (the common
    // one) needs no counting; `TMA_ARG_COUNT` tells a script when there is more to read.
    if let Some(first) = args.first() {
        env.push(("TMA_ARG".to_string(), first.clone()));
        env.push(("TMA_ARG_COUNT".to_string(), args.len().to_string()));
        for (i, value) in args.iter().enumerate() {
            env.push((format!("TMA_ARG_{}", i + 1), value.clone()));
        }
    }
    env
}

/// The outcome of spawning a synchronous exec command.
pub(super) enum ExecOutcome {
    /// The child finished with this code (a signal death folds to `128 + signal`).
    Exited(i32),
    /// The child outlived `timeout_ms` and its process group was killed.
    Timeout,
    /// The spawn itself failed (no `sh`, etc.).
    SpawnError(String),
}

/// Spawn `command` via `sh -c` with the `TMA_*` context env, in tma's own working directory,
/// as its own process group so a timeout kill takes the whole tree. stdout/stderr pass
/// through (synchronous). Bounded by `timeout_ms`; on expiry the group is SIGKILLed and reaped.
pub(super) fn run_exec(command: &str, env: &[(String, String)], timeout_ms: u64) -> ExecOutcome {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    for (k, v) in env {
        cmd.env(k, v);
    }
    spawn_and_bound(cmd, timeout_ms)
}

/// Spawn `cmd` (already configured with its command + env/stdio) as its own process group so the
/// deadline kill reaches the whole subtree, then wait bounded by `timeout_ms`, SIGKILLing the group
/// and reaping on expiry. Shared by the synchronous [`run_exec`] and the detached supervisor.
fn spawn_and_bound(mut cmd: Command, timeout_ms: u64) -> ExecOutcome {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    // `process_group(0)`: the child leads a fresh group (pgid == its pid).
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ExecOutcome::SpawnError(format!("cannot spawn `sh -c`: {e}")),
    };
    let pgid = child.id() as i32;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // `code()` is None on a signal death; fold to the shell convention 128 + signal so a
                // signalled child still reports a stable numeric code.
                let code = status
                    .code()
                    .or_else(|| status.signal().map(|s| 128 + s))
                    .unwrap_or(1);
                return ExecOutcome::Exited(code);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_group(pgid);
                    let _ = child.wait(); // reap the killed child
                    return ExecOutcome::Timeout;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return ExecOutcome::SpawnError(format!("wait on child failed: {e}")),
        }
    }
}

/// SIGKILL a process group by its leader pid. Best-effort: a race where the group already exited
/// returns an error we ignore (the child is reaped by the caller regardless).
fn kill_group(pgid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

// ---- detached supervisor -----------------------------------------------------------------------

/// The real [`BrokerIo::spawn_supervisor`]: re-exec the tma binary in its internal `supervise` mode,
/// null stdio, forwarding the target server and the completion notify command. The `TMA_*` context env
/// is set on the child so the supervised command inherits it. The `Child` is dropped unwaited:
/// when this short-lived broker exits the supervisor reparents to init (which reaps it) — the same
/// detach discipline the daemon launcher uses.
pub(super) fn spawn_supervisor_process(
    server: &tma_tmux::tmux::Server,
    notify_command: Option<&str>,
    spec: &SupervisorSpec,
) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find the tma binary to supervise: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("supervise");
    server.forward_to(&mut cmd);
    cmd.arg("--pane")
        .arg(&spec.pane_id)
        .arg("--nonce")
        .arg(&spec.nonce)
        .arg("--expiry-ms")
        .arg(spec.expiry_ms.to_string())
        .arg("--name")
        .arg(&spec.action)
        .arg("--agent")
        .arg(&spec.agent)
        .arg("--command")
        .arg(&spec.command)
        .arg("--detach-timeout-ms")
        .arg(spec.detach_timeout_ms.to_string());
    if let Some(nc) = notify_command {
        cmd.arg("--notify-command").arg(nc);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map(|_child| ())
        .map_err(|e| format!("cannot spawn the action supervisor: {e}"))
}

/// The internal supervisor entry, driven by the hidden `tma supervise` subcommand. The broker
/// spawned this process under the held lock; here it takes custody, runs the one child bounded by the
/// wall-clock deadline, then releases the lock and fires the completion notification. Every step is
/// best-effort: a dead pane's option writes are harmless no-ops and the notification still fires.
pub struct SuperviseParams {
    pub pane_id: String,
    pub nonce: String,
    pub expiry_ms: u64,
    pub action: String,
    pub agent: String,
    pub command: String,
    pub detach_timeout_ms: u64,
    /// The completion notify command (config `notify.command`), or `None`. `TMA_NOTIFY_CMD` overrides.
    pub notify_command: Option<String>,
    /// The tty + audit-log sinks the completion rides. Resolved by the supervisor's own config
    /// load, not forwarded like `notify_command`: `--config` does not cross the re-exec, so a
    /// non-default config's sinks apply only if the supervisor resolves that file too.
    pub notify_sinks: crate::config::NotifySinks,
}

/// Run one detached action to completion: detach from the broker's session, take lock custody,
/// run the child under the deadline, release the lock, and fire the completion notification.
pub fn supervise(tmux: &Tmux, params: SuperviseParams) {
    // Detach from the broker's session/controlling terminal so a closed invoking client cannot signal
    // us for the child's (up to 15 min) lifetime. `EPERM` (already a session leader) is harmless; any
    // failure is non-fatal — we still supervise. Mirrors the daemon's detach discipline.
    let _ = rustix::process::setsid();

    // Lock custody handoff: rewrite the lock nonce-conditionally with OUR pid, keeping the
    // broker's expiry + nonce, so the reclaim liveness pre-check tracks this process. A `false`/`Err`
    // means the lock was already cleared or reclaimed (e.g. the pane died) — we still run the child;
    // the final clear is then a nonce-conditional no-op.
    let held = LockValue {
        expiry_ms: params.expiry_ms,
        nonce: params.nonce.clone(),
        pid: std::process::id(),
        name: params.action.clone(),
    };
    let _ = lock::rewrite(tmux, &params.pane_id, &params.nonce, &held);

    // Run the child in its own process group, killed at the wall-clock deadline (`detach_timeout_ms`).
    // stdout/stderr go to /dev/null (no files, no captured content); the `TMA_*` env was set on
    // this process by the broker's spawn, so `sh -c` inherits it.
    let outcome = match run_detached(&params.command, params.detach_timeout_ms) {
        ExecOutcome::Exited(code) => Outcome::Exited(code),
        ExecOutcome::Timeout => Outcome::Timeout,
        ExecOutcome::SpawnError(msg) => Outcome::Error(msg),
    };
    let exit_code = match &outcome {
        Outcome::Exited(code) => Some(*code),
        _ => None,
    };

    // Release the lock nonce-conditionally (a no-op if the pane died and its options went with it).
    // The supervisor's stderr is /dev/null (see spawn_supervisor_process), so a failure has no log
    // channel; instead it rides the completion notification below as `lock_release_failed`. The lock
    // is expiry-bounded and reclaimed on a dead pid, so it self-heals regardless. A dead pane makes
    // the option write fail, so this correlates with a null `locator` (the benign pane-gone case).
    let lock_release_failed = lock::clear(tmux, &params.pane_id, &params.nonce).is_err();

    // Completion notification: its own pinned payload; the locator is null when the pane is
    // gone. `TMA_NOTIFY_CMD` overrides the forwarded config command (the test/CI seam, as elsewhere).
    let command = std::env::var("TMA_NOTIFY_CMD")
        .ok()
        .filter(|s| !s.is_empty())
        .or(params.notify_command);
    let completion = crate::notify::CompletionNotification {
        action: params.action,
        pane: params.pane_id.clone(),
        agent: params.agent,
        outcome: outcome.token().to_string(),
        exit_code,
        locator: read_locator(tmux, &params.pane_id),
        lock_release_failed,
    };
    if let Some(mut child) =
        crate::notify::fire_completion(tmux, &completion, command.as_deref(), &params.notify_sinks)
    {
        // Judge the hook here, the one place its exit is visible: the supervisor writes to
        // /dev/null and nothing waits on it, so a hook exiting 127 would otherwise leave no trace.
        // The marker is shared with the state path, and the next clean fire clears it.
        if let Ok(status) = child.wait() {
            if let Some(cmd) = command.as_deref() {
                crate::notify::failure::record_exit(cmd, &status, crate::now_ms());
            }
        }
    }
}

/// Spawn the detached child via `sh -c` with stdout/stderr to `/dev/null`, inheriting this
/// supervisor's env (the `TMA_*` context the broker set). Bounded by `detach_timeout_ms`.
fn run_detached(command: &str, detach_timeout_ms: u64) -> ExecOutcome {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_and_bound(cmd, detach_timeout_ms)
}

/// The pane's `session:window.pane` locator, or `None` when the pane is gone (its `display-message`
/// read fails). The completion payload's `locator`.
fn read_locator(tmux: &Tmux, pane_id: &str) -> Option<String> {
    tmux.pane_format(pane_id, "#{session_name}:#{window_index}.#{pane_index}")
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ---- exec runner (real subprocess, no tmux) --------------------------------------------------

    #[test]
    fn run_exec_passes_through_exit_code() {
        assert!(matches!(
            run_exec("exit 7", &[], 5_000),
            ExecOutcome::Exited(7)
        ));
        assert!(matches!(
            run_exec("true", &[], 5_000),
            ExecOutcome::Exited(0)
        ));
    }

    #[test]
    fn run_exec_sets_the_context_env() {
        let env = vec![("TMA_ACTION".to_string(), "run".to_string())];
        // The child sees TMA_ACTION; a mismatch would exit 1.
        assert!(matches!(
            run_exec("test \"$TMA_ACTION\" = run", &env, 5_000),
            ExecOutcome::Exited(0)
        ));
    }

    /// The `--arg` values reach the child as env and only as env: `TMA_ARG` is the first value,
    /// `TMA_ARG_1..N` the whole list, `TMA_ARG_COUNT` how many there are. No values means no keys at
    /// all, so a script can tell "not passed" from "passed empty".
    #[test]
    fn assemble_env_carries_the_arg_values() {
        use tma_core::AgentState;

        let action = ActionManifest::parse(
            "min_engine_version = \"0.1\"\nname = \"queue\"\nlabel = \"Q\"\nkind = \"exec\"\ncommand = \"true\"\n",
            "queue",
            "queue.toml",
        )
        .unwrap();
        let facts = PaneFacts {
            agent: Some("claude".to_string()),
            state: AgentState::Idle,
            detail: None,
            session: None,
            cwd: "/repo".to_string(),
            pid: None,
            title: String::new(),
            locator: "s:0.0".to_string(),
            stamped_at: 1,
            context_pct: None,
            context_covered: false,
            permission_request: None,
            api_endpoint: None,
            episode_ms: 0,
            pending_tool: None,
            pending_call: None,
            act_repeat: None,
        };
        let get = |env: &[(String, String)], key: &str| {
            env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };

        let none = assemble_env(&action, &facts, "%1", "claude", &[]);
        assert!(
            !none.iter().any(|(k, _)| k.starts_with("TMA_ARG")),
            "no values, no TMA_ARG* keys"
        );

        let one = assemble_env(
            &action,
            &facts,
            "%1",
            "claude",
            &["review PR 412".to_string()],
        );
        assert_eq!(get(&one, "TMA_ARG").as_deref(), Some("review PR 412"));
        assert_eq!(get(&one, "TMA_ARG_1").as_deref(), Some("review PR 412"));
        assert_eq!(get(&one, "TMA_ARG_COUNT").as_deref(), Some("1"));

        let many = assemble_env(
            &action,
            &facts,
            "%1",
            "claude",
            &["first".to_string(), "$(rm -rf /)".to_string()],
        );
        assert_eq!(get(&many, "TMA_ARG").as_deref(), Some("first"));
        assert_eq!(get(&many, "TMA_ARG_2").as_deref(), Some("$(rm -rf /)"));
        assert_eq!(get(&many, "TMA_ARG_COUNT").as_deref(), Some("2"));
    }

    /// Shell metacharacters in a value stay data: the child sees the literal text because it crossed
    /// as env, never as part of the command string.
    #[test]
    fn run_exec_keeps_arg_values_inert() {
        let env = vec![("TMA_ARG".to_string(), "$(exit 9); rm -rf /".to_string())];
        assert!(matches!(
            run_exec("test \"$TMA_ARG\" = '$(exit 9); rm -rf /'", &env, 5_000),
            ExecOutcome::Exited(0)
        ));
    }

    #[test]
    fn run_exec_kills_at_timeout() {
        let start = Instant::now();
        assert!(matches!(
            run_exec("sleep 30", &[], 100),
            ExecOutcome::Timeout
        ));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the deadline kill must fire well before the child's own sleep"
        );
    }
}
