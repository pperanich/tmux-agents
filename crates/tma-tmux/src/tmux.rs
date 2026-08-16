//! The tmux subprocess choke point: every invocation in the workspace goes through this type's
//! `run`/`run_with_timeout`. CLI one-shots only, no control mode or connection lifecycle. The
//! parent holds the `Tmux` handle, spawn plumbing, and session/probe lifecycle; one submodule per
//! concern: `read` (list-panes/capture/ps, the poll cycle's whole input), `options` (pane/server
//! option writes behind the stamp guard), `hooks` (global hook management), `display` (menu,
//! focus, keys, bell).
//!
//! Every tmux invocation handles a gone server gracefully: [`TmuxError::ServerGone`] is a clean,
//! expected outcome distinct from a real failure.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

mod display;
mod hooks;
mod options;
mod read;

pub use display::{escape_menu_label, MenuItem};
pub use read::{normalize_comm, ps_all, PaneRecord};

/// Field separator for the `list-panes` format: US (`\x1f`) never appears in a pane title or command
/// in practice, so a plain split is unambiguous; the free-form title is placed last for extra safety.
const SEP: char = '\u{1f}';

/// One live session from `list-sessions` (the daemon's control-mode pool membership basis).
/// Defined here (not in the private `read` submodule) so the pub(crate) type stays reachable
/// crate-wide for the control-mode pool, which reads `.id` by inference without naming a path.
#[derive(Clone, Debug)]
pub(crate) struct SessionInfo {
    /// Stable `$N` id (survives renames); the pool keys and attaches on this.
    pub id: String,
}

/// Errors from the tmux/ps read path. `ServerGone` is an expected, clean exit.
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("cannot run `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no tmux server running")]
    ServerGone,
    #[error("`{cmd}` failed (exit {code}): {stderr}")]
    Failed {
        cmd: String,
        code: i32,
        stderr: String,
    },
    /// A one-shot exceeded [`TMUX_TIMEOUT`]: the tmux server is wedged (unresponsive socket). Callers
    /// treat it like [`TmuxError::Failed`] (a real error, not the clean `ServerGone`), but it names the wall cap.
    #[error("`{cmd}` timed out after {secs}s (tmux server unresponsive)")]
    Timeout { cmd: String, secs: u64 },
    /// No `tmux` binary on PATH at construction. Distinct from [`TmuxError::Spawn`] so the Display can carry an
    /// install hint operators act on, instead of a bare `No such file or directory`.
    #[error(
        "tmux is not installed or not on PATH; install tmux: `brew install tmux` (macOS) or your package manager"
    )]
    NotInstalled,
    /// The server rejected the client over a protocol version difference: the `tmux` we spawned was
    /// built from a different version than the server. tmate is the common case (its server speaks
    /// its own fork's protocol), as is a second tmux left over from another package manager. Its own
    /// arm, so the Display carries the fix instead of a bare `Failed` with tmux's terse line.
    #[error(
        "tmux protocol version mismatch: the `tmux` client and this server were built from different \
         versions (are you inside tmate, or is a second tmux first on PATH?); point tma at the \
         matching client with `[tmux] bin` in config.toml or TMA_TMUX_BIN"
    )]
    ProtocolMismatch,
    #[error("unexpected `{cmd}` output: {reason}")]
    Parse { cmd: String, reason: String },
}

/// Default wall-clock cap on one `tmux` one-shot. A wedged server (unresponsive socket) would
/// otherwise block the caller forever, and every sync one-shot runs through [`Tmux::run`]: the
/// ambient `tma status` driver, the `run-shell` focus hooks (which block the invoking client), `jump`,
/// and the poll cycle. This crate takes no config, so the cap is a constant, not a runtime knob. The
/// control-mode pool (control.rs) has its own bounded I/O and never routes through here.
const TMUX_TIMEOUT: Duration = Duration::from_secs(3);

