//! Shared scaffolding for the tmux-touching integration tests.
//!
//! Every test drives an isolated scratch `tmux -L <socket>` server started with `-f /dev/null`, so
//! the user's real `~/.tmux.conf`, options, hooks, and key-tables never perturb it (the "no real
//! config touched" guarantee). Each `Scratch` owns a process-globally-unique socket +
//! workdir, so the whole workspace suite runs green under parallel `cargo test`. Dev-dependency-only
//! (`publish = false`): shared by four crates' integration tests, so a harness fix lands in one place.
#![deny(rustdoc::broken_intra_doc_links)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A cross-process serialization gate for the heavy daemon-spawning integration tests: an exclusive
/// `flock(2)` on one lock file so at most one such test runs at a time ACROSS ALL binaries (else a
/// dozen fork-storm in parallel and blow the timing windows). `flock` releases when the fd closes.
#[must_use = "the daemon-test gate is released as soon as the guard is dropped"]
pub struct DaemonTestGuard {
    // Dropping the File closes the fd, which releases the flock. That is the whole release path;
    // no explicit LOCK_UN is needed.
    _file: File,
}

impl DaemonTestGuard {
    /// Acquire the global daemon-test gate, blocking until no other daemon-spawning test holds it.
    /// Call this as the FIRST line of every test that spawns a `tma daemon`; hold it for the test.
    pub fn acquire() -> DaemonTestGuard {
        let path = std::env::temp_dir().join("tma-test-daemon.lock");
        // The file is only a handle to flock; its bytes are never read or written, so keep any
        // existing content (truncate(false)) rather than rewriting it.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap_or_else(|e| panic!("open daemon-test lock {}: {e}", path.display()));
        // Block on an exclusive advisory lock. flock can return EINTR when a signal interrupts
        // the wait, so retry rather than mistaking a spurious wakeup for a real error.
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => panic!("flock daemon-test lock: {e}"),
            }
        }
        DaemonTestGuard { _file: file }
    }
}

/// A process-globally-unique id: pid (distinct test binaries) + an atomic counter (parallel tests in
/// one binary, else a coarse-clock collision lets one Scratch's Drop wipe another's) + nanos.
pub fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

/// A short, filesystem-safe hash of a string, for keying a `/tmp` runtime dir under the unix-socket
/// path cap (see [`Scratch::new_daemon`]). The input is already unique, so this only shortens it.
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// Absolute path to the compiled `tma` binary for tests OUTSIDE the `tma` package (no
/// `CARGO_BIN_EXE_tma`); it sits at the workspace's shared `<target>/<profile>/tma`. A single-crate
/// run does not build it, so build on demand via `cargo build -p tma` (cargo owns staleness, no deadlock).
pub fn tma_bin() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(build_tma_bin).clone()
}

