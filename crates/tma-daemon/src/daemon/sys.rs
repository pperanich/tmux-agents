//! Low-level unix plumbing over rustix (no libc): private dir + socket perms, the single-instance
//! flock, and the signal self-pipe feeding [`SignalAction`]. Best-effort; a failed chmod never fails.

use std::os::unix::io::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, LazyLock};

use rustix::io::Errno;

use super::SignalAction;

/// Set by the SIGTERM/SIGINT handler (signal-hook `flag::register`); the loop swaps it false on
/// drain. Shutdown wins over a coincident reload.
static SHUTDOWN: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));
/// Set by the SIGHUP handler; the loop swaps it false on drain to hot-reload config + manifests.
static RELOAD: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));

/// Create `dir` (and parents) with `0700` perms: the private directory the socket + lock
/// live in. Idempotent; tightens perms even if the dir already existed.
pub(super) fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    } else {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir)?.permissions();
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(dir, perms);
    }
    Ok(())
}

/// Best-effort `chmod 0600` on the bound socket (local-user-only access).
pub(super) fn set_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Non-blocking `flock(LOCK_EX)`. `true` = acquired (held until the `File` closes or the process
/// dies); `false` = another live process holds it. The kernel drops it on death, reclaiming stale locks.
pub(super) fn flock_nb(file: &std::fs::File) -> bool {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Remove the socket file on shutdown so a later daemon binds fresh and clients stop finding
/// a dead endpoint. Best-effort.
pub(super) fn cleanup(socket: &Path) {
    let _ = std::fs::remove_file(socket);
}

/// Create the self-pipe and register SIGTERM/SIGINT (shutdown) + SIGHUP (reload). Per signal the
/// flag is set FIRST, then the pipe write wakes the loop (signal-hook runs actions in registration
/// order, so the flag lands before the wake byte). Returns the read end, or `None` on failure. Both
/// ends are close-on-exec (no tmux child inherits them) and non-blocking (writes never block the
/// handler; the read drains to empty). Each pipe registration owns its own dup of the write end.
///
/// Single-use, process-lifetime: registered once, never unregistered (the process exits and the OS
/// reclaims the fds + handlers). signal-hook's registry owns the write end.
pub(super) fn install_signal_pipe() -> Option<OwnedFd> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let (read_fd, write_fd) = rustix::pipe::pipe().ok()?;
    for fd in [read_fd.as_fd(), write_fd.as_fd()] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).ok()?;
        let flags = rustix::fs::fcntl_getfl(fd).ok()? | rustix::fs::OFlags::NONBLOCK;
        rustix::fs::fcntl_setfl(fd, flags).ok()?;
    }
    signal_hook::flag::register(SIGTERM, Arc::clone(&SHUTDOWN)).ok()?;
    signal_hook::flag::register(SIGINT, Arc::clone(&SHUTDOWN)).ok()?;
    signal_hook::flag::register(SIGHUP, Arc::clone(&RELOAD)).ok()?;
    for sig in [SIGTERM, SIGINT, SIGHUP] {
        signal_hook::low_level::pipe::register(sig, write_fd.try_clone().ok()?).ok()?;
    }
    Some(read_fd)
}

/// Drain the signal self-pipe (non-blocking, until `AGAIN`/EOF so a burst coalesces), then classify
/// from the flags. The byte is only a wake; the flag carries the intent (at-least-once semantics: a
/// byte may arrive with the flag already consumed, or vice versa). Shutdown wins over a coincident reload.
pub(super) fn drain_signal<Fd: AsFd>(fd: Fd) -> SignalAction {
    let mut buf = [0u8; 64];
    loop {
        match rustix::io::read(fd.as_fd(), &mut buf) {
            Ok(0) | Err(Errno::AGAIN) => break, // drained
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => break,
        }
    }
    // Swap BOTH flags so a coincident reload is cleared even when shutdown wins (no stale reload).
    let shutdown = SHUTDOWN.swap(false, Relaxed);
    let reload = RELOAD.swap(false, Relaxed);
    if shutdown {
        SignalAction::Shutdown
    } else if reload {
        SignalAction::Reload
    } else {
        SignalAction::None
    }
}