/// Which tmux server to talk to: the socket selector every spawn prepends. `socket_name` is tmux's
/// `-L <name>` (a socket under tmux's own runtime dir), `socket_path` its `-S <path>` (an absolute
/// socket, which is how tmate and a hand-placed socket are reached). They are mutually exclusive —
/// the CLI rejects both at parse time — and neither set is the ambient/default server.
#[derive(Clone, Debug, Default)]
pub struct Server {
    pub socket_name: Option<String>,
    pub socket_path: Option<PathBuf>,
    /// The tmux-compatible binary to spawn: a PATH name or a path (anything containing a `/` is
    /// taken as a path and used as-is). `None` is plain `tmux`. Set from `[tmux] bin` /
    /// `TMA_TMUX_BIN`, which is how a tmate socket or a second tmux build is driven by its OWN
    /// client — the one thing a mismatched client cannot do.
    pub bin: Option<String>,
}

impl Server {
    /// The `-L`-only form, for the many callers that only ever carried a socket name.
    pub fn named(socket_name: Option<String>) -> Server {
        Server {
            socket_name,
            ..Server::default()
        }
    }

    /// The `-u …` server args every spawn prepends. `-u`: a client tmux deems non-UTF-8 (no TMUX
    /// var, no UTF-8 locale — launchd/cron/hook envs) gets utf8_sanitize()d output, turning the
    /// U+001F stamp separator into `_`.
    fn args(&self) -> Vec<String> {
        let mut args = vec!["-u".to_string()];
        if let Some(name) = &self.socket_name {
            args.push("-L".to_string());
            args.push(name.clone());
        } else if let Some(path) = &self.socket_path {
            args.push("-S".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        args
    }

    /// Append this target's flags to a child `tma` invocation (the daemon launcher, the detached
    /// action supervisor), so the child reaches the same server this process did.
    pub fn forward_to(&self, cmd: &mut Command) {
        if let Some(name) = &self.socket_name {
            cmd.arg("--socket-name").arg(name);
        } else if let Some(path) = &self.socket_path {
            cmd.arg("--socket-path").arg(path);
        }
    }

    /// The same flags as a fragment of a shell command line (the `display-menu` entries, which are
    /// strings tmux hands to `run-shell`). Empty for the ambient server; single-quoted, since a
    /// socket path may contain a space.
    pub fn shell_flag(&self) -> String {
        match (&self.socket_name, &self.socket_path) {
            (Some(name), _) => format!(" --socket-name {name}"),
            (None, Some(path)) => format!(" --socket-path '{}'", path.display()),
            (None, None) => String::new(),
        }
    }
}

/// A configured tmux client: `-u` plus the [`Server`] socket selector prepended to every call, so
/// tests can target a scratch server and no output is locale-sanitized (see [`Server::args`]).
pub struct Tmux {
    server_args: Vec<String>,
    /// The tmux binary resolved once at construction: `Some(path)` is the absolute path every spawn
    /// reuses (so PATH is not re-walked per call), `None` records a resolution miss so each call
    /// returns a guided [`TmuxError::NotInstalled`] instead of a bare per-spawn error.
    bin: Option<PathBuf>,
}

impl Tmux {
    /// `socket_name` maps to tmux `-L <name>`; `None` uses the ambient/default server. The
    /// name-only shorthand for [`Tmux::connect`].
    pub fn new(socket_name: Option<String>) -> Self {
        Self::connect(&Server::named(socket_name))
    }

    /// Talk to `server`. Resolves the `tmux` binary against PATH once, here, so the resolution cost
    /// and any miss are paid up front rather than per spawn.
    pub fn connect(server: &Server) -> Self {
        Self::with_bin(resolve_configured_binary(server.bin.as_deref()), server)
    }

    /// Construct with an already-resolved binary (`None` records a resolution miss). The seam the
    /// timeout and not-installed unit tests drive to point at a fake or absent binary without a server.
    fn with_bin(bin: Option<PathBuf>, server: &Server) -> Self {
        Tmux {
            server_args: server.args(),
            bin,
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, TmuxError> {
        self.run_with_timeout(args, TMUX_TIMEOUT)
    }

    /// Spawn one `tmux` one-shot, bounded by `timeout`. Sync (this crate runs no async runtime, see
    /// control.rs): a waiter thread owns the child plus a reader thread per pipe, and the main thread
    /// governs the whole join with `recv_timeout`. The readers own the pipes so a full pipe buffer can
    /// never deadlock the reap loop (the classic collect-on-the-wait-thread trap). On expiry the main
    /// thread only sets a kill flag; the waiter, the sole owner of the `Child`, does every kill and
    /// reap, so no reaped-and-recycled pid is ever signalled.
    fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Result<String, TmuxError> {
        let cmd_desc = || describe_argv(args);
        // Resolved once at construction: a miss is a guided NotInstalled, never a per-call PATH walk.
        let Some(bin) = self.bin.as_ref() else {
            return Err(TmuxError::NotInstalled);
        };

        let mut child = Command::new(bin)
            .args(&self.server_args)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| TmuxError::Spawn {
                cmd: cmd_desc(),
                source,
            })?;

        // The only path a timeout uses to stop the child: the main thread flips this, the waiter acts
        // on it. A raw kill-by-pid from here could hit a recycled pid after the waiter already reaped.
        let kill = Arc::new(AtomicBool::new(false));
        let waiter_kill = Arc::clone(&kill);

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // Each pipe is drained to EOF on its own thread; the reap loop below then never blocks
            // against a pipe buffer neither thread is draining.
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let out_reader = thread::spawn(move || drain(stdout));
            let err_reader = thread::spawn(move || drain(stderr));
            // Poll for exit instead of a blocking `wait()`, so the thread that owns the `Child` is the
            // one that observes the kill flag and delivers the kill itself, then reaps on a later tick.
            // Backoff from 1ms to a 25ms cap: a fast one-shot (the common case, on the status-line and
            // poll hot paths) returns within a millisecond or two, while a wedged child costs ~40 idle
            // wakes a second, not a tight spin.
            let mut backoff = Duration::from_millis(1);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Err(err) => break Err(err),
                    Ok(None) => {
                        if waiter_kill.load(Ordering::Relaxed) {
                            // `Child::kill` tracks its own wait status, so it cannot race a recycled
                            // pid; the next `try_wait` reaps the killed child (no zombie).
                            let _ = child.kill();
                        }
                        thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_millis(25));
                    }
                }
            };
            let stdout = out_reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            // The receiver may be gone (we timed out); a failed send is expected, so ignore it.
            let _ = tx.send((status, stdout, stderr));
        });

        let (status, stdout, stderr) = match rx.recv_timeout(timeout) {
            Ok(triple) => triple,
            Err(_) => {
                // Expired. Signal the waiter to kill and reap the child it owns, then return at once.
                // We do not join, so a wedged grandchild still holding a pipe cannot block this path.
                kill.store(true, Ordering::Relaxed);
                return Err(TmuxError::Timeout {
                    cmd: cmd_desc(),
                    secs: timeout.as_secs(),
                });
            }
        };

        let status = status.map_err(|source| TmuxError::Spawn {
            cmd: cmd_desc(),
            source,
        })?;
        if status.success() {
            return Ok(String::from_utf8_lossy(&stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        if is_server_gone(&stderr) {
            return Err(TmuxError::ServerGone);
        }
        if is_protocol_mismatch(&stderr) {
            return Err(TmuxError::ProtocolMismatch);
        }
        Err(TmuxError::Failed {
            cmd: cmd_desc(),
            code: status.code().unwrap_or(-1),
            stderr,
        })
    }

    /// Create a detached throwaway session running `shell_cmd`, returning `(session_id, pane_id)`.
    /// Daemon-only: the behavior probe owns and kills this marker-emitting session.
    pub(crate) fn new_probe_session(
        &self,
        name: &str,
        shell_cmd: &str,
    ) -> Result<(String, String), TmuxError> {
        let fmt = format!("#{{session_id}}{SEP}#{{pane_id}}");
        let out = self.run(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            &fmt,
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            shell_cmd,
        ])?;
        let line = out.lines().next().unwrap_or("");
        match line.split_once(SEP) {
            Some((sid, pid)) if !sid.is_empty() && !pid.is_empty() => {
                Ok((sid.to_string(), pid.to_string()))
            }
            _ => Err(TmuxError::Parse {
                cmd: "new-session".to_string(),
                reason: format!("unexpected output {line:?}"),
            }),
        }
    }

    /// Kill a session by id (`kill-session -t`). Daemon-only: tears down the probe session.
    pub(crate) fn kill_session(&self, session_id: &str) -> Result<(), TmuxError> {
        self.run(&["kill-session", "-t", session_id]).map(|_| ())
    }

    /// Build (not spawn) the argv for a control-mode client attached to `session_id`
    /// (`tmux <server-args> -C attach-session -t <session_id>`); the pool owns the child. `-C` (not
    /// `-CC`) is line-oriented: notifications arrive as `%…` lines on stdout.
    pub(crate) fn control_client_command(&self, session_id: &str) -> Command {
        // The same binary the one-shots resolved: a `[tmux] bin` override must reach control mode
        // too, or the daemon would attach with a different client than everything else spawns.
        let mut cmd = match self.bin.as_ref() {
            Some(bin) => Command::new(bin),
            None => Command::new("tmux"),
        };
        cmd.args(&self.server_args);
        cmd.args(["-C", "attach-session", "-t", session_id]);
        cmd
    }
}

