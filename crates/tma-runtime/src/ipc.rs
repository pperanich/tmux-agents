//! The daemon wire protocol and per-server socket keying.
//!
//! One module owns the TMA1 frame format, the ACK/NAK bytes, and the
//! `$XDG_RUNTIME_DIR/tma/<key>.{sock,lock}` path derivation, so client ([`DaemonSink`]) and server
//! (`tma_daemon`) can never frame or target divergently. The daemon is never required: `tma event`
//! direct-stamps when none answers. A second speaker, [`WaitSubscription`] (`tma wait`), rides the
//! same socket under a distinct `SUBSCRIBE_MAGIC`. Compat rule: a new capability is a new discriminant
//! an old peer rejects cleanly, so version skew silently degrades to polling rather than erroring.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::{fcntl_setfd, Errno, FdFlags};
use rustix::net::{connect, socket, AddressFamily, SocketAddrUnix, SocketType};
use rustix::process::{kill_process, Pid, Signal};
use tma_tmux::tmux::Tmux;

use crate::debug::fnv1a64;

/// A `poll(2)` timeout as rustix 1.x wants it: `Some(&Timespec)` (was a `c_int` ms in 0.38).
fn poll_timeout(d: Duration) -> Timespec {
    Timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
        tv_nsec: d.subsec_nanos() as _,
    }
}

/// Wire-frame magic (protocol v1). A frame that does not start with these four bytes is
/// rejected before any allocation, so random garbage on the socket costs nothing.
const MAGIC: &[u8; 4] = b"TMA1";

/// Delivery-ack bytes on the same unchanged TMA1 frames. The daemon writes one byte after handling
/// a connection: [`ACK`] when it RESOLVED the event (frame parsed, the agent's manifest is present,
/// and its manifests reached a verdict — a write plan, or a deliberate no-write such as the subagent
/// ownership guard), else [`NAK`]. An event the daemon's manifests map to nothing NAKs, so a
/// resident daemon carrying older compiled-in manifests cannot ack away a transition it never wrote.
/// The client treats only an ACK as delivered; a NAK/timeout/EOF (including
/// an old daemon that writes nothing) falls through to a direct stamp, so a droppable frame is never
/// silently lost and the duplicate is idempotent under guarded writes.
pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;

/// Subscribe-request magic: a `tma wait` client riding the daemon's edge pushes. Distinct from
/// [`MAGIC`] so one read classifies the kinds and an old daemon rejects it cleanly (NAK+close ⇒ poll).
const SUBSCRIBE_MAGIC: &[u8; 4] = b"TMAS";

/// History-request magic: `tma debug transitions` reading the daemon's transition ring. A daemon
/// predating it classifies the magic as garbage and NAKs, which the client reports as an unsupported
/// daemon rather than an error — the compat rule this module states.
const HISTORY_MAGIC: &[u8; 4] = b"TMAH";

/// Every magic a daemon may be handed. `magic_kind` classifies against this list, so a new
/// discriminant is one entry rather than a new byte comparison to keep in sync.
const MAGICS: &[&[u8; 4]] = &[MAGIC, SUBSCRIBE_MAGIC, HISTORY_MAGIC];

/// History-accepted byte, written before the length-prefixed document body. Distinct from every
/// other ack so a client tells a history-capable daemon from one that NAKs the unknown magic.
pub const HIST_ACK: u8 = 0x13;

/// Subscription-accepted byte: written once, before any [`PUSH`], so the client tells a live
/// push-capable daemon from an old one (NAK/EOF) or a dead socket. Distinct from [`ACK`] for clarity.
pub const SUB_ACK: u8 = 0x11;

/// Edge-push wake byte, one per state-affecting serve-loop iteration. A WAKE HINT only, never the
/// state, so `tma wait` stays cycle-authoritative: a spurious or coalesced push costs one extra cycle.
pub const PUSH: u8 = 0x12;

/// Field-size caps for [`read_frame`] (tiny identifiers; a JSON-blob payload). Bounding both stops a
/// hostile length prefix from forcing a huge allocation.
const MAX_SMALL: usize = 4096;
const MAX_PAYLOAD: usize = 1 << 20; // 1 MiB

/// How long the client waits for the daemon's delivery ack before direct-stamping. Short: the
/// daemon writes the byte immediately. Bounds the read half symmetrically with the 2 s write timeout.
const ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a `tma wait` client waits for [`SUB_ACK`] before polling. Mirrors [`ACK_TIMEOUT`]; also
/// bounds an old daemon that neither ACKs nor closes.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The parsed inbound frame: the raw hook data the direct path also holds (`$TMUX_PANE`, agent,
/// kind, payload). The daemon derives state/session from `(kind, payload)` via the same pure
/// [`crate::event::apply_event`] mapping, never trusting a pre-computed state off the wire, so the
/// two paths cannot diverge.
pub struct Frame {
    pub pane: String,
    pub agent: String,
    pub kind: String,
    pub payload: String,
}

/// The per-server runtime paths.
pub struct Paths {
    /// The `0700` parent directory holding the files below.
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    /// When an automatic upgrade restart last fired for this server ([`note_restart`]). Beside the
    /// lock rather than inside it: the lock belongs to whichever daemon holds the flock, and a
    /// restart has to be remembered ACROSS the daemon it replaced.
    pub restart_stamp: PathBuf,
}

/// The stable filename stem for a server: a hex FNV-1a of its `#{socket_path}`. Pure, so client and
/// daemon feeding the identical `#{socket_path}` land on the identical stem.
pub fn socket_key(socket_path: &str) -> String {
    format!("{:016x}", fnv1a64(socket_path.as_bytes()))
}

/// The runtime base dir: `$XDG_RUNTIME_DIR`, else `$TMPDIR`, else `/tmp`. An empty value falls
/// through as unset: `XDG_RUNTIME_DIR=` would otherwise put the daemon socket at a cwd-relative
/// path, so a client and a daemon started from different directories would never find each other.
fn runtime_base() -> PathBuf {
    runtime_base_from(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("TMPDIR"),
    )
}

