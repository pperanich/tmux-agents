//! The connection registry: what bounds concurrency, and what records which device holds a pipe.
//!
//! Each connection claims one of `max_connections` numbered marker files under the runtime dir,
//! beside the slot ledger. The cap exists because **every serve connection runs its own detection
//! cycle** (ARCHITECTURE §1.2, §1.8): connections cost tmux query throughput and nothing else
//! bounds them, so the fifth is refused with a typed error rather than accepted and starved.
//!
//! The claim is `O_EXCL` create on a numbered path rather than a lock around a count. The create is
//! already atomic, so two connections racing for the last slot cannot both win, and there is no
//! lock file to leave behind. A marker whose pid is gone is swept and re-claimed.
//!
//! This is a registry, not a queue: it holds a pid and a device id, never a payload. Nothing on
//! this path stores an unsent frame, which is the property the runtime-dir scan greps for.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The registry subdirectory under the runtime dir.
pub(crate) const REGISTRY_DIR: &str = "serve";

/// How many times a claim re-scans after sweeping a stale marker. Two passes: one to sweep, one to
/// take what the sweep freed. A third would only ever lose another race, which is a refusal anyway.
const PASSES: usize = 2;

/// This process's entry in the registry, removed on drop.
pub(crate) struct Connection {
    path: PathBuf,
}

impl Connection {
    /// Claim a connection slot under `runtime_dir`, or report the cap. `max` of zero is read as
    /// one: a host that configured the cap away still gets a serving process rather than none.
    pub(crate) fn open(
        runtime_dir: &Path,
        device: &str,
        max: usize,
    ) -> Result<Connection, Refused> {
        let dir = runtime_dir.join(REGISTRY_DIR);
        ensure_private_dir(&dir).map_err(Refused::Io)?;
        let max = max.max(1);
        let pid = std::process::id();
        for pass in 0..PASSES {
            for slot in 0..max {
                let path = dir.join(format!("slot-{slot}.conn"));
                match claim(&path, pid, device) {
                    Ok(()) => return Ok(Connection { path }),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Only the first pass sweeps: a marker written by a live process on the
                        // second pass belongs to a connection that beat us to the slot.
                        if pass == 0 && !holder_alive(&path) {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    Err(err) => return Err(Refused::Io(err)),
                }
            }
        }
        Err(Refused::Full { max })
    }

    /// This connection's marker, for a signal handler that has to clean up without running `Drop`.
    pub(crate) fn marker(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Why a connection could not be registered.
pub(crate) enum Refused {
    /// Every slot is held by a live process.
    Full {
        max: usize,
    },
    Io(std::io::Error),
}

/// Write the marker, failing when it already exists. `create_new` is the whole cap: the kernel
/// arbitrates, so two processes cannot both believe they took the last slot.
fn claim(path: &Path, pid: u32, device: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // Two facts and no third. `tma device revoke` reads the device line to find the pids to
    // SIGTERM; a payload here would be the store-and-forward queue v1 promises not to have.
    writeln!(file, "pid={pid}")?;
    writeln!(file, "device={device}")
}

/// Whether the process named in an existing marker is still running. An unreadable or unparsable
/// marker counts as dead: it cannot name a process to wait for.
fn holder_alive(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(tma_tmux::lock::pid_alive)
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tma-serve-conn-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_cap_admits_exactly_max_and_refuses_the_next() {
        let dir = scratch("cap");
        let held: Vec<Connection> = (0..3)
            .map(|_| {
                Connection::open(&dir, "phone", 3)
                    .ok()
                    .expect("under the cap")
            })
            .collect();
        assert!(matches!(
            Connection::open(&dir, "phone", 3),
            Err(Refused::Full { max: 3 })
        ));
        drop(held);
        // A closed connection frees its slot immediately, not on the next sweep.
        assert!(Connection::open(&dir, "phone", 3).is_ok());
    }

    /// A marker left by a process that died without unwinding must not hold a slot forever: the
    /// sweep is what makes a killed serve process cost nothing.
    #[test]
    fn a_marker_from_a_dead_process_is_swept() {
        let dir = scratch("stale");
        let slot = dir.join(REGISTRY_DIR).join("slot-0.conn");
        ensure_private_dir(&dir.join(REGISTRY_DIR)).unwrap();
        // Pid 0 is never a live process to signal, so it reads as gone.
        std::fs::write(&slot, "pid=0\ndevice=ghost\n").unwrap();
        let conn = Connection::open(&dir, "phone", 1).ok().expect("swept");
        assert_eq!(conn.marker(), slot);
        assert!(std::fs::read_to_string(&slot)
            .unwrap()
            .contains("device=phone"));
    }

    #[test]
    fn the_marker_records_the_pid_and_the_device_and_nothing_else() {
        let dir = scratch("marker");
        let conn = Connection::open(&dir, "SHA256:abc", 2)
            .ok()
            .expect("claimed");
        let text = std::fs::read_to_string(conn.marker()).unwrap();
        assert_eq!(
            text,
            format!("pid={}\ndevice=SHA256:abc\n", std::process::id())
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_registry_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let conn = Connection::open(&dir, "phone", 1).ok().expect("claimed");
        let mode = std::fs::metadata(conn.marker())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(dir.join(REGISTRY_DIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }
}