/// Read a child pipe to EOF, discarding any read error (a killed child's pipe simply ends). Owns the
/// pipe by value and runs on its own thread per pipe, so draining can never block the waiter's `wait()`.
fn drain<R: Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    buf
}

/// Resolve the configured tmux binary: `None` is plain `tmux` off `PATH`, a value containing a `/`
/// is a path used as-is (verified executable), and anything else is a `PATH` name. A configured
/// binary that does not resolve stays a [`TmuxError::NotInstalled`], naming what to install rather
/// than failing per spawn.
fn resolve_configured_binary(bin: Option<&str>) -> Option<PathBuf> {
    match bin {
        None => resolve_binary("tmux"),
        Some(spec) if spec.contains('/') => {
            let path = PathBuf::from(spec);
            is_executable(&path).then_some(path)
        }
        Some(name) => resolve_binary(name),
    }
}

/// Resolve `name` to an absolute executable path by walking `PATH`, `None` when nothing matches. A
/// std-only replacement for the `which` crate (no new dependency): the resolution runs once per
/// [`Tmux`], so the linear walk is not a hot path. The returned path is always absolute, so the spawn
/// that reuses it cannot diverge from the candidate checked here even if the process CWD later moves.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        // Skip empty PATH components: execvp reads them as the current directory, and honouring that
        // would let the checked path resolve CWD-relative and then differ from what we spawn.
        .filter(|dir| !dir.as_os_str().is_empty())
        // Canonicalize the *directory* (resolving a relative entry like `.` to an absolute path), then
        // re-join the name. Canonicalizing the full path instead would follow a final-component symlink
        // and rewrite the basename, breaking a multicall shim (uutils/busybox) that dispatches on argv[0].
        .filter_map(|dir| dir.canonicalize().ok())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether `p` is a regular file with an execute bit set (unix-only crate). Used to accept a PATH