fn build_tma_bin() -> String {
    let mut p = std::env::current_exe().expect("current test exe path");
    p.pop(); // drop the test-runner filename → .../deps
    p.pop(); // drop `deps` → .../<profile>

    // The profile dir is `release` under `cargo test --release`, `debug` otherwise (the dev
    // profile). Match it so the on-demand build targets the same profile the runner was built with;
    // a custom `--profile` dir falls through to the default (debug) build.
    let release = p.file_name().map(|n| n == "release").unwrap_or(false);
    p.push("tma");
    let bin = p.to_string_lossy().into_owned();

    // `cargo test` sets CARGO to the cargo that launched us; standalone use falls back to PATH.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.arg("build").arg("-p").arg("tma");
    if release {
        cmd.arg("--release");
    }
    // Capture output so the normal green path stays quiet; surface it only when the build fails.
    match cmd.output() {
        Ok(out) if out.status.success() => bin, // cargo guarantees the bin is now current
        Ok(out) => panic!(
            "`cargo build -p tma` failed ({}) while preparing the integration-test binary.\n\
             Fix the build, then re-run. cargo stderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        ),
        Err(e) => {
            // Could not even spawn cargo (no cargo on PATH). Fail loud on an absent binary;
            // otherwise use the one already on disk (freshness unverifiable without cargo).
            if !Path::new(&bin).exists() {
                panic!(
                    "could not run `cargo build -p tma` ({e}) and no tma binary exists at {bin}.\n\
                     Build it first: `cargo build -p tma` (or run `cargo test --workspace`)."
                );
            }
            bin
        }
    }
}

/// The fail-closed gate: `TMA_REQUIRE_TMUX=1` flips a missing tmux/python3 from a green skip into a
/// panic, so CI cannot report all-green while silently running none of the tmux-touching suites.
fn require_tmux() -> bool {
    std::env::var("TMA_REQUIRE_TMUX").as_deref() == Ok("1")
}

/// Whether `tmux` is on PATH. Every integration test gates its scratch-server body on this. When tmux
/// is absent AND `TMA_REQUIRE_TMUX=1` is set it panics instead of returning `false`, so CI hard-fails
/// rather than skip-greening; without the env var it returns the plain availability bool (today's skip).
pub fn tmux_available() -> bool {
    let ok = Command::new("tmux").arg("-V").output().is_ok();
    if !ok && require_tmux() {
        panic!("tmux required by TMA_REQUIRE_TMUX=1 but not found on PATH");
    }
    ok
}

/// Whether `python3` (the PTY-attach helper's interpreter) is on PATH, gated by `TMA_REQUIRE_TMUX=1`
/// like [`tmux_available`]: under the env var an absent python3 panics rather than skip-greening.
pub fn python3_available() -> bool {
    let ok = Command::new("python3").arg("--version").output().is_ok();
    if !ok && require_tmux() {
        panic!("python3 required by TMA_REQUIRE_TMUX=1 but not found on PATH");
    }
    ok
}

/// Drop the ambient tmux session from a scratch child's environment.
///
/// A suite run from inside tmux exports `TMUX_PANE` (say `%3`). tmux resolves a command's default
/// target through it (`cmd_find_inside_pane` falls back to the client's `TMUX_PANE`) by pane id
/// *string*, never checking that the id came from the server being addressed. On a scratch server
/// that has its own `%3` every targetless read then silently retargets to that pane, so `tma jump`
/// resolves its origin to the wrong session. The pane id of the terminal running the tests means
/// nothing on a scratch server, so no scratch child may see it.
fn without_ambient_tmux(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("TMUX").env_remove("TMUX_PANE")
}

/// Run a tmux command against the scratch `-L` socket with `-f /dev/null` (isolates the server: no
/// config loaded). The server-spawning `new-session` retries once on a transient parallel-startup fail.
pub fn scratch_tmux(socket: &str, args: &[&str]) -> std::process::Output {
    let run = || {
        without_ambient_tmux(&mut Command::new("tmux"))
            .arg("-L")
            .arg(socket)
            .arg("-f")
            .arg("/dev/null")
            .args(args)
            .output()
            .expect("spawn tmux")
    };
    let out = run();
    if !out.status.success() && args.first() == Some(&"new-session") {
        std::thread::sleep(std::time::Duration::from_millis(50));
        return run();
    }
    out
}

/// The directory tmux keeps `-L` sockets in (`<TMUX_TMPDIR-or-/tmp>/tmux-<uid>`).
fn scratch_socket_dir() -> PathBuf {
    let base = std::env::var_os("TMUX_TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // tmux keys the directory on the real uid; getuid never fails.
    let uid = rustix::process::getuid().as_raw();
    base.join(format!("tmux-{uid}"))
}

/// The on-disk path of a scratch server's socket file (`<TMUX_TMPDIR-or-/tmp>/tmux-<uid>/<socket>`),
/// derived so [`cleanup_scratch_socket`] can delete what `kill-server` leaves behind.
fn scratch_socket_path(socket: &str) -> PathBuf {
    scratch_socket_dir().join(socket)
}

/// Remove a scratch server's socket file after `kill-server`, which does not reliably unlink the
/// inode (notably on macOS); else dead sockets accumulate and slow creation into timing flake.
///
/// Never unlinks while a process still holds the name. The socket file is what [`reap_orphans`]
/// walks, so removing it out from under a survivor makes that server unreachable AND invisible: it
/// then lives until reboot, which is how a machine ends up with a hundred of them.
pub fn cleanup_scratch_socket(socket: &str) {
    if scratch_processes().iter().any(|(_, s)| s == socket) {
        return;
    }
    let _ = std::fs::remove_file(scratch_socket_path(socket));
}

/// Every live process whose argv names a `-L tma_test_…` socket, as `(pid, socket)`. Both the server
/// and any leaked control client match, which is intended: either is load the next run pays for.
fn scratch_processes() -> Vec<(u32, String)> {
    let Ok(out) = Command::new("ps").args(["-eo", "pid=,command="]).output() else {
        return Vec::new(); // no `ps` (a stripped container): the socket sweep still runs
    };
    parse_ps_sockets(&String::from_utf8_lossy(&out.stdout))
}

/// The `(pid, socket)` pairs in `ps -eo pid=,command=` output, split out for its own test.
fn parse_ps_sockets(text: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(Ok(pid)) = fields.first().map(|p| p.parse::<u32>()) else {
            continue;
        };
        // The socket is the token after `-L`; a tmux command line carries at most one.
        let Some(socket) = fields
            .iter()
            .position(|f| *f == "-L")
            .and_then(|i| fields.get(i + 1))
        else {
            continue;
        };
        if socket.starts_with("tma_test_") {
            found.push((pid, (*socket).to_string()));
        }
    }
    found
}

/// SIGKILL, ignoring every error: the process may have exited between the scan and here, and a drop
/// path must never panic.
fn kill_pid(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else { return };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return;
    };
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

/// Kill a scratch server without ever panicking: a drop path runs while a failed test is unwinding,
/// where a panic would abort the whole binary and leak every other scratch server. Every harness
/// Drop must go through this rather than a `.expect`-ing tmux call.
///
/// `kill-server` is cooperative and its exit status says only that the request was delivered: a
/// server whose control-mode client has lost its reader (a daemon test's client, orphaned when the
/// daemon died) acknowledges and then never finishes tearing down. So the holders are noted first,
/// waited out, and SIGKILLed if the polite path did not take.
pub fn kill_scratch_server(socket: &str) {
    let holders: Vec<u32> = scratch_processes()
        .into_iter()
        .filter(|(_, s)| s == socket)
        .map(|(pid, _)| pid)
        .collect();
    let _ = without_ambient_tmux(&mut Command::new("tmux"))
        .arg("-L")
        .arg(socket)
        .arg("-f")
        .arg("/dev/null")
        .arg("kill-server")
        .output();
    // Signal-0 probes rather than another `ps`: the pids are already known, and the healthy case
    // exits within a tick or two, so this costs nothing when the kill worked.
    for _ in 0..KILL_SETTLE_POLLS {
        if holders.iter().all(|pid| !pid_is_live(*pid)) {
            return;
        }
        std::thread::sleep(KILL_SETTLE_STEP);
    }
    for pid in holders {
        if pid_is_live(pid) {
            kill_pid(pid);
        }
    }
}

/// How long [`kill_scratch_server`] waits out a cooperative `kill-server` before SIGKILL: 25 × 20ms.
/// Long enough that a healthy server is never killed mid-teardown, short enough to not pad a suite.
const KILL_SETTLE_POLLS: u32 = 25;
const KILL_SETTLE_STEP: Duration = Duration::from_millis(20);

/// The pid embedded in a scratch socket name (`tma_test_<tag>_<pid>_<nanos>_<counter>`, see
/// [`unique_id`]); [`None`] for any name this harness did not create. Anchored on the nanosecond
/// stamp (16+ digits, which no pid is) rather than on field positions, since tags carry underscores
/// of their own (`act_menu_noclient`) and a name may be extended on the right.
fn socket_owner_pid(socket: &str) -> Option<u32> {
    let rest = socket.strip_prefix("tma_test_")?;
    let parts: Vec<&str> = rest.split('_').collect();
    let nanos = parts
        .iter()
        .rposition(|p| p.len() >= 16 && p.bytes().all(|b| b.is_ascii_digit()))?;
    parts.get(nanos.checked_sub(1)?)?.parse().ok()
}

/// Whether `pid` still exists (a signal-0 probe). Anything but ESRCH counts as alive — EPERM means
/// the process is there and simply not ours — so the caller only ever acts on a certain absence.
fn pid_is_live(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return true;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// Reap `tma_test`-prefixed scratch servers orphaned by an earlier run that died before its
/// [`Scratch`] drops could (SIGKILL, a double panic, an editor stopping the runner). Each leaked
/// server keeps a tmux process and a client-noise source alive, which is load the next run's timing
/// windows pay for.
///
/// Safe under the parallel test binaries that share the prefix: the socket name carries the pid of
/// the binary that created it, so a socket is only reaped once that pid is provably gone. A live
/// owner's server is never touched, and a recycled pid only costs a skipped reap.
///
/// The sweep runs once per test binary; later calls are no-ops. [`Scratch`] calls it for its own
/// suites, so only a harness with its own socket type ([`unique_id`] by hand) needs to call it.
pub fn reap_orphan_scratch_servers() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(reap_orphans);
}

fn reap_orphans() {
    if let Ok(entries) = std::fs::read_dir(scratch_socket_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(owner) = socket_owner_pid(&name) else {
                continue;
            };
            if pid_is_live(owner) {
                continue;
            }
            kill_scratch_server(&name);
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // A second pass over processes, not files: an orphan whose socket was already unlinked (an
    // older harness did that unconditionally) is unreachable by name, so the sweep above cannot see
    // it and it would otherwise live until reboot. Same ownership rule — a socket is only ever
    // reaped once the pid that created it is provably gone.
    for (pid, socket) in scratch_processes() {
        let Some(owner) = socket_owner_pid(&socket) else {
            continue;
        };
        if pid_is_live(owner) {
            continue;
        }
        kill_pid(pid);
    }
}

/// A process-shared empty `config.toml` the harness pins `TMA_CONFIG` to, so a harness-spawned `tma`
/// ignores the developer's real config. FIXED name (one file, not per-pid); racing writes are identical.
pub fn empty_config_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let p = std::env::temp_dir().join("tma-test-empty-config.toml");
        // An empty (or re-truncated) file; ignore a benign race with a sibling thread's write.
        let _ = std::fs::write(&p, b"");
        p
    })
    .as_path()
}

