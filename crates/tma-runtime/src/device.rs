//! The paired-device store: which remote devices exist, and what each one is allowed to ask for.
//!
//! One `devices.toml` under tma's config dir, `0600`, written whole by atomic rename. It is the
//! authorization record and the only place a scope is granted: there is no in-app path to widen
//! one, no approval prompt a device can raise, and no protocol frame that asks for a scope.
//!
//! Three rules are load-bearing and easy to lose, so they are written down here.
//!
//! - **The store is re-read per request, never cached for a connection.** A revoked device's next
//!   request has to be refused, and absence of the *record* (not merely of a scope) ends the
//!   connection: a revoked device is not a `read`-only device. [`Cache`] makes that cheap by
//!   memoizing on the file's identity tuple, so the steady state is one `stat` per request.
//! - **`read` is implicit and always present.** Every paired device may list the fleet, read
//!   transcripts and receipts, and mute a pane. The three `act:*` scopes are what a grant is about.
//! - **`act:always` is never granted by default.** `approve_always` grants every following action
//!   of its class, so it takes a deliberate host-side act (`tma device grant <name> act:always`).
//!
//! TOML rather than JSON, deliberately. tma's config dir is TOML throughout (`config.toml`,
//! `agents/*.toml`, `actions/*.toml`, the hook install record), the `toml` crate is already a
//! dependency of this crate, and this file is one a person reads when a pairing misbehaves. The
//! slot ledger next door is JSONL for the opposite reason: it is appended under a lock and a torn
//! record has to name the line it tore on, which a whole-document parse cannot do.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tma_proto::Scope;

/// The store's filename under the config dir.
pub const DEVICES_FILE: &str = "devices.toml";

/// The scopes `tma device pair` grants with no `--scope` flag: read, answering a prompt, and
/// steering. `act:always` is deliberately absent (see the module docs).
pub const DEFAULT_SCOPES: [Scope; 3] = [Scope::Read, Scope::ActAnswer, Scope::ActSteer];

/// One paired device. `id` is the opaque string the spawner passes on the serve command line; for
/// the SSH transport it is the public key's fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    /// The human label a `tma device grant`/`revoke` names.
    pub name: String,
    /// What this device may ask for. Always contains [`Scope::Read`].
    pub scopes: Vec<Scope>,
    pub paired_at_ms: u64,
    /// the pairing encryption key, distinct from the SSH signing key. Reserved: nothing writes
    /// or reads it yet, and the field exists so a v2 forwarder has a key to encrypt to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_key: Option<String>,
}

impl Device {
    /// Whether this device's grants cover `scope`.
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// The whole file. `schema` is the same additive-only integer every tma document carries.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    #[serde(default = "one")]
    schema: u32,
    /// `[[device]]`, matching the `[[agent]]` shape in `config.toml`. An array of tables rather
    /// than a table keyed by id: a fingerprint is not a bare TOML key and quoting one per record is
    /// a footgun nobody should have to remember.
    #[serde(default, rename = "device", skip_serializing_if = "Vec::is_empty")]
    devices: Vec<Device>,
}

fn one() -> u32 {
    1
}

/// Why a store operation could not answer.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("device store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("device store {path} is malformed: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot write device store {path}: {source}")]
    Encode {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("no device named {name:?} (run `tma device list`)")]
    NoSuchName { name: String },
    #[error("a device named {name:?} is already paired; revoke it first")]
    DuplicateName { name: String },
    #[error("tma has no config directory (neither XDG_CONFIG_HOME nor HOME is set)")]
    NoConfigDir,
}

/// `$XDG_CONFIG_HOME/tma`, else `~/.config/tma`. The same resolution the agent-manifest and action
/// loaders use, and deliberately not `--config`/`TMA_CONFIG`: those name one *file*, never the
/// directory the store shares with `agents/` and `actions/`.
pub fn user_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("tma"));
        }
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".config/tma"))
}