/// entry as a runnable binary rather than a same-named directory or a non-executable file.
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Render a tmux argv for an error's `Display`. For `send-keys` the trailing keystroke arguments are
/// the payload a user typed into a pane, so they are redacted to a char count before they can land in
/// stderr/logs on a failure; the subcommand, its flags, and the `-t` pane target stay visible because
/// that context is load-bearing for debugging. Every other subcommand keeps its full argv (`send-keys`
/// is this crate's only keystroke-carrying command; `paste-buffer` is never constructed here).
fn describe_argv(args: &[&str]) -> String {
    let rendered = match args.first() {
        Some(&"send-keys") => redact_payload_argv(args),
        _ => args.iter().map(|a| a.to_string()).collect(),
    };
    format!("tmux {}", rendered.join(" "))
}

/// Keep the subcommand and the leading flag block (tmux places all flags before positionals), with
/// `-t <pane>` kept whole so the target stays visible; redact every positional payload argument to
/// `[redacted N chars]`. A realistic key (`Enter`, `C-c`, `/compact`) does not lead with `-`, so the
/// flag scan stops at the first key and the whole payload is redacted.
fn redact_payload_argv(args: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if i == 0 {
            out.push(arg.to_string());
            i += 1;
        } else if arg.starts_with('-') {
            out.push(arg.to_string());
            if arg == "-t" && i + 1 < args.len() {
                out.push(args[i + 1].to_string());
                i += 2;
            } else {
                i += 1;
            }
        } else {
            break;
        }
    }
    for arg in &args[i..] {
        out.push(format!("[redacted {} chars]", arg.chars().count()));
    }
    out
}