/// The base-dir choice, pure over the two variables so the empty-is-unset rule is testable without
/// touching process env.
fn runtime_base_from(xdg: Option<OsString>, tmpdir: Option<OsString>) -> PathBuf {
    xdg.filter(|v| !v.is_empty())
        .or_else(|| tmpdir.filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The per-user runtime directory holding the keyed sockets and locks (and the notify failure
/// marker). Public so the notify path lands its marker beside them rather than inventing a location.
pub fn runtime_dir() -> PathBuf {
    runtime_base().join("tma")
}

/// Socket + lock paths for the server whose `#{socket_path}` is `socket_path`. Both `tma
/// event` and `tma daemon` call this, so a daemon binds exactly the path its clients probe.
pub fn paths_for(socket_path: &str) -> Paths {
    let dir = runtime_dir();
    let key = socket_key(socket_path);
    Paths {
        socket: dir.join(format!("{key}.sock")),
        lock: dir.join(format!("{key}.lock")),
        restart_stamp: dir.join(format!("{key}.restart")),
        dir,
    }
}

/// Resolve the target server's `#{socket_path}`; `None` when the server is gone or empty. The
/// `Tmux` handle carries any `--socket-name`, so this keys on the intended server.
pub fn resolve_socket_path(tmux: &Tmux) -> Option<String> {
    tmux.socket_path().ok().filter(|s| !s.is_empty())
}

/// The version this build stamps into the lock file and compares against. Every workspace crate
/// inherits the same version, so the daemon writer and the `tma doctor` reader agree by construction.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The lock-file line prefix carrying the daemon's version. Line-based rather than a second field on
/// the pid line, so a lock file written by an older daemon (bare pid) still parses.
const LOCK_VERSION_PREFIX: &str = "version=";

/// What a daemon recorded in its flock-held lock file.
pub struct LockInfo {
    pub pid: i32,
    /// The daemon's build version, `None` for a lock file written before it was recorded.
    pub version: Option<String>,
}

/// Render the lock-file body a running daemon writes: the pid on the first line, then its version.
/// Pure, so the writer and [`parse_lock`] are tested against each other rather than a literal.
pub fn render_lock(pid: u32, version: &str) -> String {
    format!("{pid}\n{LOCK_VERSION_PREFIX}{version}\n")
}

/// Parse a lock-file body. The first line is the pid; the rest is optional, so the bare-pid format an
/// older daemon wrote still yields a usable [`LockInfo`] (that compatibility is the whole reason the
/// version is a separate line). `None` when there is no positive pid to read.
pub fn parse_lock(body: &str) -> Option<LockInfo> {
    let mut lines = body.lines();
    let pid = lines
        .next()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|p| *p > 0)?;
    let version = lines.find_map(|l| {
        l.trim()
            .strip_prefix(LOCK_VERSION_PREFIX)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    });
    Some(LockInfo { pid, version })
}

/// Read the lock file at `path` without creating it. `None` when it is absent or holds no pid.
fn read_lock(path: &Path) -> Option<LockInfo> {
    parse_lock(&std::fs::read_to_string(path).ok()?)
}

/// A read-only snapshot of the per-server daemon's presence, for `tma doctor`. `alive` is a connect
/// probe (the same signal `DaemonSink::deliver` uses), so doctor reports exactly the reachability
/// the hook path sees. It lives here (tier 2) so `tma doctor` avoids importing tier 3, and never
/// mutates: connect fails fast on a missing/stale socket and creates no file.
pub struct DaemonStatus {
    /// The per-server socket path probed ([`paths_for`]).
    pub socket: PathBuf,
    /// The per-server lock path, reported for diagnosis. Read, never opened for write (doctor must
    /// not create it, which `--ensure`'s `create` would).
    pub lock: PathBuf,
    /// Whether a daemon is currently accepting connections on the socket.
    pub alive: bool,
    /// The running daemon's build version from the lock file. `None` when nothing is running, the
    /// lock file is unreadable, or it predates version recording.
    pub version: Option<String>,
}

/// Probe the per-server daemon read-only (`tma doctor`): connect to the keyed socket, report whether
/// a daemon answers, and read its recorded version. `None` when the tmux server is gone. See
/// [`DaemonStatus`].
pub fn daemon_status(tmux: &Tmux) -> Option<DaemonStatus> {
    let socket_path = resolve_socket_path(tmux)?;
    let paths = paths_for(&socket_path);
    // A successful connect is the liveness signal; the connection is dropped immediately without
    // sending a frame, so the daemon reads EOF and discards it — no stamp, no side effect.
    let alive = daemon_answers(&paths);
    // Only a live daemon's version means anything: a leftover lock file describes a dead one.
    let version = alive
        .then(|| read_lock(&paths.lock).and_then(|l| l.version))
        .flatten();
    Some(DaemonStatus {
        socket: paths.socket,
        lock: paths.lock,
        alive,
        version,
    })
}

/// Outcome of a `tma reload` signal. `tma reload` is a thin convenience over `kill -HUP`: users
/// rarely know the daemon's pid, and `--socket-name X` may target one of several servers.
pub enum ReloadOutcome {
    /// No tmux server for this handle ⇒ no daemon to signal.
    NoServer,
    /// A live daemon was found and sent SIGHUP (it reloads config + manifests in place).
    Signaled,
    /// No daemon is currently running for this server — a no-op, not an error (one-shots and the
    /// picker reload on their own; the daemon is strictly additive).
    NotRunning,
    /// A daemon is running but its pid could not be read or signaled (stale/old lock file).
    Failed(String),
}

/// Signal the per-server daemon to hot-reload config + manifests (`tma reload`). Lives in tier-2 ipc
/// so the bin reaches the daemon without importing tier 3, like [`daemon_status`]. It gates on a
/// live socket connect, reads the pid from the daemon's lock file, and re-probes before SIGHUP (see
/// the inline note on the residual pid-recycle window).
pub fn reload_daemon(tmux: &Tmux) -> ReloadOutcome {
    let Some(socket_path) = resolve_socket_path(tmux) else {
        return ReloadOutcome::NoServer;
    };
    let paths = paths_for(&socket_path);
    // Liveness gate: a successful connect proves a daemon is accepting on the socket right now.
    // The connection is dropped without a frame, so the daemon reads EOF and discards it.
    if !daemon_answers(&paths) {
        return ReloadOutcome::NotRunning;
    }
    let Some(pid) = read_lock(&paths.lock).map(|l| l.pid) else {
        return ReloadOutcome::Failed(
            "daemon is running but its pid is unavailable (lock file empty or stale)".to_string(),
        );
    };
    // Re-probe immediately before the kill: the pid read is a filesystem round-trip during which the
    // daemon could exit and its pid be recycled (SIGHUP default-terminates). A second connect
    // narrows the recycle window to the gap between this probe and the `kill` below; it cannot fully
    // close it (the daemon may still exit in that gap). Residual is local-user-only, low-stakes.
    if !daemon_answers(&paths) {
        return ReloadOutcome::NotRunning;
    }
    // SIGHUP wakes the daemon's self-pipe; it reloads and swaps derived state without dropping
    // control clients or notify history. Best-effort: an invalid reloaded config is kept-old.
    match Pid::from_raw(pid) {
        Some(p) => match kill_process(p, Signal::HUP) {
            Ok(()) => ReloadOutcome::Signaled,
            Err(e) => ReloadOutcome::Failed(e.to_string()),
        },
        None => ReloadOutcome::Failed("daemon pid is invalid".to_string()),
    }
}

/// Outcome of a stop request. Same shape as [`ReloadOutcome`] so the two management verbs report
/// the same four situations in the same words.
pub enum StopOutcome {
    /// No tmux server for this handle ⇒ no daemon to stop.
    NoServer,
    /// A live daemon was signalled and is gone: its socket no longer answers AND its
    /// single-instance lock is free, so a replacement can be spawned straight away.
    Stopped,
    /// No daemon is currently running for this server — a no-op, not an error.
    NotRunning,
    /// A daemon is running but could not be stopped (unreadable pid, a failed signal, or it was
    /// still holding the lock when the budget ran out).
    Failed(String),
}

/// How long [`stop_daemon_at`] waits out a signalled daemon. A measured shutdown unlinks the socket
/// and exits in under 10 ms, so this is two orders of magnitude of headroom: reaching it means the
/// daemon is wedged, not that the box was busy.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// The poll step while waiting out a signalled daemon.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Stop the per-server daemon (`tma daemon --restart`'s first half). Tier-2 like [`reload_daemon`],
/// so the bin reaches the daemon without importing tier 3.
pub fn stop_daemon(tmux: &Tmux) -> StopOutcome {
    let Some(socket_path) = resolve_socket_path(tmux) else {
        return StopOutcome::NoServer;
    };
    stop_daemon_at(&paths_for(&socket_path))
}

/// [`stop_daemon`] against already-resolved paths, for a caller that holds them (the daemon's own
/// `--restart` / upgrade-eviction paths). Never returns [`StopOutcome::NoServer`]: the paths ARE the
/// server. SIGTERM only — see the inline note on why SIGKILL is never an escalation here.
pub fn stop_daemon_at(paths: &Paths) -> StopOutcome {
    // Liveness gate, exactly as `reload_daemon` does it: a successful connect proves a daemon is
    // accepting right now, and the connection is dropped without a frame, so it reads EOF.
    if !daemon_answers(paths) {
        return StopOutcome::NotRunning;
    }
    let Some(pid) = read_lock(&paths.lock).map(|l| l.pid) else {
        return StopOutcome::Failed(
            "daemon is running but its pid is unavailable (lock file empty or stale)".to_string(),
        );
    };
    // Re-probe immediately before the kill, and for the reason `reload_daemon` documents: the pid
    // read is a filesystem round-trip during which the daemon could exit and its pid be recycled.
    // This narrows the recycle window to the gap below; it cannot close it. Local-user-only.
    if !daemon_answers(paths) {
        return StopOutcome::NotRunning;
    }
    let Some(p) = Pid::from_raw(pid) else {
        return StopOutcome::Failed("daemon pid is invalid".to_string());
    };
    // SIGTERM, and never an escalation to SIGKILL: the daemon reaps its `tmux -C` control clients
    // only on a clean exit (`ControlClient::drop`), so a killed daemon orphans one control client
    // per monitored session. A daemon that will not take SIGTERM is reported, not killed.
    if let Err(err) = kill_process(p, Signal::TERM) {
        return StopOutcome::Failed(err.to_string());
    }
    // Gone means the LOCK is free, not merely that the socket stopped answering: the daemon unlinks
    // its socket before its lock fd closes, so a respawn in that gap exits as a duplicate instance
    // and leaves nothing running at all. Polled rather than probed once for a second reason too —
    // measured on macOS, an flock released by closing its fd is not always visible as free to an
    // immediate re-lock; a retry a moment later succeeds.
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if !daemon_answers(paths) && lock_is_free(&paths.lock) {
            return StopOutcome::Stopped;
        }
        if Instant::now() >= deadline {
            return StopOutcome::Failed(format!(
                "daemon (pid {pid}) still holds the socket or the lock {STOP_TIMEOUT:?} after SIGTERM"
            ));
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// Whether a daemon is accepting on this server's socket right now. The one liveness probe every
/// management verb shares: connect and drop, so the daemon reads EOF and nothing is stamped.
pub fn daemon_answers(paths: &Paths) -> bool {
    UnixStream::connect(&paths.socket).is_ok()
}

/// Whether the single-instance flock is currently unheld — the "no daemon owns this server" test a
/// respawn needs. Takes the lock only to drop it again, and never creates the file: an absent lock
/// is trivially free.
fn lock_is_free(lock: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(lock) else {
        return true;
    };
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

// ---- the upgrade-restart decision -------------------------------------------------------

/// What [`restart_decision`] resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Leave the running daemon where it is.
    Hold,
    /// The running daemon is strictly older than this build: stop this pid and respawn.
    Evict { pid: i32 },
}

/// The floor between two automatic restarts of the same server. Anti-symmetry makes a *version*
/// loop impossible, but it cannot stop a flap: if the newer build's daemon fails to come up and
/// something keeps restarting the older one, the eviction is correct every time and fires as often
/// as the check runs (a status-line driver runs it about once a second). This bounds that to once a
/// minute. The explicit `tma daemon --restart` is never subject to it.
pub const RESTART_COOLDOWN: Duration = Duration::from_secs(60);

/// Whether this build should evict the daemon a lock file describes. **Strictly newer evicts
/// older**; equal never restarts, and older NEVER evicts newer.
///
/// That asymmetry is the loop guard, and it is not arbitrary: it is the direction of skew the
/// protocol already tolerates. A new capability is a discriminant an old peer rejects cleanly, so a
/// newer daemon serving an older client degrades safely, while an older daemon serving a newer
/// client is the harmful direction (it can map an event to a verdict this build no longer agrees
/// with, ACK it, and leave the client thinking the stamp was written). Because the relation is
/// strict, at most one of any two builds can ever evict the other, so two installs cannot take
/// turns — the property test pins exactly that.
///
/// Every other input is a veto: an absent or unparseable version on either side, a recorded pid
/// that is not alive, or a restart already fired inside [`RESTART_COOLDOWN`]. Pure, so the whole
/// rule is unit-testable without a process in sight.
pub fn restart_decision(
    my_version: &str,
    lock: Option<&LockInfo>,
    pid_alive: bool,
    last_restart_ms: Option<u64>,
    now_ms: u64,
) -> RestartDecision {
    let Some(lock) = lock else {
        return RestartDecision::Hold;
    };
    // The recorded pid must be alive. A lock file keeps its body after the daemon exits (only the
    // flock is released), and a daemon that has taken the flock but not yet stamped its own body
    // leaves it EMPTY — both read as "nothing to act on" rather than as a version to compare.
    if !pid_alive {
        return RestartDecision::Hold;
    }
    let (Some(theirs), Some(mine)) = (
        lock.version.as_deref().and_then(parse_version),
        parse_version(my_version),
    ) else {
        return RestartDecision::Hold;
    };
    if mine <= theirs {
        return RestartDecision::Hold;
    }
    // A clock that jumped backwards leaves the stamp in the future, which `saturating_sub` reads as
    // zero elapsed — inside the cooldown, so it holds. The fail-safe direction.
    if last_restart_ms
        .is_some_and(|at| now_ms.saturating_sub(at) < RESTART_COOLDOWN.as_millis() as u64)
    {
        return RestartDecision::Hold;
    }
    RestartDecision::Evict { pid: lock.pid }
}

/// Parse a `MAJOR.MINOR.PATCH` build version into a comparable tuple. A pre-release or build suffix
/// (`-rc.1`, `+meta`) is dropped, so an rc and its release compare EQUAL and therefore never evict
/// each other — the conservative reading, since nothing here needs to rank them. `None` for
/// anything that is not three numbers, and `None` never evicts.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let (major, minor, patch) = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some((major, minor, patch))
}

/// The upgrade-restart verdict for whichever daemon currently holds this server's lock: read the
/// lock body, probe the recorded pid, read the cooldown stamp, and apply [`restart_decision`]. The
/// impure half, kept to one function so the rule itself stays testable without processes.
pub fn upgrade_restart_decision(paths: &Paths, my_version: &str, now_ms: u64) -> RestartDecision {
    let lock = read_lock(&paths.lock);
    let alive = lock.as_ref().is_some_and(|l| pid_is_live(l.pid));
    let last = std::fs::read_to_string(&paths.restart_stamp)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    restart_decision(my_version, lock.as_ref(), alive, last, now_ms)
}

/// Record that an automatic restart is being attempted for this server. Written BEFORE the signal,
/// so an attempt that then fails still counts against [`RESTART_COOLDOWN`] — the point of the
/// cooldown is precisely the case where the restart does not stick. Best-effort: an unwritable
/// runtime dir costs the cooldown, not the restart.
pub fn note_restart(paths: &Paths, now_ms: u64) {
    let _ = std::fs::write(&paths.restart_stamp, now_ms.to_string());
}

/// Whether `pid` still exists (a signal-0 probe). Anything but ESRCH counts as alive — EPERM means
/// the process is there and simply is not ours — so only a certain absence reads as dead.
fn pid_is_live(pid: i32) -> bool {
    let Some(p) = Pid::from_raw(pid) else {
        return false;
    };
    !matches!(rustix::process::test_kill_process(p), Err(Errno::SRCH))
}

// ---- wire protocol -----------------------------------------------------------------------

/// Serialize one event: `MAGIC` then four `u32`-LE length-prefixed fields (`pane`, `agent`, `kind`,
/// `payload`). Length-prefixed, not delimited, since the JSON payload carries newlines to escape.
/// Public so the daemon-side tests speak the wire format through the same encoder the client sink
/// uses, rather than re-deriving it, which keeps this the single owner of the frame layout.
pub fn encode_frame(pane: &str, agent: &str, kind: &str, payload: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + 16 + payload.len());
    buf.extend_from_slice(MAGIC);
    for field in [pane, agent, kind, payload] {
        buf.extend_from_slice(&(field.len() as u32).to_le_bytes());
        buf.extend_from_slice(field.as_bytes());
    }
    buf
}

/// Whole-frame read budget: the wall-clock cap on one complete frame. A per-`read` `SO_RCVTIMEO`
/// resets each syscall, so this bounds how long one dribbling client stalls the accept path. The
/// daemon also uses it as each parked connection's absolute drop deadline (`accept + FRAME_DEADLINE`).
pub const FRAME_DEADLINE: Duration = Duration::from_secs(2);

/// A classified inbound connection: a hook event ([`Frame`], `TMA1`) or a `tma wait` push
/// subscription ([`Inbound::Subscribe`], `SUBSCRIBE_MAGIC`). An event is acked and dropped; a
/// subscription is retained and fed [`PUSH`] wakes.
pub enum Inbound {
    Event(Frame),
    Subscribe,
    /// A transition-history read: the daemon answers [`HIST_ACK`] + a length-prefixed document and
    /// closes. Carries no body, like a subscribe.
    History,
}

/// The result of parsing a (possibly partial) frame out of an in-memory buffer, without blocking.
/// The daemon parks a `NeedMore` connection in its poll set and retries after the next read; the
/// blocking [`read_inbound`] loops read+`parse_inbound` until it leaves `NeedMore`. Both share this
/// one decoder so the wire format lives in exactly one place.
pub enum ParseStatus {
    /// A whole frame decoded from the front of the buffer.
    Complete(Inbound),
    /// The buffer holds a valid-so-far frame prefix; read more bytes and parse again.
    NeedMore,
    /// Unrecoverable: unknown magic or an oversize length prefix (rejected before allocation) or
    /// non-UTF-8. The caller drops the connection.
    Invalid,
}

/// Classify + decode one frame from `buf` without consuming the stream, so a connection whose bytes
/// have not all arrived can be parked and retried. `Complete` when the leading `TMA1` frame or a
/// bodiless `TMAS` subscribe is fully present; `NeedMore` while the prefix is still valid but short;
/// `Invalid` on unknown magic or an oversize length prefix (checked against the field caps *before*
/// any allocation, exactly as the blocking reader did) or non-UTF-8. Trailing bytes past one frame
/// are ignored: a connection carries a single frame.
pub fn parse_inbound(buf: &[u8]) -> ParseStatus {
    match magic_kind(buf) {
        MagicKind::NeedMore => ParseStatus::NeedMore,
        MagicKind::Invalid => ParseStatus::Invalid,
        MagicKind::Subscribe => ParseStatus::Complete(Inbound::Subscribe),
        MagicKind::History => ParseStatus::Complete(Inbound::History),
        MagicKind::Event => {
            // Four length-prefixed fields after the 4-byte magic, small ids then the payload.
            let mut at = 4;
            let mut fields: Vec<String> = Vec::with_capacity(4);
            for max in [MAX_SMALL, MAX_SMALL, MAX_SMALL, MAX_PAYLOAD] {
                match take_field(buf, at, max) {
                    FieldStep::Field(s, next) => {
                        fields.push(s);
                        at = next;
                    }
                    FieldStep::NeedMore => return ParseStatus::NeedMore,
                    FieldStep::Invalid => return ParseStatus::Invalid,
                }
            }
            let mut it = fields.into_iter();
            ParseStatus::Complete(Inbound::Event(Frame {
                pane: it.next().unwrap(),
                agent: it.next().unwrap(),
                kind: it.next().unwrap(),
                payload: it.next().unwrap(),
            }))
        }
    }
}

/// The magic classification of a buffer's first four bytes.
enum MagicKind {
    Event,
    Subscribe,
    History,
    NeedMore,
    Invalid,
}

/// Classify the leading magic. A byte diverging from EVERY known magic is `Invalid` at once (garbage
/// costs nothing, and a short garbage buffer never parks); a short but still-compatible prefix is
/// `NeedMore`. The magics share their `TMA` prefix, so this stays derived from [`MAGICS`] rather than
/// repeating the literals.
fn magic_kind(buf: &[u8]) -> MagicKind {
    for i in 0..buf.len().min(4) {
        if !MAGICS.iter().any(|m| m[i] == buf[i]) {
            return MagicKind::Invalid;
        }
    }
    if buf.len() < 4 {
        return MagicKind::NeedMore;
    }
    match &buf[..4] {
        m if m == MAGIC => MagicKind::Event,
        m if m == SUBSCRIBE_MAGIC => MagicKind::Subscribe,
        m if m == HISTORY_MAGIC => MagicKind::History,
        _ => MagicKind::Invalid,
    }
}

/// One decoded length-prefixed field, or why not.
enum FieldStep {
    /// The field and the buffer offset just past it.
    Field(String, usize),
    NeedMore,
    Invalid,
}

/// Decode the `u32`-LE length-prefixed UTF-8 field at offset `at`, capped at `max`. The cap is
/// checked on the length value before slicing, so a hostile prefix is `Invalid` without allocation.
fn take_field(buf: &[u8], at: usize, max: usize) -> FieldStep {
    let body_at = at + 4;
    if buf.len() < body_at {
        return FieldStep::NeedMore;
    }
    let len = u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize;
    if len > max {
        return FieldStep::Invalid;
    }
    let end = body_at + len;
    if buf.len() < end {
        return FieldStep::NeedMore;
    }
    match std::str::from_utf8(&buf[body_at..end]) {
        Ok(s) => FieldStep::Field(s.to_string(), end),
        Err(_) => FieldStep::Invalid,
    }
}

/// Read and classify one inbound connection under `FRAME_DEADLINE`, returning `None` on any
/// malformation (unknown magic, oversize/truncated field, non-UTF-8, EOF mid-frame, deadline). A
/// single bad client never crashes a caller nor wedges it. The blocking convenience over
/// [`parse_inbound`]; the daemon drives the non-blocking parser directly.
pub fn read_inbound(stream: &mut UnixStream) -> Option<Inbound> {
    read_inbound_by(stream, Instant::now() + FRAME_DEADLINE)
}

/// [`read_inbound`] against an explicit `deadline` (the test seam). Reads chunks under the shrinking
/// budget into a growing buffer, re-running [`parse_inbound`] after each, so the wire-format decode
/// stays in one place. `NeedMore` reads on; `Complete`/`Invalid` return; a read that hits the
/// deadline or EOF mid-frame yields `None`.
fn read_inbound_by(stream: &mut UnixStream, deadline: Instant) -> Option<Inbound> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match parse_inbound(&buf) {
            ParseStatus::Complete(inbound) => return Some(inbound),
            ParseStatus::Invalid => return None,
            ParseStatus::NeedMore => {}
        }
        let n = read_some_by(stream, &mut chunk, deadline)?;
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read one hook-event frame; `None` on any malformation or a subscribe frame (event-only callers
/// want an event or nothing). Thin wrapper over `read_inbound_by`.
pub fn read_frame(stream: &mut UnixStream) -> Option<Frame> {
    read_frame_by(stream, Instant::now() + FRAME_DEADLINE)
}

/// [`read_frame`] against an explicit whole-frame `deadline` (the seam the deadline unit test
/// drives).
fn read_frame_by(stream: &mut UnixStream, deadline: Instant) -> Option<Frame> {
    match read_inbound_by(stream, deadline)? {
        Inbound::Event(frame) => Some(frame),
        Inbound::Subscribe | Inbound::History => None,
    }
}

/// Read once under `deadline`: `poll` for readability bounded by the remaining budget, then a single
/// `read`. `Some(n)` with `n > 0` bytes; `None` on the deadline, EOF, or any I/O error. `poll`, not
/// `set_read_timeout`, because macOS rejects `setsockopt(SO_RCVTIMEO)` with EINVAL once the peer has
/// closed (a client that sent its frame and closed the write end, or a bare connect-probe). The read
/// after a positive poll returns promptly.
fn read_some_by(stream: &mut UnixStream, chunk: &mut [u8], deadline: Instant) -> Option<usize> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())?; // whole-frame deadline exceeded
        let ts = poll_timeout(remaining);
        // Scope the borrow so the mutable read below is free of the PollFd's immutable borrow.
        match poll(&mut [PollFd::new(&*stream, PollFlags::IN)], Some(&ts)) {
            Ok(0) => return None, // no readability within the remaining budget: deadline reached
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => return None,
        }
        // Readable (POLLIN) or a hangup/error (POLLHUP/POLLERR are reported regardless of `events`).
        match stream.read(chunk) {
            Ok(0) => return None, // EOF
            Ok(n) => return Some(n),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

// ---- client (the daemon delivery seam) ---------------------------------------------------

/// A client of the per-server daemon socket ([`paths_for`]), keyed on the same `#{socket_path}` the
/// daemon binds. The delivery seam: [`deliver`](DaemonSink::deliver) hands the raw event to a
/// running daemon and reports `false` on any failure (no socket, connect refused, write error,
/// NAK/timeout/EOF), so `tma event` falls through to a direct guarded stamp.
pub(crate) struct DaemonSink {
    pub(crate) path: PathBuf,
}

/// Connect to the daemon socket without ever blocking the hook. `UnixStream::connect`'s blocking
/// `connect(2)` can stall on a full accept backlog while the daemon is mid-startup (its ~2 s
/// control-mode probe runs before the accept loop drains). So connect on a non-blocking fd: an
/// AF_UNIX `SOCK_STREAM` connect completes synchronously (no handshake), enqueuing onto the backlog
/// or returning `EAGAIN` when it is full. Any error (`EAGAIN`, `ECONNREFUSED`, `ENOENT`) yields
/// `None` and `tma event` direct-stamps. Restored to blocking on success so caller timeouts apply.
fn hook_connect(path: &Path) -> Option<UnixStream> {
    let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).ok()?;
    // macOS has no SOCK_CLOEXEC, so set FD_CLOEXEC via a separate fcntl; the brief fork-inherit
    // window between socket() and here is unchanged from the old code and local-user-only.
    fcntl_setfd(&fd, FdFlags::CLOEXEC).ok()?;
    let stream = UnixStream::from(fd); // OwnedFd -> UnixStream, owning the fd from here on.
    stream.set_nonblocking(true).ok()?;
    // SocketAddrUnix::new rejects a path too long for sun_path (never our keyed socket paths).
    let addr = SocketAddrUnix::new(path).ok()?;
    // Any connect error (EAGAIN backlog-full, ECONNREFUSED, ENOENT) returns None immediately so the
    // hook never waits; a non-blocking AF_UNIX connect completes synchronously (no handshake).
    connect(&stream, &addr).ok()?;
    // Connected (queued in the backlog). Restore blocking so set_write_timeout / the ack read work.
    stream.set_nonblocking(false).ok()?;
    Some(stream)
}

impl DaemonSink {
    /// `true` if the daemon accepted the event (tma is then done, and the daemon owns the
    /// stamp + any notification); `false` to fall through to a direct stamp.
    pub(crate) fn deliver(&self, pane: &str, agent: &str, kind: &str, payload: &str) -> bool {
        // Non-blocking connect ([`hook_connect`]): a missing/stale socket or a full backlog degrades
        // to an immediate direct-stamp rather than stalling the hook.
        let Some(mut stream) = hook_connect(&self.path) else {
            return false;
        };
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let frame = encode_frame(pane, agent, kind, payload);
        if stream.write_all(&frame).is_err() {
            return false;
        }
        // A write success is not delivery: the daemon can still drop the frame. Treat only an
        // explicit ACK as delivered; a NAK/timeout/EOF (an old daemon writes nothing) direct-stamps,
        // idempotent under guarded writes. See [`ACK`].
        let _ = stream.set_read_timeout(Some(ACK_TIMEOUT));
        let mut ack = [0u8; 1];
        matches!(stream.read_exact(&mut ack), Ok(()) if ack[0] == ACK)
    }
}

// ---- client (the transition-history read) --------------------------------------------

/// How long the history client waits for the daemon's answer. Generous next to [`ACK_TIMEOUT`] only
/// because the daemon may be mid-sweep; the document itself is small and written in one go.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on an answered history document, so a hostile or confused peer cannot force a large read. The
/// ring is 256 records of ~80 bytes, well inside this.
const MAX_HISTORY: usize = 1 << 20;

/// What a transition-history read resolved to.
pub enum HistoryOutcome {
    /// The daemon answered; the payload is a [`crate::transitions`] document.
    Document(String),
    /// No tmux server for this handle.
    NoServer,
    /// No daemon is running for this server (nothing keeps a transition history).
    NotRunning,
    /// A daemon answered but does not speak the history discriminant: it predates this build, so it
    /// rejected the magic. Restarting the daemon is the fix (a SIGHUP reload cannot add a verb).
    Unsupported,
}

/// Read the running daemon's transition ring. Follows this module's compat rule: an old daemon
/// classifies the unknown magic as garbage and NAKs, which surfaces as
/// [`HistoryOutcome::Unsupported`] rather than an error, so a version-skewed client degrades cleanly.
pub fn fetch_transitions(tmux: &Tmux) -> HistoryOutcome {
    let Some(socket_path) = resolve_socket_path(tmux) else {
        return HistoryOutcome::NoServer;
    };
    let path = paths_for(&socket_path).socket;
    let Some(mut stream) = hook_connect(&path) else {
        return HistoryOutcome::NotRunning;
    };
    let _ = stream.set_write_timeout(Some(HISTORY_TIMEOUT));
    if stream.write_all(HISTORY_MAGIC).is_err() {
        return HistoryOutcome::NotRunning;
    }
    let _ = stream.set_read_timeout(Some(HISTORY_TIMEOUT));
    let mut head = [0u8; 5];
    if stream.read_exact(&mut head).is_err() || head[0] != HIST_ACK {
        return HistoryOutcome::Unsupported;
    }
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_HISTORY {
        return HistoryOutcome::Unsupported;
    }
    let mut body = vec![0u8; len];
    match stream.read_exact(&mut body) {
        Ok(()) => match String::from_utf8(body) {
            Ok(text) => HistoryOutcome::Document(text),
            Err(_) => HistoryOutcome::Unsupported,
        },
        Err(_) => HistoryOutcome::Unsupported,
    }
}

/// Answer a history request on `stream`: [`HIST_ACK`], the `u32`-LE body length, then the document.
/// Best-effort — a client that vanished mid-write is not the daemon's problem. Lives here so the
/// answer and [`fetch_transitions`] share one framing.
pub fn write_history(stream: &mut UnixStream, document: &str) {
    let body = document.as_bytes();
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(HIST_ACK);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    let _ = stream.write_all(&out);
}

// ---- client (the `tma wait` push subscription) --------------------------------------

/// A `tma wait` push subscription: a latency-only upgrade over polling. `wait` stays
/// cycle-authoritative (a [`PUSH`] is a wake hint, not evidence), so this carries no state off the
/// wire. [`try_subscribe`](WaitSubscription::try_subscribe) degrades to `None` on every failure, so
/// the caller never surfaces an error.
pub struct WaitSubscription {
    stream: UnixStream,
}

/// Why [`WaitSubscription::wait_edge`] returned. `tma wait` treats `Pushed` and `Elapsed` alike
/// (re-cycle either way, the cycle is the authority); `tma subscribe` distinguishes them, emitting
/// unconditionally on a real edge but only on change for the belt.
pub enum WaitWake {
    /// A state-affecting edge push arrived (its bytes drained). Re-run a cycle.
    Pushed,
    /// The fallback cap elapsed (or a spurious wake): re-run a cycle as a belt.
    Elapsed,
    /// The daemon dropped the connection (died/restarted mid-wait). The caller degrades to the poll
    /// loop for the rest of the wait; nothing is lost.
    Closed,
}

impl WaitSubscription {
    /// Try to subscribe to the daemon's edge pushes. `None` (the caller polls) on every degrade path:
    /// no server, no daemon or full backlog, an old daemon that NAKs, or any I/O error.
    pub fn try_subscribe(tmux: &Tmux) -> Option<WaitSubscription> {
        let socket_path = resolve_socket_path(tmux)?;
        let path = paths_for(&socket_path).socket;
        // The same non-blocking probe the hook path uses: a missing/stale socket or full backlog
        // declines immediately, so the no-daemon case costs no wait before falling back to polling.
        let mut stream = hook_connect(&path)?;
        let _ = stream.set_write_timeout(Some(SUBSCRIBE_TIMEOUT));
        if stream.write_all(SUBSCRIBE_MAGIC).is_err() {
            return None;
        }
        // One SUB_ACK, written before any push, confirms a live push-capable daemon. An old daemon
        // NAKs/closes TMAS (bad magic), a non-SUB_ACK byte, EOF, or timeout all fall back to polling.
        let _ = stream.set_read_timeout(Some(SUBSCRIBE_TIMEOUT));
        let mut ack = [0u8; 1];
        match stream.read_exact(&mut ack) {
            Ok(()) if ack[0] == SUB_ACK => Some(WaitSubscription { stream }),
            _ => None,
        }
    }

    /// Block until the next edge push or `cap` (the fallback cadence, clamped to `--timeout`)
    /// elapses. [`WaitWake::Pushed`] on a push, [`WaitWake::Elapsed`] on `cap` or a spurious wake
    /// (both ⇒ re-run a cycle), [`WaitWake::Closed`] when the daemon dropped us. `poll(2)`, not
    /// read-with-timeout, for the same macOS EINVAL reason as `read_some_by`.
    pub fn wait_edge(&mut self, cap: Duration) -> WaitWake {
        let ts = poll_timeout(cap);
        // Scope the borrow so the read below has sole access to the stream.
        let prc = poll(&mut [PollFd::new(&self.stream, PollFlags::IN)], Some(&ts));
        if !matches!(prc, Ok(n) if n > 0) {
            // cap elapsed (0) or EINTR/poll error: re-run a cycle as a belt, a spurious wake costs one.
            return WaitWake::Elapsed;
        }
        // Readable or hangup. After a positive poll the read returns promptly; drain the coalesced
        // push bytes. Ok(0) EOF or a hard error means the daemon is gone.
        let mut buf = [0u8; 64];
        match self.stream.read(&mut buf) {
            Ok(0) => WaitWake::Closed,
            Ok(_) => WaitWake::Pushed,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => WaitWake::Elapsed,
            Err(_) => WaitWake::Closed,
        }
    }

    /// Absorb further pushes arriving within `window` into the current wake — the 100 ms debounce,
    /// so an edge burst collapses to one emission rather than one per byte. Drains readable bytes
    /// without blocking past `window`; `false` when the peer hung up mid-window (the caller degrades
    /// to polling), `true` when the window elapsed still connected.
    pub fn coalesce(&mut self, window: Duration) -> bool {
        let deadline = Instant::now() + window;
        loop {
            let Some(remaining) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                return true; // window elapsed, still connected
            };
            let ts = poll_timeout(remaining);
            let prc = poll(&mut [PollFd::new(&self.stream, PollFlags::IN)], Some(&ts));
            match prc {
                Ok(0) => return true,  // nothing more within the window
                Ok(_) => {}            // readable or hangup: drain below
                Err(_) => return true, // EINTR/poll error: treat as the window elapsing
            }
            let mut buf = [0u8; 64];
            match self.stream.read(&mut buf) {
                Ok(0) => return false, // EOF: the daemon is gone
                Ok(_) => continue, // drained a push byte; keep absorbing until the window closes
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `VAR=` means "I did not set this". Honoring an empty `XDG_RUNTIME_DIR` would put the daemon
    /// socket at a cwd-relative `tma/` path, so a client and a daemon started from different
    /// directories would silently never find each other.
    #[test]
    fn an_empty_runtime_var_falls_through_as_unset() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            runtime_base_from(some("/run/user/501"), some("/var/tmp")),
            PathBuf::from("/run/user/501")
        );
        assert_eq!(
            runtime_base_from(some(""), some("/var/tmp")),
            PathBuf::from("/var/tmp"),
            "an empty XDG_RUNTIME_DIR falls through to TMPDIR"
        );
        assert_eq!(
            runtime_base_from(some(""), some("")),
            PathBuf::from("/tmp"),
            "both empty lands on the same floor as both unset"
        );
        assert_eq!(runtime_base_from(None, None), PathBuf::from("/tmp"));
    }

    #[test]
    fn socket_key_is_pure_function_of_socket_path() {
        // Deterministic: same input, same key across calls.
        let a = socket_key("/tmp/tmux-501/default");
        let b = socket_key("/tmp/tmux-501/default");
        assert_eq!(a, b);
        // 16 hex chars (a u64).
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn distinct_socket_paths_hash_to_distinct_filenames() {
        // Two tmux servers (distinct `#{socket_path}`) must never share a daemon.
        let one = paths_for("/tmp/tmux-501/default");
        let two = paths_for("/tmp/tmux-501/work");
        assert_ne!(one.socket, two.socket);
        assert_ne!(one.lock, two.lock);
        // Same server ⇒ same socket AND lock, so `tma event` and `--ensure` collide (as
        // intended) on exactly one endpoint.
        let again = paths_for("/tmp/tmux-501/default");
        assert_eq!(one.socket, again.socket);
        assert_eq!(one.lock, again.lock);
    }

    #[test]
    fn socket_and_lock_share_the_stem_and_dir() {
        let p = paths_for("/tmp/tmux-501/default");
        let key = socket_key("/tmp/tmux-501/default");
        assert_eq!(p.socket, p.dir.join(format!("{key}.sock")));
        assert_eq!(p.lock, p.dir.join(format!("{key}.lock")));
        assert!(p.dir.ends_with("tma"));
    }

    #[test]
    fn lock_file_round_trips_and_the_old_bare_pid_format_still_parses() {
        let body = render_lock(4242, "0.9.1");
        let info = parse_lock(&body).expect("the rendered body parses");
        assert_eq!(info.pid, 4242);
        assert_eq!(info.version.as_deref(), Some("0.9.1"));

        // A lock file written before the version was recorded: pid only, no trailing newline.
        let old = parse_lock("4242").expect("a bare pid is still a valid lock file");
        assert_eq!(old.pid, 4242);
        assert_eq!(old.version, None);
        assert_eq!(parse_lock("4242\n").map(|l| l.pid), Some(4242));

        // Nothing usable: empty, non-numeric, or a nonsense pid.
        assert!(parse_lock("").is_none());
        assert!(parse_lock("not-a-pid\nversion=1.0\n").is_none());
        assert!(parse_lock("0\n").is_none());
    }

    /// A lock body as the daemon on `version` would have written it, with `pid`.
    fn lock_of(pid: i32, version: &str) -> LockInfo {
        parse_lock(&render_lock(pid as u32, version)).expect("a rendered lock body parses")
    }

    /// The whole rule with the vetoes cleared: pid alive, no cooldown stamp.
    fn decide(my_version: &str, lock: &LockInfo) -> RestartDecision {
        restart_decision(my_version, Some(lock), true, None, 0)
    }

    #[test]
    fn only_a_strictly_newer_build_evicts_the_running_daemon() {
        // Strictly newer, at each position of the tuple.
        for (mine, theirs) in [
            ("0.4.4", "0.3.5"),
            ("1.0.0", "0.9.9"),
            ("0.4.4", "0.4.3"),
            ("0.10.0", "0.9.0"),
        ] {
            assert_eq!(
                decide(mine, &lock_of(4242, theirs)),
                RestartDecision::Evict { pid: 4242 },
                "{mine} must evict {theirs}"
            );
        }
        // Equal never restarts, and older never evicts newer (the downgrade is served by the
        // explicit `tma daemon --restart`, not by this rule).
        for (mine, theirs) in [("0.4.4", "0.4.4"), ("0.3.5", "0.4.4"), ("0.9.0", "0.10.0")] {
            assert_eq!(
                decide(mine, &lock_of(4242, theirs)),
                RestartDecision::Hold,
                "{mine} must leave {theirs} alone"
            );
        }
        // `10` sorts after `9` as a number and before it as a string: the compare is numeric.
        assert_eq!(
            parse_version("0.10.0").cmp(&parse_version("0.9.0")),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn a_version_that_cannot_be_read_is_never_a_restart_trigger() {
        // No lock at all, and a lock predating version recording (a bare pid).
        assert_eq!(
            restart_decision("9.9.9", None, true, None, 0),
            RestartDecision::Hold
        );
        let bare = parse_lock("4242").expect("a bare pid is a valid lock file");
        assert_eq!(decide("9.9.9", &bare), RestartDecision::Hold);
        // A body the daemon has not stamped yet: the flock is held, the version is unknown.
        assert!(parse_lock("").is_none());
        // Junk on either side.
        for theirs in ["nightly", "0.4", "0.4.4.1", "v0.4.4", ""] {
            assert_eq!(
                decide("9.9.9", &lock_of(7, theirs)),
                RestartDecision::Hold,
                "{theirs:?} is not a version this rule can rank"
            );
        }
        assert_eq!(
            decide("git-build", &lock_of(7, "0.0.1")),
            RestartDecision::Hold,
            "a build whose OWN version is unrankable evicts nothing"
        );
        // A pre-release and its release read as the same version, so neither evicts the other.
        assert_eq!(
            decide("0.4.4", &lock_of(7, "0.4.4-rc.1")),
            RestartDecision::Hold
        );
        assert_eq!(
            decide("0.4.4-rc.1", &lock_of(7, "0.4.4")),
            RestartDecision::Hold
        );
    }

    #[test]
    fn eviction_requires_a_live_pid() {
        let old = lock_of(4242, "0.0.1");
        assert_eq!(decide("0.4.4", &old), RestartDecision::Evict { pid: 4242 });
        assert_eq!(
            restart_decision("0.4.4", Some(&old), false, None, 0),
            RestartDecision::Hold,
            "a lock file keeps its body after the daemon exits; signalling that pid could hit a \
             recycled process, and there is nothing to evict anyway"
        );
    }

    #[test]
    fn the_cooldown_bounds_a_restart_that_does_not_stick() {
        let old = lock_of(4242, "0.0.1");
        let cooldown = RESTART_COOLDOWN.as_millis() as u64;
        let evict = RestartDecision::Evict { pid: 4242 };
        // Nothing recorded, or the last attempt is older than the cooldown: the eviction stands.
        assert_eq!(
            restart_decision("0.4.4", Some(&old), true, None, 1_000_000),
            evict
        );
        assert_eq!(
            restart_decision(
                "0.4.4",
                Some(&old),
                true,
                Some(1_000_000),
                1_000_000 + cooldown
            ),
            evict
        );
        // Inside the window it holds, however correct the eviction is — this is the flap guard, so
        // it deliberately overrules a true "the running daemon is older".
        assert_eq!(
            restart_decision(
                "0.4.4",
                Some(&old),
                true,
                Some(1_000_000),
                1_000_000 + cooldown - 1
            ),
            RestartDecision::Hold
        );
        // A stamp in the future (a clock that went backwards) reads as zero elapsed, so it holds.
        assert_eq!(
            restart_decision("0.4.4", Some(&old), true, Some(2_000_000), 1_000_000),
            RestartDecision::Hold
        );
    }

    /// The anti-symmetry theorem, the whole reason this rule cannot loop: for ANY pair of builds, at
    /// most one direction is an eviction. Two installs racing each other over one server would
    /// otherwise churn a real tmux probe session and every control client, once per check — about
    /// once a second under a status-line driver.
    ///
    /// Every veto is switched off here (pid alive, no cooldown) so the property is proved of the
    /// version rule itself rather than of the guards around it.
    #[test]
    fn no_two_builds_can_ever_evict_each_other() {
        use proptest::prelude::*;

        // A generator that mixes well-formed triples with the strings the rule must refuse.
        let version = prop_oneof![
            (0u64..3, 0u64..12, 0u64..12).prop_map(|(a, b, c)| format!("{a}.{b}.{c}")),
            (0u64..3, 0u64..12, 0u64..12).prop_map(|(a, b, c)| format!("{a}.{b}.{c}-rc.1")),
            Just("0.4".to_string()),
            Just("nightly".to_string()),
            Just(String::new()),
        ];
        proptest!(|((a, b) in (version.clone(), version))| {
            let a_evicts_b = decide(&a, &lock_of(11, &b));
            let b_evicts_a = decide(&b, &lock_of(22, &a));
            prop_assert!(
                !(matches!(a_evicts_b, RestartDecision::Evict { .. })
                    && matches!(b_evicts_a, RestartDecision::Evict { .. })),
                "{a} and {b} evict each other, which is a restart loop"
            );
            // And the diagonal: a build never evicts itself, so a matched pair is quiescent.
            prop_assert_eq!(decide(&a, &lock_of(11, &a)), RestartDecision::Hold);
        });
    }

    #[test]
    fn frame_round_trips_through_a_unix_socket() {
        // Encode → write to one end of a socketpair → read_frame the other end.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let frame = encode_frame("%1", "claude", "Notification", r#"{"session_id":"s"}"#);
        a.write_all(&frame).unwrap();
        drop(a); // EOF after the frame
        let ev = read_frame(&mut b).expect("valid frame parses");
        assert_eq!(ev.pane, "%1");
        assert_eq!(ev.agent, "claude");
        assert_eq!(ev.kind, "Notification");
        assert_eq!(ev.payload, r#"{"session_id":"s"}"#);
    }

    #[test]
    fn read_inbound_classifies_event_and_subscribe() {
        // A TMA1 event frame round-trips as Inbound::Event with its fields intact.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&encode_frame("%1", "claude", "Stop", "{}"))
            .unwrap();
        drop(a);
        assert!(matches!(read_inbound(&mut b), Some(Inbound::Event(f)) if f.pane == "%1"));

        // A bare TMAS subscribe magic (no body) round-trips as Inbound::Subscribe.
        let (mut c, mut d) = UnixStream::pair().unwrap();
        c.write_all(SUBSCRIBE_MAGIC).unwrap();
        drop(c);
        assert!(matches!(read_inbound(&mut d), Some(Inbound::Subscribe)));
    }

    #[test]
    fn read_frame_rejects_a_subscribe_frame() {
        // The event-only reader treats a subscribe magic as not-an-event, so a version-skew subscribe
        // is dropped, never misparsed as a frame.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(SUBSCRIBE_MAGIC).unwrap();
        drop(a);
        assert!(read_frame(&mut b).is_none());
    }

    #[test]
    fn bad_magic_is_rejected_without_reading_a_body() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(b"XXXXsome trailing garbage").unwrap();
        drop(a);
        assert!(read_frame(&mut b).is_none());
    }

    #[test]
    fn oversize_length_prefix_is_rejected() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(MAGIC).unwrap();
        // A pane field claiming 4 GiB: must be refused, never allocated.
        a.write_all(&u32::MAX.to_le_bytes()).unwrap();
        drop(a);
        assert!(read_frame(&mut b).is_none());
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(MAGIC).unwrap();
        a.write_all(&(3u32).to_le_bytes()).unwrap(); // promises 3 bytes
        a.write_all(b"ab").unwrap(); // delivers 2, then EOF
        drop(a);
        assert!(read_frame(&mut b).is_none());
    }

    #[test]
    fn read_frame_aborts_a_slow_client_at_the_whole_frame_deadline() {
        // A client that sends the magic then stalls mid-frame (never sending the first length
        // prefix) and keeps the connection OPEN must not wedge the reader: the whole-frame deadline
        // aborts it. `a` is held open through the read so the abort is the deadline, not EOF.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(MAGIC).unwrap();
        let deadline = Instant::now() + Duration::from_millis(150);
        let start = Instant::now();
        assert!(read_frame_by(&mut b, deadline).is_none());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "aborted at the deadline, not blocked on the open connection: {elapsed:?}"
        );
        drop(a);
    }

    #[test]
    fn parse_inbound_completes_a_whole_event_frame() {
        let frame = encode_frame("%7", "claude", "Stop", r#"{"k":"v"}"#);
        match parse_inbound(&frame) {
            ParseStatus::Complete(Inbound::Event(f)) => {
                assert_eq!(f.pane, "%7");
                assert_eq!(f.agent, "claude");
                assert_eq!(f.kind, "Stop");
                assert_eq!(f.payload, r#"{"k":"v"}"#);
            }
            _ => panic!("a whole event frame must parse Complete"),
        }
        // A bare subscribe magic is a complete bodiless frame.
        assert!(matches!(
            parse_inbound(SUBSCRIBE_MAGIC),
            ParseStatus::Complete(Inbound::Subscribe)
        ));
    }

    #[test]
    fn parse_inbound_needs_more_at_every_split_then_completes() {
        // The incremental-parser property the parked-connection path relies on: a valid frame fed
        // one prefix at a time is NeedMore until the final byte, then Complete. Cover every boundary.
        let frame = encode_frame("%1", "claude", "Notification", r#"{"session_id":"s"}"#);
        for split in 0..frame.len() {
            assert!(
                matches!(parse_inbound(&frame[..split]), ParseStatus::NeedMore),
                "a {split}-byte prefix of a valid frame must be NeedMore, never Complete/Invalid"
            );
        }
        assert!(matches!(
            parse_inbound(&frame),
            ParseStatus::Complete(Inbound::Event(_))
        ));
    }

    #[test]
    fn parse_inbound_rejects_bad_magic_and_oversize_length() {
        // Garbage magic is Invalid as soon as one byte diverges from both known magics (never parks).
        assert!(matches!(parse_inbound(b"X"), ParseStatus::Invalid));
        assert!(matches!(parse_inbound(b"TMAX"), ParseStatus::Invalid));
        // A shared "TMA" prefix is still NeedMore (compatible with both magics).
        assert!(matches!(parse_inbound(b"TMA"), ParseStatus::NeedMore));

        // An oversize first-field length prefix is Invalid before any body allocation.
        let mut oversize = Vec::new();
        oversize.extend_from_slice(MAGIC);
        oversize.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(parse_inbound(&oversize), ParseStatus::Invalid));

        // Non-UTF-8 field bytes are Invalid.
        let mut bad_utf8 = Vec::new();
        bad_utf8.extend_from_slice(MAGIC);
        bad_utf8.extend_from_slice(&1u32.to_le_bytes());
        bad_utf8.push(0xff);
        assert!(matches!(parse_inbound(&bad_utf8), ParseStatus::Invalid));
    }

    #[test]
    fn hook_connect_reaches_a_live_listener_and_declines_a_dead_path() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("tma-hookconn-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        // Live listener: connect succeeds and yields a usable, blocking stream.
        let s = hook_connect(&sock).expect("connect to a live listener");
        assert!(
            s.set_write_timeout(Some(Duration::from_millis(50))).is_ok(),
            "the returned stream is a normal blocking socket"
        );
        drop(s);
        drop(listener);
        let _ = std::fs::remove_file(&sock);

        // No listener (socket path gone): declines without blocking.
        assert!(hook_connect(&sock).is_none());
        let _ = std::fs::remove_dir(&dir);
    }
}