/// The device store, addressed by the directory holding it.
#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// The store inside `dir`. Nothing is created until a write.
    pub fn open(dir: &Path) -> Store {
        Store {
            path: dir.join(DEVICES_FILE),
        }
    }

    /// The one store this user's tma commands share.
    pub fn at_config_dir() -> Result<Store, DeviceError> {
        user_config_dir()
            .map(|dir| Store::open(&dir))
            .ok_or(DeviceError::NoConfigDir)
    }

    /// The file itself, for a message that has to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every paired device, in pairing order. An absent file is an empty store, not an error: a
    /// host that has never paired anything is the ordinary starting state.
    pub fn load(&self) -> Result<Vec<Device>, DeviceError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(DeviceError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        parse(&text).map_err(|source| DeviceError::Parse {
            path: self.path.clone(),
            source,
        })
    }

    /// The record for `id`, or `None` when there is none. `None` is a revocation, not a degraded
    /// device: the caller ends the connection rather than falling back to `read`.
    pub fn get(&self, id: &str) -> Result<Option<Device>, DeviceError> {
        Ok(self.load()?.into_iter().find(|d| d.id == id))
    }

    /// Record a new device. `extra` is added to [`DEFAULT_SCOPES`], or to `read` alone when `only`
    /// is set, which is the watch-only device: without it the store can only express a pairing that
    /// may already answer prompts, and "less than the default" is the whole point of a scope.
    ///
    /// A name already in the store is refused rather than silently rebound, because the name is
    /// what `grant` and `revoke` address and two records answering to one name is unanswerable.
    pub fn pair(
        &self,
        id: &str,
        name: &str,
        extra: &[Scope],
        only: bool,
        now_ms: u64,
    ) -> Result<Device, DeviceError> {
        let mut devices = self.load()?;
        if devices.iter().any(|d| d.name == name && d.id != id) {
            return Err(DeviceError::DuplicateName {
                name: name.to_string(),
            });
        }
        // `read` is implicit and cannot be dropped: every paired device may list the fleet.
        let mut scopes: Vec<Scope> = if only {
            vec![Scope::Read]
        } else {
            DEFAULT_SCOPES.to_vec()
        };
        for scope in extra {
            if !scopes.contains(scope) {
                scopes.push(*scope);
            }
        }
        let device = Device {
            id: id.to_string(),
            name: name.to_string(),
            scopes,
            paired_at_ms: now_ms,
            notify_key: None,
        };
        // Re-pairing one id replaces its record: a device that generated a fresh keypair keeps its
        // name, and its grants are re-stated rather than inherited.
        match devices.iter_mut().find(|d| d.id == id) {
            Some(existing) => *existing = device.clone(),
            None => devices.push(device.clone()),
        }
        self.write(&devices)?;
        Ok(device)
    }

    /// Add one scope to the device named `name`. Idempotent.
    pub fn grant(&self, name: &str, scope: Scope) -> Result<Device, DeviceError> {
        let mut devices = self.load()?;
        let device =
            devices
                .iter_mut()
                .find(|d| d.name == name)
                .ok_or_else(|| DeviceError::NoSuchName {
                    name: name.to_string(),
                })?;
        if !device.scopes.contains(&scope) {
            device.scopes.push(scope);
        }
        let updated = device.clone();
        self.write(&devices)?;
        Ok(updated)
    }

    /// Remove the device named `name` entirely. Removing the record, not a scope: the revocation rule
    /// is "this device is not paired", and the serve loop reads absence as a terminated connection.
    pub fn revoke(&self, name: &str) -> Result<Device, DeviceError> {
        let mut devices = self.load()?;
        let at =
            devices
                .iter()
                .position(|d| d.name == name)
                .ok_or_else(|| DeviceError::NoSuchName {
                    name: name.to_string(),
                })?;
        let removed = devices.remove(at);
        self.write(&devices)?;
        Ok(removed)
    }

    /// Replace the file: a temp sibling, `0600`, fsynced, then renamed over the old one. A reader
    /// sees either the whole previous file or the whole new one, never a half-written store.
    fn write(&self, devices: &[Device]) -> Result<(), DeviceError> {
        let doc = Document {
            schema: 1,
            devices: devices.to_vec(),
        };
        let text = toml::to_string(&doc).map_err(|source| DeviceError::Encode {
            path: self.path.clone(),
            source,
        })?;
        let io = |source| DeviceError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let temp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let result = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = opts.open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(io)
    }
}