/// Whether a tmux stderr indicates the server is gone. tmux phrases this a few ways
/// across versions; match the stable substrings.
fn is_server_gone(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no server running")
        || s.contains("error connecting")
        || s.contains("no such file or directory") && s.contains("tmux")
        || s.contains("failed to connect to server")
}

/// Whether a tmux stderr is the protocol-version rejection. tmux prints `protocol version mismatch
/// (client 8, server 7)`; tmate the same phrase with its own numbers. Matched on the stable
/// substring, like [`is_server_gone`].
fn is_protocol_mismatch(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("protocol version mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spawn_carries_dash_u_before_the_socket_selector() {
        // Both spawn paths (one-shots and the control-mode client) prepend server_args, so
        // asserting `-u` here covers both; see `Server::args` for why the flag matters.
        let scoped = Tmux::with_bin(None, &Server::named(Some("scratch".into())));
        assert_eq!(scoped.server_args, ["-u", "-L", "scratch"]);

        let ambient = Tmux::with_bin(None, &Server::default());
        assert_eq!(ambient.server_args, ["-u"]);

        let argv: Vec<String> = scoped
            .control_client_command("$1")
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            ["-u", "-L", "scratch", "-C", "attach-session", "-t", "$1"]
        );
    }

    /// A socket path is tmux's `-S`, and it forwards to a child `tma` and into a `run-shell` string
    /// as the same target, so a menu entry or a detached supervisor cannot land on another server.
    #[test]
    fn a_socket_path_target_is_dash_s_everywhere() {
        let by_path = Server {
            socket_path: Some(PathBuf::from("/tmp/tmate-501/x y")),
            ..Server::default()
        };
        assert_eq!(
            Tmux::with_bin(None, &by_path).server_args,
            ["-u", "-S", "/tmp/tmate-501/x y"]
        );
        assert_eq!(by_path.shell_flag(), " --socket-path '/tmp/tmate-501/x y'");

        let mut cmd = Command::new("tma");
        by_path.forward_to(&mut cmd);
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["--socket-path", "/tmp/tmate-501/x y"]);

        // The ambient server forwards nothing at all.
        let mut cmd = Command::new("tma");
        Server::default().forward_to(&mut cmd);
        assert_eq!(cmd.get_args().count(), 0);
        assert_eq!(Server::default().shell_flag(), "");
    }

    #[test]
    fn send_keys_error_redacts_payload_but_keeps_subcommand_and_target() {
        let desc = describe_argv(&["send-keys", "-t", "%3", "my-secret-token", "Enter"]);
        assert!(desc.contains("send-keys"), "subcommand dropped: {desc}");
        assert!(desc.contains("%3"), "pane target dropped: {desc}");
        assert!(!desc.contains("my-secret-token"), "payload leaked: {desc}");
        assert!(!desc.contains("Enter"), "payload key leaked: {desc}");
        assert!(
            desc.contains("[redacted"),
            "no redaction placeholder: {desc}"
        );
    }

    #[test]
    fn non_send_keys_command_keeps_full_argv() {
        // A non-payload command's full argv is load-bearing for debugging, so it is kept verbatim.
        let desc = describe_argv(&["list-panes", "-a", "-F", "#{pane_id}"]);
        assert_eq!(desc, "tmux list-panes -a -F #{pane_id}");
    }

    /// A configured binary is what gets spawned: a bare name resolves against PATH, a value with a
    /// slash is taken as a path, and a miss on either stays the guided NotInstalled.
    #[test]
    fn a_configured_binary_replaces_plain_tmux() {
        let Some(sh) = resolve_binary("sh") else {
            eprintln!("skip: no `sh` on PATH");
            return;
        };
        assert_eq!(resolve_configured_binary(Some("sh")), Some(sh.clone()));
        assert_eq!(
            resolve_configured_binary(Some(sh.to_str().unwrap())),
            Some(sh.clone()),
            "a value containing a slash is used as the path"
        );
        assert_eq!(resolve_configured_binary(Some("/no/such/tmux")), None);
        assert_eq!(resolve_configured_binary(Some("tma-nonexistent-xyz")), None);

        // Reaching Tmux: the configured binary is the one every spawn (and control mode) uses.
        let server = Server {
            bin: Some(sh.to_string_lossy().into_owned()),
            ..Server::default()
        };
        assert_eq!(Tmux::connect(&server).bin.as_ref(), Some(&sh));
        let argv0 = Tmux::connect(&server)
            .control_client_command("$1")
            .get_program()
            .to_string_lossy()
            .into_owned();
        assert_eq!(argv0, sh.to_string_lossy());
    }

    /// tmate (and a mismatched second tmux) fails with a protocol-version line; it gets its own arm
    /// so the message says what to do instead of quoting tmux at the user.
    #[test]
    fn protocol_mismatch_detection() {
        assert!(is_protocol_mismatch(
            "protocol version mismatch (client 8, server 7)"
        ));
        assert!(is_protocol_mismatch("Protocol version mismatch"));
        assert!(!is_protocol_mismatch("no server running on /tmp/x"));
        assert!(!is_protocol_mismatch("usage: list-panes"));
        // The Display names the escape hatch, the way NotInstalled names the install.
        let msg = TmuxError::ProtocolMismatch.to_string();
        assert!(
            msg.contains("TMA_TMUX_BIN") && msg.contains("tmate"),
            "{msg}"
        );
    }

    #[test]
    fn server_gone_detection() {
        assert!(is_server_gone("no server running on /tmp/tmux-501/default"));
        assert!(is_server_gone(
            "error connecting to /tmp/tmux-501/foo (No such file)"
        ));
        assert!(!is_server_gone("usage: list-panes"));
    }

    #[test]
    fn resolve_binary_finds_present_and_misses_absent() {
        // `sh` exists on any unix and resolves to a runnable, absolute path (so the stored path the
        // spawn reuses is CWD-independent); a bogus name never resolves.
        let sh = resolve_binary("sh").expect("sh should resolve on unix");
        assert!(
            sh.is_absolute(),
            "resolved path must be absolute, got {sh:?}"
        );
        assert!(resolve_binary("tma-nonexistent-binary-xyz").is_none());
    }

    #[test]
    fn missing_binary_reports_not_installed() {
        // A construction-time resolution miss (`None`) surfaces as NotInstalled without ever spawning,
        // and its Display carries the install hint operators act on.
        let tmux = Tmux::with_bin(None, &Server::default());
        let err = tmux
            .run_with_timeout(&["list-sessions"], Duration::from_secs(1))
            .unwrap_err();
        assert!(matches!(err, TmuxError::NotInstalled), "got {err:?}");
        assert!(err.to_string().contains("install tmux"));
    }

    #[test]
    fn slow_command_times_out_without_hanging() {
        // `sh -c 'sleep 30'` stands in for a wedged tmux server (sh swallows the prepended `-u`
        // as nounset); the call must return on the 150ms timeout, so the suite never hangs.
        let Some(sh) = resolve_binary("sh") else {
            eprintln!("skip: no `sh` on PATH");
            return;
        };
        let tmux = Tmux::with_bin(Some(sh), &Server::default());
        let start = std::time::Instant::now();
        let err = tmux
            .run_with_timeout(&["-c", "sleep 30"], Duration::from_millis(150))
            .unwrap_err();
        assert!(matches!(err, TmuxError::Timeout { .. }), "got {err:?}");
        // Sanity ceiling only: the call must return on the timeout, not the 30s sleep. The bound is
        // deliberately loose so a loaded CI box (scheduler stalls dwarfing the 150ms budget) is safe.
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "the timed-out call must return promptly, took {:?}",
            start.elapsed()
        );
    }
}