/// How long the PTY helper client stays attached. A backstop against a leaked helper, not a test
/// budget: [`Scratch`] reaps the client on drop, and it must outlive the slowest test that attaches
/// one, so that no assertion can lose a race against its own client expiring.
const ATTACH_LIFETIME: Duration = Duration::from_secs(120);

/// The ceiling every readiness poll in the suites waits out. Deliberately far past any healthy
/// timing (a loaded box running `cargo test --workspace` next to a build is still an order of
/// magnitude inside it), so a lapse means the condition never happened, not that the box was busy.
pub const POLL_CEILING: Duration = Duration::from_secs(45);

/// The prompt [`Scratch::new_shell_pane`] pins on its isolated shell. Exported so a test can anchor
/// a capture needle on it: `SHELL_PROMPT` + the keystroke is text only a delivered key can produce,
/// where the bare keystroke could be anything the developer's own rc had already painted.
pub const SHELL_PROMPT: &str = "tma> ";

/// Poll `cond` every 100 ms until it holds, panicking with `what` once [`POLL_CEILING`] lapses.
/// The suites' replacement for a blind sleep in front of an assertion: it returns the moment the
/// state is observable, and names the condition it was waiting on when it never arrives.
pub fn poll_until(what: &str, cond: impl FnMut() -> bool) {
    poll_until_within(POLL_CEILING, what, cond);
}