/// Parse the document, tolerating an empty file.
fn parse(text: &str) -> Result<Vec<Device>, toml::de::Error> {
    Ok(toml::from_str::<Document>(text)?.devices)
}

/// The file-identity tuple the [`Cache`] compares, mirroring the rollout tail's memo: an unchanged
/// tuple skips the parse entirely, so a connection's steady state is one `stat` per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileMemo {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileMemo {
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> FileMemo {
        use std::os::unix::fs::MetadataExt;
        FileMemo {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// A per-process memo over the store, so revalidating on every request and every publish costs one
/// `stat` rather than a re-parse. Correctness never depends on it: a changed file always re-reads,
/// and a `stat` failure falls back to reading.
#[derive(Debug, Default)]
pub struct Cache {
    memo: Option<FileMemo>,
    devices: Vec<Device>,
    loaded: bool,
    stat_calls: u64,
    parse_calls: u64,
}

impl Cache {
    /// The current record for `id`, re-reading the store only when its identity tuple moved.
    pub fn get(&mut self, store: &Store, id: &str) -> Result<Option<Device>, DeviceError> {
        self.refresh(store)?;
        Ok(self.devices.iter().find(|d| d.id == id).cloned())
    }

    /// `fs::metadata` calls made: the seam the per-request-cost test reads.
    pub fn stat_calls(&self) -> u64 {
        self.stat_calls
    }

    /// Parses performed. Flat across a run in which nothing paired or revoked.
    pub fn parse_calls(&self) -> u64 {
        self.parse_calls
    }

    fn refresh(&mut self, store: &Store) -> Result<(), DeviceError> {
        self.stat_calls += 1;
        let memo = match std::fs::metadata(store.path()) {
            Ok(meta) => Some(FileMemo::of(&meta)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            // An unreadable store is not an empty one: report it rather than reading a permission
            // failure as "nobody is paired", which would refuse every device instead of erroring.
            Err(source) => {
                return Err(DeviceError::Io {
                    path: store.path().to_path_buf(),
                    source,
                })
            }
        };
        if self.loaded && self.memo == memo {
            return Ok(());
        }
        self.devices = store.load()?;
        self.memo = memo;
        self.loaded = true;
        self.parse_calls += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tma-devices-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_absent_store_is_empty_rather_than_an_error() {
        let store = Store::open(&scratch("absent").join("nothing"));
        assert_eq!(store.load().unwrap(), Vec::new());
        assert_eq!(store.get("SHA256:whatever").unwrap(), None);
    }

    /// Pairing grants read, answer and steer, and never `act:always`: that one is the second-factor
    /// class, and a default grant of it would make the whole scope model decorative.
    #[test]
    fn pairing_grants_the_three_defaults_and_never_act_always() {
        let store = Store::open(&scratch("defaults"));
        let device = store
            .pair("SHA256:aaa", "phone", &[], false, 1_730_000_000_000)
            .unwrap();
        assert_eq!(
            device.scopes,
            vec![Scope::Read, Scope::ActAnswer, Scope::ActSteer]
        );
        assert!(!device.allows(Scope::ActAlways));
        assert_eq!(store.get("SHA256:aaa").unwrap().as_ref(), Some(&device));
    }

    /// The watch-only pairing: `read` and nothing else, plus whatever was named explicitly. A
    /// device that can only look is the scope model's simplest claim and has to be expressible.
    #[test]
    fn only_pairs_a_device_that_can_look_and_not_answer() {
        let store = Store::open(&scratch("only"));
        let watcher = store.pair("SHA256:w", "watcher", &[], true, 1).unwrap();
        assert_eq!(watcher.scopes, vec![Scope::Read]);
        assert!(!watcher.allows(Scope::ActAnswer));

        let steerer = store
            .pair("SHA256:s", "steerer", &[Scope::ActSteer], true, 2)
            .unwrap();
        assert_eq!(steerer.scopes, vec![Scope::Read, Scope::ActSteer]);
        assert!(!steerer.allows(Scope::ActAnswer));
    }

    #[test]
    fn a_grant_is_additive_and_idempotent() {
        let store = Store::open(&scratch("grant"));
        store.pair("SHA256:bbb", "tablet", &[], false, 1).unwrap();
        let once = store.grant("tablet", Scope::ActAlways).unwrap();
        let twice = store.grant("tablet", Scope::ActAlways).unwrap();
        assert_eq!(once, twice);
        assert!(twice.allows(Scope::ActAlways));
        assert_eq!(
            twice.scopes.len(),
            4,
            "no duplicate token: {:?}",
            twice.scopes
        );
    }

    /// Revocation removes the record, not a scope. The serve loop reads absence as "end the
    /// connection", so a revoked device that degraded to `read` would be exactly the hole the scope design names.
    #[test]
    fn revocation_removes_the_record() {
        let store = Store::open(&scratch("revoke"));
        store.pair("SHA256:ccc", "phone", &[], false, 1).unwrap();
        store.pair("SHA256:ddd", "tablet", &[], false, 2).unwrap();
        store.revoke("phone").unwrap();
        assert_eq!(store.get("SHA256:ccc").unwrap(), None);
        assert!(store.get("SHA256:ddd").unwrap().is_some());
        assert!(matches!(
            store.revoke("phone"),
            Err(DeviceError::NoSuchName { .. })
        ));
    }

    #[test]
    fn two_devices_cannot_share_a_name() {
        let store = Store::open(&scratch("dupe"));
        store.pair("SHA256:eee", "phone", &[], false, 1).unwrap();
        assert!(matches!(
            store.pair("SHA256:fff", "phone", &[], false, 2),
            Err(DeviceError::DuplicateName { .. })
        ));
        // Re-pairing the SAME id keeps one record: a device with a fresh keypair is a new id, and a
        // device re-enrolling under its own id is a restatement.
        store
            .pair("SHA256:eee", "phone", &[Scope::ActAlways], false, 3)
            .unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
    }

    /// The file is private and survives a round trip with a fingerprint that is not a bare TOML key.
    #[cfg(unix)]
    #[test]
    fn the_store_is_private_and_round_trips_an_opaque_id() {
        use std::os::unix::fs::PermissionsExt;
        let store = Store::open(&scratch("mode"));
        let id = "SHA256:a.b/c+d=";
        store.pair(id, "phone", &[], false, 7).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "nobody else may read a scope record");
        assert_eq!(store.get(id).unwrap().unwrap().paired_at_ms, 7);
    }

    /// The memo's whole job: a hundred revalidations against an unchanged store parse once, and a
    /// write is seen on the next call rather than on the next process.
    #[test]
    fn the_cache_stats_every_time_and_parses_only_on_a_change() {
        let store = Store::open(&scratch("memo"));
        store.pair("SHA256:ggg", "phone", &[], false, 1).unwrap();
        let mut cache = Cache::default();
        for _ in 0..100 {
            assert!(cache.get(&store, "SHA256:ggg").unwrap().is_some());
        }
        assert_eq!(cache.stat_calls(), 100);
        assert_eq!(
            cache.parse_calls(),
            1,
            "an unchanged store is not re-parsed"
        );

        store.revoke("phone").unwrap();
        assert_eq!(cache.get(&store, "SHA256:ggg").unwrap(), None);
        assert_eq!(cache.parse_calls(), 2, "a changed store re-reads");
    }

    /// A malformed store is a loud error, never an empty one: reading a typo as "nobody is paired"
    /// would silently refuse every device, which looks exactly like a revocation nobody performed.
    #[test]
    fn a_malformed_store_names_itself() {
        let dir = scratch("malformed");
        std::fs::write(dir.join(DEVICES_FILE), "[[device]]\nname = 3\n").unwrap();
        let store = Store::open(&dir);
        assert!(matches!(store.load(), Err(DeviceError::Parse { .. })));
    }
}