/// [`poll_until`] with an explicit ceiling, for the few polls whose bound is part of the assertion.
fn poll_until_within(ceiling: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + ceiling;
    loop {
        if cond() {
            return;
        }
        if Instant::now() >= end {
            panic!("timed out after {ceiling:?} waiting for: {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Poll `cond` every 20 ms until it returns `true` or `deadline` elapses; returns the last value.
/// The suites' condition-poll primitive replacing a blind `sleep` (which over- or under-waits: flake).
pub fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `capture-pane -p` on `target` (empty = active pane) until its text contains `marker`, or
/// `deadline` elapses. Host-agnostic readiness gate for a `printf '<marker>'; exec <proc>` pane: the
/// marker proves the shell reached its `exec`, unlike `#{pane_current_command}` (breaks under uutils).
pub fn wait_capture_contains(socket: &str, target: &str, marker: &str, deadline: Duration) -> bool {
    wait_until(deadline, || {
        let out = if target.is_empty() {
            scratch_tmux(socket, &["capture-pane", "-p"])
        } else {
            scratch_tmux(socket, &["capture-pane", "-p", "-t", target])
        };
        String::from_utf8_lossy(&out.stdout).contains(marker)
    })
}

/// Poll a daemon `--status-file` until its control pool is quiescent (`active=0`) continuously for
/// `settle`, or `overall` elapses. The daemon captures a just-quiet pane's edge BEFORE writing status,
/// so `active==0` held past `settle` means every `%output` ran its capture: an empirical margin
/// (`settle` must exceed the daemon's 1 s quiet threshold), not a proof. A missing `active` resets the clock.
pub fn wait_daemon_quiescent(status_file: &Path, settle: Duration, overall: Duration) -> bool {
    let end = Instant::now() + overall;
    let mut quiet_since: Option<Instant> = None;
    loop {
        let active = status_field_u64(status_file, "active");
        let now = Instant::now();
        quiet_since = match (active, quiet_since) {
            // Only a readable, parsed `active == 0` counts as quiet; None (unreadable file /
            // missing / unparsable key) resets the clock rather than passing vacuously.
            (Some(0), Some(t)) => Some(t),
            (Some(0), None) => Some(now),
            _ => None,
        };
        if let Some(t) = quiet_since {
            if now.duration_since(t) >= settle {
                return true;
            }
        }
        if now >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read one trimmed `key=value` field from a daemon status file, [`None`] when the file is
/// unreadable or the key absent. For the suites that hold a `--status-file` path rather than a
/// [`Scratch`]; [`Scratch::status`] is the harness-owned equivalent.
pub fn status_field(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// [`status_field`] parsed as u64. [`None`] for every failure (file absent, key missing,
/// unparsable), so a caller distinguishes "reported 0" from "nothing yet".
fn status_field_u64(path: &Path, key: &str) -> Option<u64> {
    status_field(path, key)?.parse().ok()
}

/// Poll a daemon status file until `key` equals `want`, `false` once `timeout` elapses.
pub fn wait_status_eq(path: &Path, key: &str, want: &str, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if status_field(path, key).as_deref() == Some(want) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A scratch tmux server + private workdir that tear down on drop, plus an optional attached PTY
/// client. The shared harness for every tmux-touching suite (interactive UI, `config`/`custom_agent`,
/// and daemon capture/control/notify). Public `socket`/`workdir` pin paths; the attached client is
/// reaped first.
pub struct Scratch {
    pub socket: String,
    pub workdir: PathBuf,
    /// A PTY client attached via [`Scratch::attach_client`], SIGKILL-reaped before the server.
    attach: Option<Child>,
    /// The unique suffix both this socket and any [`Scratch::nested_socket`] are keyed on.
    unique: String,
    /// Extra servers this test started ([`Scratch::nested_socket`]), killed with the main one.
    nested: Vec<String>,
}

impl Scratch {
    /// A fresh scratch server keyed by `tag` (socket `tma_test_<tag>_<unique>`, workdir under the
    /// temp dir). The server spawns lazily on the first `tmux` call; the workdir is created now.
    pub fn new(tag: &str) -> Scratch {
        reap_orphan_scratch_servers();
        let unique = unique_id();
        let workdir = std::env::temp_dir().join(format!("tma_{tag}_{unique}"));
        std::fs::create_dir_all(&workdir).unwrap();
        Scratch {
            socket: format!("tma_test_{tag}_{unique}"),
            workdir,
            attach: None,
            unique,
            nested: Vec::new(),
        }
    }

    /// A second socket name owned by this scratch: the inner server the nested-multiplexer suites
    /// run inside a pane. Killed on drop with the outer one (so a panicking test leaks neither), and
    /// keyed on the same `<pid>_<nanos>_<counter>` suffix, which is what the orphan reap reads.
    pub fn nested_socket(&mut self, label: &str) -> String {
        let socket = format!("tma_test_{label}_{}", self.unique);
        self.nested.push(socket.clone());
        socket
    }

    /// Like [`Scratch::new`] but roots the workdir at `/tmp/tma_<tag>_<8-hex>`, so it doubles as the
    /// daemon's `XDG_RUNTIME_DIR` (see [`Scratch::command`]) without blowing the ~104-byte unix-socket
    /// path cap (`SUN_LEN`) the default deeply-nested macOS temp dir overruns. The socket NAME is unchanged.
    pub fn new_daemon(tag: &str) -> Scratch {
        reap_orphan_scratch_servers();
        let unique = unique_id();
        let workdir = PathBuf::from(format!("/tmp/tma_{tag}_{}", short_hash(&unique)));
        std::fs::create_dir_all(&workdir).unwrap();
        Scratch {
            socket: format!("tma_test_{tag}_{unique}"),
            workdir,
            attach: None,
            unique,
            nested: Vec::new(),
        }
    }

    /// Run a tmux command against the scratch `-L` socket (`-f /dev/null`, [`scratch_tmux`]).
    pub fn tmux(&self, args: &[&str]) -> Output {
        scratch_tmux(&self.socket, args)
    }

    /// A `display-message -p` read of `fmt` on `target` (empty omits `-t`), `trim_end` only. Companion
    /// to [`Scratch::display`] (trims both ends) for daemon/event reads pinned to `trim_end`.
    pub fn get(&self, target: &str, fmt: &str) -> String {
        let out = if target.is_empty() {
            self.tmux(&["display-message", "-p", fmt])
        } else {
            self.tmux(&["display-message", "-p", "-t", target, fmt])
        };
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// Read a pane option (`show-options -pqv`), trimmed; empty when unset.
    pub fn pane_option(&self, pane: &str, key: &str) -> String {
        let out = self.tmux(&["show-options", "-pqv", "-t", pane, key]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Set a pane option (`set-option -p`), asserting the command succeeded.
    pub fn set_opt(&self, pane: &str, key: &str, val: &str) {
        let out = self.tmux(&["set-option", "-p", "-t", pane, key, val]);
        assert!(
            out.status.success(),
            "set {key} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A `tma` [`Command`] pre-wired for the daemon suites: the on-demand binary, the workdir as private
    /// `XDG_RUNTIME_DIR` (daemon socket/lock never touch a real `~/tma`), `TMA_CONFIG` pinned empty.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(self.bin());
        without_ambient_tmux(&mut cmd);
        cmd.env("XDG_RUNTIME_DIR", &self.workdir);
        cmd.env("TMA_CONFIG", empty_config_path());
        cmd
    }

    /// The conventional daemon `--status-file` path (`<workdir>/status`).
    pub fn status_path(&self) -> PathBuf {
        self.workdir.join("status")
    }

    /// Parse the daemon status file into a `key=value` map; empty when it does not exist yet.
    pub fn status(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(self.status_path()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
        map
    }

    /// Read a single status field as u64, `0` when absent/unparsable (the daemon suites' gauge
    /// reads, where a not-yet-written field reads as the pre-increment 0).
    pub fn status_u64(&self, key: &str) -> u64 {
        self.status()
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Poll the status file until `key == want`, panicking with the last value seen once
    /// [`POLL_CEILING`] lapses. The daemon suites' readiness gate (client attach, sweep counters):
    /// no fixed ceiling to outrun under load, and a failure names the field it never saw.
    pub fn expect_status(&self, key: &str, want: &str) {
        let end = Instant::now() + POLL_CEILING;
        loop {
            let got = self.status().get(key).cloned().unwrap_or_default();
            if got == want {
                return;
            }
            if Instant::now() >= end {
                panic!(
                    "daemon status `{key}` never reached {want:?} within {POLL_CEILING:?} \
                     (last seen: {got:?})"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The `tma` binary path (built on demand, [`tma_bin`]).
    pub fn bin(&self) -> String {
        tma_bin()
    }

    /// Run `tma <args>` against the scratch server + this suite's manifest dir (the workdir),
    /// with the real user config pinned out via `TMA_CONFIG`.
    pub fn tma(&self, args: &[&str]) -> Output {
        without_ambient_tmux(&mut Command::new(self.bin()))
            .args(args)
            .arg("--socket-name")
            .arg(&self.socket)
            .arg("--manifest-dir")
            .arg(&self.workdir)
            .env("TMA_CONFIG", empty_config_path())
            .output()
            .expect("spawn tma")
    }

    /// Create the context suites' standard pane: a detached 80x24 `s1` session running `exec sleep
    /// 100000`, returning its `%`-prefixed pane id. Shared by the per-agent context intake suites.
    pub fn new_pane(&self) -> String {
        assert!(self
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "s1",
                "-x",
                "80",
                "-y",
                "24",
                "exec sleep 100000",
            ])
            .status
            .success());
        let pane = self.get("s1", "#{pane_id}");
        assert!(pane.starts_with('%'), "got pane {pane:?}");
        pane
    }

    /// A fresh detached 80x24 session running a MINIMAL interactive shell (so `send-keys` produces
    /// visible output), returning its active pane's `%`-prefixed id. Distinct from
    /// [`Scratch::new_pane`], which execs `sleep` in a named `s1`: the broker/act/detach suites need
    /// a live shell.
    ///
    /// `env -i` isolates the shell the way the `-L` socket isolates the server. Without it the
    /// developer's own rc files load into the pane, and a test that looks for a typed keystroke in
    /// the capture can find it in a themed prompt instead — which is how the broker's only
    /// "the keys were delivered" assertion came to pass with `send-keys` stubbed out entirely. The
    /// prompt is the fixed [`SHELL_PROMPT`], so a needle anchored on it cannot be ambient noise.
    /// The pane is returned only once that prompt is on screen: the shell is then reading input, so
    /// a caller's keys land in it rather than in the pty buffer of a process still starting up.
    pub fn new_shell_pane(&self) -> String {
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        let shell = format!(
            "exec env -i PATH={path} HOME={home} TERM=xterm-256color PS1='{SHELL_PROMPT}' sh -i",
            home = self.workdir.display()
        );
        assert!(self
            .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &shell])
            .status
            .success());
        let pane = self.get("", "#{pane_id}");
        assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
        // `capture-pane` right-trims every line, so an empty prompt reads without its trailing
        // space; match the trimmed form. (With a keystroke echoed after it the space is preserved,
        // which is what lets a caller anchor on the full `SHELL_PROMPT`.)
        assert!(
            wait_capture_contains(&self.socket, &pane, SHELL_PROMPT.trim_end(), POLL_CEILING),
            "the scratch shell must reach its prompt before the pane is handed out"
        );
        pane
    }

    /// Drive `tma event --agent <agent> --kind <kind> --pane <pane>` against this scratch server and
    /// its `agents/` manifest dir, payload written to a `--payload` file. `TMUX_PANE` is set too so the
    /// same helper drives the hook path (pane from the env) and the context intake (explicit `--pane`).
    pub fn event(&self, agent: &str, kind: &str, pane: &str, payload: &str) -> Output {
        let path = self.workdir.join(format!("payload_{kind}_{pane}"));
        std::fs::write(&path, payload).unwrap();
        without_ambient_tmux(&mut Command::new(self.bin()))
            .args(["event", "--agent", agent, "--kind", kind])
            .args(["--socket-name", &self.socket])
            .arg("--manifest-dir")
            .arg(self.manifest_dir())
            .args(["--pane", pane])
            .arg("--payload")
            .arg(&path)
            .env("TMUX_PANE", pane)
            .env("TMA_CONFIG", empty_config_path())
            .output()
            .expect("spawn tma event")
    }

    /// Run `tma ls --json` against this scratch server + `agents/` manifest dir, returning stdout.
    pub fn ls_json(&self) -> String {
        let out = without_ambient_tmux(&mut Command::new(self.bin()))
            .args(["ls", "--json"])
            .args(["--socket-name", &self.socket])
            .arg("--manifest-dir")
            .arg(self.manifest_dir())
            .env("TMA_CONFIG", empty_config_path())
            .output()
            .expect("spawn tma ls");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// The `agents/` manifest subdir under the workdir, created on demand. A SUBDIR (not the workdir
    /// root) so a sibling `config.toml` is never scanned as a broken manifest by the dir-reading loader.
    pub fn manifest_dir(&self) -> PathBuf {
        let dir = self.workdir.join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write manifest `body` to `<manifest_dir>/<name>` (creating the `agents/` subdir).
    pub fn write_manifest(&self, name: &str, body: &str) {
        std::fs::write(self.manifest_dir().join(name), body).unwrap();
    }

    /// The workdir `config.toml` path (may or may not exist yet — an absent one is the
    /// zero-config floor).
    pub fn config_path(&self) -> PathBuf {
        self.workdir.join("config.toml")
    }

    /// Write a `config.toml` `body` into the workdir (at [`Scratch::config_path`]).
    pub fn write_config(&self, body: &str) {
        std::fs::write(self.config_path(), body).unwrap();
    }

    /// A `display-message -p` read of `fmt`, trimmed. An empty `target` omits `-t` (the
    /// active pane / no specific target).
    pub fn display(&self, target: &str, fmt: &str) -> String {
        let out = if target.is_empty() {
            self.tmux(&["display-message", "-p", fmt])
        } else {
            self.tmux(&["display-message", "-p", "-t", target, fmt])
        };
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The drop-box the attached PTY helper drains keystrokes from.
    fn client_keys_path(&self) -> PathBuf {
        self.workdir.join("client-keys")
    }

    /// Type `keys` at the attached client's real terminal, as a person at the keyboard would: the
    /// bytes go to whatever that client is showing, including a `display-popup` overlay and the
    /// prefix-key table, neither of which `send-keys` can reach (it addresses panes, and a popup's
    /// pane is not one). Raw bytes: `"\r"` is Enter, `"\x02"` the default `C-b` prefix.
    ///
    /// Renamed into place so the helper can never read a half-written batch, and the previous batch
    /// must be drained first so two sends cannot clobber each other.
    pub fn send_client_keys(&self, keys: &str) {
        let path = self.client_keys_path();
        assert!(
            wait_until(POLL_CEILING, || !path.exists()),
            "the PTY client never drained the previous keys"
        );
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, keys).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
    }

    /// Attach a real PTY client to `session` via a small Python helper (`pty.fork` + `tmux attach`);
    /// the child is stored in `attach`, reaped on drop. Returns a three-state [`AttachOutcome`] so a
    /// caller never folds a genuine environment gap (python3 absent) with a real attach regression.
    pub fn attach_client(&mut self, session: &str) -> AttachOutcome {
        self.attach_with(session, None)
    }

    /// Like [`Scratch::attach_client`], but the PTY client also presses `q` a few times a second, so a
    /// tmux menu overlay (`display-menu`) is dismissed almost as soon as it opens. The `tma act --menu`
    /// execution test needs this: a one-shot `tmux display-menu` blocks until its menu is closed, and no
    /// `send-keys` can close it (the overlay captures the client's real terminal input, not the pane's).
    /// `q` reaching an idle `sleep` pane whenever no menu is open is a harmless no-op.
    pub fn attach_menu_client(&mut self, session: &str) -> AttachOutcome {
        self.attach_with(session, Some("q"))
    }

    /// Shared attach path for [`Scratch::attach_client`] and [`Scratch::attach_menu_client`]. When
    /// `dismiss` is `Some(key)` the PTY helper types that key ~4×/s so any open menu overlay is closed;
    /// `None` is the plain read-only client the jump/picker/watch suites use. Either way the helper
    /// drains [`Scratch::client_keys_path`] every tick, which is how [`Scratch::send_client_keys`]
    /// types at the client's real terminal.
    fn attach_with(&mut self, session: &str, dismiss: Option<&str>) -> AttachOutcome {
        let script = self.workdir.join("attach.py");
        std::fs::write(
            &script,
            "import os, pty, sys, time, select\n\
             sock, session, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])\n\
             keyfile = sys.argv[4]\n\
             dismiss = sys.argv[5].encode() if len(sys.argv) > 5 else None\n\
             pid, master = pty.fork()\n\
             if pid == 0:\n    os.execvp('tmux', ['tmux','-L',sock,'attach','-t',session])\n\
             end = time.time() + secs\n\
             last = 0.0\n\
             while time.time() < end:\n\
             \x20   r,_,_ = select.select([master],[],[],0.2)\n\
             \x20   if r:\n\
             \x20       try: os.read(master, 65536)\n\
             \x20       except OSError: break\n\
             \x20   try:\n\
             \x20       with open(keyfile,'rb') as f: keys = f.read()\n\
             \x20       os.unlink(keyfile)\n\
             \x20   except OSError: keys = b''\n\
             \x20   if keys:\n\
             \x20       try: os.write(master, keys)\n\
             \x20       except OSError: break\n\
             \x20   if dismiss is not None and time.time() - last > 0.25:\n\
             \x20       last = time.time()\n\
             \x20       try: os.write(master, dismiss)\n\
             \x20       except OSError: break\n\
             try: os.kill(pid, 15)\n\
             except OSError: pass\n",
        )
        .unwrap();
        let mut cmd = Command::new("python3");
        without_ambient_tmux(&mut cmd);
        // The tmux client refuses to attach without a usable TERM, and CI step environments
        // carry none; the PTY is synthetic, so pin a terminfo every platform ships.
        cmd.env("TERM", "xterm-256color");
        cmd.arg(&script)
            .arg(&self.socket)
            .arg(session)
            .arg(ATTACH_LIFETIME.as_secs_f64().to_string())
            .arg(self.client_keys_path());
        if let Some(key) = dismiss {
            cmd.arg(key);
        }
        match cmd.spawn() {
            Ok(child) => {
                self.attach = Some(child);
                let end = Instant::now() + POLL_CEILING;
                while Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(100));
                    let clients = self.tmux(&["list-clients", "-F", "#{client_name}"]);
                    if !String::from_utf8_lossy(&clients.stdout).trim().is_empty() {
                        return AttachOutcome::Attached;
                    }
                }
                // python3 spawned but no client ever appeared: a real attach/startup regression,
                // not an environment gap.
                AttachOutcome::Failed
            }
            // Could not even spawn python3 — a genuine environment gap (a legitimate skip), unless
            // TMA_REQUIRE_TMUX=1 demands the PTY suites actually run (CI), where it must hard-fail.
            Err(_) => {
                if require_tmux() {
                    panic!(
                        "python3 required by TMA_REQUIRE_TMUX=1 but not found for the PTY attach"
                    );
                }
                AttachOutcome::NoPython
            }
        }
    }
}

/// The result of [`Scratch::attach_client`], kept three-state so callers skip only on a real
/// environment gap and hard-fail on an actual attach regression (rather than masking it as a skip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "an attach failure must be distinguished from an environment skip"]
pub enum AttachOutcome {
    /// A PTY client attached and is visible to `list-clients`.
    Attached,
    /// `python3` could not be spawned at all — a legitimate environment skip.
    NoPython,
    /// `python3` ran but no client ever attached within the window — a regression, not a skip.
    Failed,
}

/// Owns a spawned `tma daemon` [`Child`], SIGKILL-reaped on drop so none leaks; a self-exited daemon
/// makes `kill` a no-op and `wait` still reaps it (no zombie). `pid`/`wait_exit` serve exit assertions.
pub struct DaemonGuard {
    child: Child,
}

impl DaemonGuard {
    /// Wrap an already-spawned daemon child so it is reaped on drop.
    pub fn new(child: Child) -> DaemonGuard {
        DaemonGuard { child }
    }

    /// The daemon process id (for tests that inspect or compare the running instance).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait up to `timeout` for the daemon to exit on its own, reaping it. `true` if it exited.
    pub fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(mut child) = self.attach.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for socket in &self.nested {
            kill_scratch_server(socket);
            cleanup_scratch_socket(socket);
        }
        kill_scratch_server(&self.socket);
        cleanup_scratch_socket(&self.socket);
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process sweep reads `-L <socket>`, which is what both a scratch server (macOS keeps the
    /// creating argv) and a leaked control client carry. Everything else on the machine is ignored:
    /// another user's tmux, a `-L` socket this harness never made, a header row.
    #[test]
    fn ps_scan_finds_scratch_sockets_and_nothing_else() {
        let text = "\
  350 tmux -L tma_test_sub-degrade_98112_1786627368312963000_5 -f /dev/null new-session -d
  443 /nix/store/x/bin/tmux -u -L tma_test_sub-degrade_98112_1786627368312963000_5 -C attach-session
  512 tmux -L work new-session
  777 tmux new-session -d
  PID COMMAND";
        assert_eq!(
            parse_ps_sockets(text),
            vec![
                (
                    350,
                    "tma_test_sub-degrade_98112_1786627368312963000_5".into()
                ),
                (
                    443,
                    "tma_test_sub-degrade_98112_1786627368312963000_5".into()
                ),
            ],
            "only tma_test sockets, server and client alike"
        );
    }

    /// A socket file is never unlinked while a process still holds the name: that is what turns a
    /// reapable orphan into one no later run can find.
    #[test]
    fn cleanup_spares_the_socket_of_a_live_holder() {
        let s = Scratch::new("cleanup_live");
        assert!(s
            .tmux(&["new-session", "-d", "-x", "80", "-y", "24"])
            .status
            .success());
        let path = scratch_socket_path(&s.socket);
        assert!(path.exists(), "the scratch server bound its socket");
        cleanup_scratch_socket(&s.socket);
        assert!(
            path.exists(),
            "a running server keeps its socket file: {}",
            path.display()
        );
    }

    #[test]
    fn owner_pid_is_parsed_from_the_socket_name() {
        let s = Scratch::new("owner_pid_tag");
        assert_eq!(
            socket_owner_pid(&s.socket),
            Some(std::process::id()),
            "a live Scratch's socket names this process, so the reap skips it: {}",
            s.socket
        );
    }

    #[test]
    fn owner_pid_survives_underscores_in_the_tag() {
        assert_eq!(
            socket_owner_pid("tma_test_act_menu_noclient_42_1786622442612306000_0"),
            Some(42)
        );
        // A name extended on the right (the nested inner server) still reads its owner.
        assert_eq!(
            socket_owner_pid("tma_test_nested_44962_1786622442612306000_1_inner"),
            Some(44962)
        );
        // Names the harness did not mint carry no owner and are left alone.
        assert_eq!(socket_owner_pid("default"), None);
        assert_eq!(socket_owner_pid("tma_test_short"), None);
        assert_eq!(
            socket_owner_pid("tma_test_notapid_1786622442612306000_0"),
            None
        );
    }

    /// Every scratch child is spawned with the ambient tmux session removed: an inherited
    /// `TMUX_PANE` is a pane id on the developer's own server that tmux would happily match against
    /// a same-numbered pane on the scratch server.
    #[test]
    fn scratch_children_do_not_inherit_the_ambient_tmux_pane() {
        let mut cmd = Command::new("tmux");
        without_ambient_tmux(&mut cmd);
        let removed: Vec<&str> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .filter_map(|(k, _)| k.to_str())
            .collect();
        assert_eq!(removed, ["TMUX", "TMUX_PANE"]);
    }

    #[test]
    fn pid_liveness_distinguishes_self_from_a_reaped_pid() {
        assert!(pid_is_live(std::process::id()));
        // pid 1 always exists and is not ours: the EPERM leg must still read as live.
        assert!(pid_is_live(1));
    }
}
