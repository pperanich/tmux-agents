//! The per-host slot ledger: one receipt per dispatch, so a caller that lost the response can
//! retry without ever re-firing a keystroke.
//!
//! One file under [`crate::ipc::runtime_dir`], `0600`, keyed by a caller-supplied opaque slot id
//! and shared by every process on the host. A dispatch claims its slot BEFORE it fires and writes
//! the receipt after, so a replay of the same slot returns the cached receipt and sends nothing.
//! `locked` is the one refusal that releases the claim, because it is the one that is retryable by
//! construction: consume-before-fire is only unrecoverable when the consume is unconditional.
//!
//! Four rules are easy to "tidy" into bugs, so they are written down here with their reasons.
//!
//! - **The TTL floor is 24 h** ([`TTL_MS`]). A long TTL is safe: a replay returns a cached receipt
//!   and dispatches nothing. A short one is the hazard, because it turns a retry after a pocket
//!   disconnect back into a genuine re-fire.
//! - **Eviction is by size** ([`MAX_ENTRIES`]), never by age below the TTL. Age-based eviction is
//!   the short-TTL hazard wearing a different hat.
//! - **A torn record refuses** ([`LedgerError::Torn`]) and is never read as absent. An unparsable
//!   entry answers "did this fire?" with "unknown", and guessing "no" there is the double-fire the
//!   module exists to prevent.
//! - **Every write is atomic under an exclusive flock**: a temp file in the same directory, fsync,
//!   rename. The flock is on a SIBLING lock file because the rename replaces the ledger's inode,
//!   and a lock held on a replaced inode guards nothing.
//!
//! The file is JSONL rather than one JSON array: a record is a line, so a truncated tail names the
//! line it stopped on instead of failing the whole document, and a reader can tell a torn record
//! from a short-but-valid ledger.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::FlockOperation;
use rustix::io::Errno;

use crate::broker::{ActResult, Outcome};
use crate::json::JsonWriter;

/// The ledger's filename under [`crate::ipc::runtime_dir`].
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// The sibling lock file every read-modify-write flocks.
pub const LOCK_FILE: &str = "ledger.lock";

/// How long a receipt answers for its slot: 24 h, the floor the slot-ledger design fixes. Raising this is
/// safe (a replay dispatches nothing); lowering it re-fires keystrokes after a long disconnect.
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The size cap that is the only thing which evicts: 4096 entries, roughly 1 MB of JSONL. An entry
/// inside the TTL is dropped only because something newer needed the room, never because it aged.
pub const MAX_ENTRIES: usize = 4096;

/// How long a claim may stay unresolved before a second caller stops waiting for its receipt. Well
/// past a synchronous fire's own bound (an action's `timeout_ms` plus the broker's lock slack).
pub const PENDING_MS: u64 = 120_000;

/// The `reason` a receipt carries when the dispatch's effect is genuinely unknown: the broker
/// reported `error`, or the claimer died before writing. The keystroke may have landed, so a retry
/// gets this back rather than a second fire.
pub const FIRED_UNKNOWN: &str = "fired-unknown";

/// How often a caller waiting on another process's in-flight claim re-reads the ledger.
const POLL: Duration = Duration::from_millis(20);

/// What a dispatch ended as, in the same closed vocabulary `tma act --json` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The `outcome` token (`sent`, `refused`, `error`, ...).
    pub outcome: String,
    /// The `reason` token, or [`FIRED_UNKNOWN`] for an indeterminate dispatch.
    pub reason: Option<String>,
    /// The exit code the original dispatch returned, replayed verbatim.
    pub exit_code: i32,
}

impl Receipt {
    /// The receipt for a finished fire. `error` records [`FIRED_UNKNOWN`] because the broker failed
    /// somewhere it cannot prove the keystroke did not land, and a retry must not re-send it.
    pub fn from_act_result(result: &ActResult) -> Receipt {
        Receipt {
            outcome: result.outcome.token().to_string(),
            reason: match result.outcome {
                Outcome::Error(_) => Some(FIRED_UNKNOWN.to_string()),
                _ => result.reason().map(str::to_string),
            },
            exit_code: result.exit_code(),
        }
    }

    /// The receipt for a dispatch nobody can account for: an abandoned claim.
    fn unknown() -> Receipt {
        Receipt {
            outcome: Outcome::Error(String::new()).token().to_string(),
            reason: Some(FIRED_UNKNOWN.to_string()),
            exit_code: 1,
        }
    }
}

/// One ledger record. `device` is deliberately NOT part of the key: idempotency across devices is
/// the point, so device B's claim inside one slot returns device A's receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The caller-supplied key.
    pub slot: String,
    pub pane: String,
    pub action: String,
    /// Which device dispatched, for "who approved this"; never part of the key.
    pub device: Option<String>,
    /// When the slot was claimed (epoch ms). The TTL and `--since-ms` both read this.
    pub at_ms: u64,
    /// The claiming process, so a second caller can tell an in-flight claim from an abandoned one.
    pub pid: i32,
    /// `None` while the dispatch is in flight.
    pub receipt: Option<Receipt>,
}

/// What a [`Ledger::claim`] found.
pub enum Claim {
    /// The slot already has a receipt: return it and dispatch nothing.
    Hit(Receipt),
    /// The slot is ours. Resolve it with [`SlotGuard::write`] or [`SlotGuard::release`].
    Claimed(SlotGuard),
}

/// Why a ledger operation could not answer.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// A record that does not parse. Refusing is the point: an unparsable entry means "unknown",
    /// and reading it as absent is what re-fires a keystroke.
    #[error(
        "dispatch ledger {path} is torn at line {line}; it cannot say whether that slot fired. \
         Remove the file to start a fresh ledger."
    )]
    Torn { path: PathBuf, line: usize },
    #[error("dispatch ledger {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Which receipts [`Ledger::receipts`] returns. Both filters are optional and combine.
#[derive(Clone, Copy, Default)]
pub struct ReceiptFilter<'a> {
    pub slot: Option<&'a str>,
    /// Claimed at or after this epoch-ms instant.
    pub since_ms: Option<u64>,
}

/// The host's ledger, addressed by the directory holding it.
#[derive(Clone, Debug)]
pub struct Ledger {
    dir: PathBuf,
    path: PathBuf,
    lock: PathBuf,
}

impl Ledger {
    /// Open (and create, `0700`) the ledger directory `dir`.
    pub fn open(dir: &Path) -> Result<Ledger, LedgerError> {
        ensure_private_dir(dir).map_err(|source| LedgerError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        Ok(Ledger {
            dir: dir.to_path_buf(),
            path: dir.join(LEDGER_FILE),
            lock: dir.join(LOCK_FILE),
        })
    }

    /// The one ledger every process on this host shares.
    pub fn at_runtime_dir() -> Result<Ledger, LedgerError> {
        Ledger::open(&crate::ipc::runtime_dir())
    }

    /// The ledger file itself, for a message that has to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Claim `slot` for a dispatch that has not happened yet, or return the receipt of the one that
    /// already did. A slot another live process is mid-dispatch on waits for that process's
    /// receipt rather than firing a second time; a claim whose owner is gone or out of
    /// [`PENDING_MS`] resolves to [`FIRED_UNKNOWN`], since nobody can say whether it landed.
    pub fn claim(
        &self,
        slot: &str,
        pane: &str,
        action: &str,
        device: Option<&str>,
        now_ms: u64,
    ) -> Result<Claim, LedgerError> {
        // `now_ms` is the caller's clock and dates the entry; the in-flight wait is measured
        // monotonically, so a fixed injected clock cannot turn the poll into a spin.
        let mut wait_until: Option<Instant> = None;
        loop {
            let lock = self.lock(FlockOperation::LockExclusive)?;
            let mut entries = self.read()?;
            if let Some(i) = entries.iter().position(|e| e.slot == slot) {
                match entries[i].receipt.clone() {
                    Some(receipt) if !is_expired(entries[i].at_ms, now_ms) => {
                        return Ok(Claim::Hit(receipt))
                    }
                    Some(_) => {}
                    None => {
                        let spent = now_ms.saturating_sub(entries[i].at_ms);
                        let deadline = *wait_until.get_or_insert_with(|| {
                            Instant::now() + Duration::from_millis(PENDING_MS.saturating_sub(spent))
                        });
                        if crate::ipc::pid_is_live(entries[i].pid) && Instant::now() < deadline {
                            drop(lock);
                            std::thread::sleep(POLL);
                            continue;
                        }
                        let receipt = Receipt::unknown();
                        entries[i].receipt = Some(receipt.clone());
                        self.write(&mut entries, now_ms)?;
                        return Ok(Claim::Hit(receipt));
                    }
                }
            }
            entries.retain(|e| e.slot != slot);
            entries.push(Entry {
                slot: slot.to_string(),
                pane: pane.to_string(),
                action: action.to_string(),
                device: device.map(str::to_string),
                at_ms: now_ms,
                pid: std::process::id() as i32,
                receipt: None,
            });
            self.write(&mut entries, now_ms)?;
            return Ok(Claim::Claimed(SlotGuard {
                ledger: self.clone(),
                slot: slot.to_string(),
                claimed_at: now_ms,
                resolved: false,
            }));
        }
    }

    /// Read receipts without claiming anything: how a caller learns an outcome it lost the response
    /// to. In-flight claims carry no receipt and are not listed. Entries past the TTL are listed
    /// until the size cap evicts them, so an old receipt here can still name a slot a fresh claim
    /// would dispatch again; `at_ms` is what says which.
    pub fn receipts(&self, filter: &ReceiptFilter) -> Result<Vec<Entry>, LedgerError> {
        let _lock = self.lock(FlockOperation::LockShared)?;
        let mut entries = self.read()?;
        entries.retain(|e| {
            e.receipt.is_some()
                && filter.slot.is_none_or(|s| e.slot == s)
                && filter.since_ms.is_none_or(|since| e.at_ms >= since)
        });
        entries.sort_by_key(|e| e.at_ms);
        Ok(entries)
    }

    /// Resolve our claim on `slot`: `Some(receipt)` writes it terminally, `None` drops the entry so
    /// the slot is claimable again. Ownership is `(pid, claimed_at)`, so a claim that was evicted or
    /// taken over while we were dispatching is left alone rather than overwritten or resurrected.
    fn resolve(
        &self,
        slot: &str,
        claimed_at: u64,
        receipt: Option<Receipt>,
    ) -> Result<(), LedgerError> {
        let _lock = self.lock(FlockOperation::LockExclusive)?;
        let mut entries = self.read()?;
        let pid = std::process::id() as i32;
        let ours = |e: &Entry| e.slot == slot && e.pid == pid && e.at_ms == claimed_at;
        match receipt {
            Some(receipt) => match entries.iter_mut().find(|e| ours(e)) {
                Some(entry) => entry.receipt = Some(receipt),
                None => return Ok(()),
            },
            None => {
                if !entries.iter().any(&ours) {
                    return Ok(());
                }
                entries.retain(|e| !ours(e));
            }
        }
        self.write(&mut entries, claimed_at)
    }

    /// Every record in the file, or [`LedgerError::Torn`] naming the first line that is not one.
    fn read(&self) -> Result<Vec<Entry>, LedgerError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io(source)),
        };
        let mut entries = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_entry(line) {
                Some(entry) => entries.push(entry),
                None => {
                    return Err(LedgerError::Torn {
                        path: self.path.clone(),
                        line: n + 1,
                    })
                }
            }
        }
        Ok(entries)
    }

    /// Replace the ledger atomically: evict down to the cap, write a temp file beside it, fsync,
    /// rename. Callers hold the exclusive flock.
    fn write(&self, entries: &mut Vec<Entry>, now_ms: u64) -> Result<(), LedgerError> {
        evict(entries, now_ms);
        entries.sort_by_key(|e| e.at_ms);
        let mut body = String::new();
        for entry in entries.iter() {
            body.push_str(&render_entry(entry));
            body.push('\n');
        }
        // One temp name per process is enough: the flock serializes every writer, this one included.
        let tmp = self
            .dir
            .join(format!("{LEDGER_FILE}.{}.tmp", std::process::id()));
        let mut file = private_file(&tmp, true).map_err(|e| self.io(e))?;
        file.write_all(body.as_bytes()).map_err(|e| self.io(e))?;
        rustix::fs::fsync(&file).map_err(|e| self.io(errno_io(e)))?;
        drop(file);
        std::fs::rename(&tmp, &self.path).map_err(|e| self.io(e))?;
        // The rename itself is durable only once the directory entry is on disk.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = rustix::fs::fsync(&dir);
        }
        Ok(())
    }

    /// Take the ledger's flock, retrying the signal interruption. Blocking: the critical section is
    /// a read, a write and a rename, and the kernel releases the lock if a holder dies.
    fn lock(&self, op: FlockOperation) -> Result<LockGuard, LedgerError> {
        let file = private_file(&self.lock, false).map_err(|e| LedgerError::Io {
            path: self.lock.clone(),
            source: e,
        })?;
        loop {
            match rustix::fs::flock(&file, op) {
                Ok(()) => return Ok(LockGuard { file }),
                Err(Errno::INTR) => continue,
                Err(e) => {
                    return Err(LedgerError::Io {
                        path: self.lock.clone(),
                        source: errno_io(e),
                    })
                }
            }
        }
    }

    fn io(&self, source: std::io::Error) -> LedgerError {
        LedgerError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

/// A claimed slot, resolved exactly once. Dropping it unresolved releases the claim, so an
/// abandoned code path leaves the slot claimable rather than pending.
pub struct SlotGuard {
    ledger: Ledger,
    slot: String,
    /// When this claim was made: half of the `(pid, claimed_at)` ownership check.
    claimed_at: u64,
    resolved: bool,
}

impl SlotGuard {
    /// Record the dispatch's terminal outcome. Every outcome but `locked` ends here, `error`
    /// included: see [`Receipt::from_act_result`].
    pub fn write(mut self, receipt: Receipt) -> Result<(), LedgerError> {
        // Marked resolved before the write, so a failed write leaves the claim pending (which
        // resolves to `fired-unknown`) rather than releasing a slot that may have fired.
        self.resolved = true;
        self.ledger
            .resolve(&self.slot, self.claimed_at, Some(receipt))
    }

    /// Give the slot back, for the one refusal that changed nothing and will pass on a retry.
    pub fn release(mut self) -> Result<(), LedgerError> {
        self.resolved = true;
        self.ledger.resolve(&self.slot, self.claimed_at, None)
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if !self.resolved {
            let _ = self.ledger.resolve(&self.slot, self.claimed_at, None);
        }
    }
}

/// A held flock, released when the file closes. The explicit unlock keeps the release visible.
struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
    }
}

/// Whether a claim made at `at_ms` has aged past the TTL. A clock that stepped backwards reads as
/// not expired, which keeps the cached receipt rather than re-firing.
fn is_expired(at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(at_ms) >= TTL_MS
}

/// Drop entries only when the ledger is over [`MAX_ENTRIES`]: expired receipts first, then the
/// oldest. An in-flight claim is never evicted, because losing one is a licence to re-fire.
fn evict(entries: &mut Vec<Entry>, now_ms: u64) {
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    entries.retain(|e| e.receipt.is_none() || !is_expired(e.at_ms, now_ms));
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    let (pending, mut done): (Vec<Entry>, Vec<Entry>) =
        entries.drain(..).partition(|e| e.receipt.is_none());
    done.sort_by_key(|e| e.at_ms);
    let room = MAX_ENTRIES.saturating_sub(pending.len());
    if done.len() > room {
        done.drain(..done.len() - room);
    }
    *entries = pending;
    entries.append(&mut done);
}

/// Create `dir` (and parents) `0700`, the same privacy the daemon gives its socket directory.
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

/// Open `path` for writing, creating it `0600`. Without the explicit mode the file lands at
/// `0666 & ~umask`, which is world-writable under `umask 000`.
fn private_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
}

fn errno_io(e: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.raw_os_error())
}

/// One record as its JSONL line, through the writer every other `--json` surface uses.
fn render_entry(entry: &Entry) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.string("slot", &entry.slot);
    j.string("pane", &entry.pane);
    j.string("action", &entry.action);
    match &entry.device {
        Some(device) => j.string("device", device),
        None => j.null("device"),
    }
    j.number("at_ms", entry.at_ms as i64);
    j.number("pid", entry.pid as i64);
    match &entry.receipt {
        Some(receipt) => {
            j.string("outcome", &receipt.outcome);
            match &receipt.reason {
                Some(reason) => j.string("reason", reason),
                None => j.null("reason"),
            }
            j.number("exit_code", receipt.exit_code as i64);
        }
        None => {
            j.null("outcome");
            j.null("reason");
            j.null("exit_code");
        }
    }
    j.end_object();
    j.finish()
}

/// Parse one record. `None` is a torn or unparsable line, never a missing entry. Unknown keys are
/// skipped so a later field is additive; the values are flat by construction, so a nested one is
/// as much a torn record as a truncated string.
fn parse_entry(line: &str) -> Option<Entry> {
    let mut p = Parser::new(line);
    p.spaces();
    if !p.eat(b'{') {
        return None;
    }
    let (mut slot, mut pane, mut action, mut device) = (None, None, None, None);
    let (mut at_ms, mut pid) = (None, None);
    let (mut outcome, mut reason, mut exit_code) = (None, None, None);
    p.spaces();
    if !p.eat(b'}') {
        loop {
            p.spaces();
            let key = p.string()?;
            p.spaces();
            if !p.eat(b':') {
                return None;
            }
            p.spaces();
            let value = p.value()?;
            match key.as_str() {
                "slot" => slot = Some(value.into_string()?),
                "pane" => pane = Some(value.into_string()?),
                "action" => action = Some(value.into_string()?),
                "device" => device = value.into_opt_string()?,
                "at_ms" => at_ms = Some(u64::try_from(value.into_number()?).ok()?),
                "pid" => pid = Some(i32::try_from(value.into_number()?).ok()?),
                "outcome" => outcome = value.into_opt_string()?,
                "reason" => reason = value.into_opt_string()?,
                "exit_code" => exit_code = value.into_opt_number()?,
                _ => {}
            }
            p.spaces();
            if p.eat(b',') {
                continue;
            }
            if p.eat(b'}') {
                break;
            }
            return None;
        }
    }
    p.spaces();
    if !p.done() {
        return None;
    }
    // A half-written receipt is as unreadable as a half-written line.
    let receipt = match (outcome, exit_code) {
        (Some(outcome), Some(exit_code)) => Some(Receipt {
            outcome,
            reason,
            exit_code: i32::try_from(exit_code).ok()?,
        }),
        (None, None) => None,
        _ => return None,
    };
    Some(Entry {
        slot: slot?,
        pane: pane?,
        action: action?,
        device,
        at_ms: at_ms?,
        pid: pid?,
        receipt,
    })
}

/// A JSON scalar, which is every value this record shape holds.
enum Value {
    Str(String),
    Num(i64),
    Null,
}

impl Value {
    fn into_string(self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    fn into_opt_string(self) -> Option<Option<String>> {
        match self {
            Value::Str(s) => Some(Some(s)),
            Value::Null => Some(None),
            Value::Num(_) => None,
        }
    }
    fn into_number(self) -> Option<i64> {
        match self {
            Value::Num(n) => Some(n),
            _ => None,
        }
    }
    fn into_opt_number(self) -> Option<Option<i64>> {
        match self {
            Value::Num(n) => Some(Some(n)),
            Value::Null => Some(None),
            Value::Str(_) => None,
        }
    }
}

/// A byte cursor over one line. Small on purpose: the workspace has no JSON parser, and this record
/// shape is flat, fixed, and written by [`render_entry`] alone.
struct Parser<'a> {
    src: &'a [u8],
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(line: &'a str) -> Parser<'a> {
        Parser {
            src: line.as_bytes(),
            at: 0,
        }
    }

    fn spaces(&mut self) {
        while matches!(self.src.get(self.at), Some(b' ' | b'\t')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.src.get(self.at) == Some(&byte) {
            self.at += 1;
            return true;
        }
        false
    }

    fn done(&self) -> bool {
        self.at == self.src.len()
    }

    /// Bytes in, UTF-8 checked when the string closes, so a multi-byte character crosses the loop
    /// as its own continuation bytes rather than being reassembled a byte at a time.
    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = *self.src.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let esc = *self.src.get(self.at)?;
                    self.at += 1;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => {
                            let hex = self.src.get(self.at..self.at + 4)?;
                            self.at += 4;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(
                                char::from_u32(code)?.encode_utf8(&mut buf).as_bytes(),
                            );
                        }
                        _ => return None,
                    }
                }
                _ => out.push(byte),
            }
        }
    }

    fn value(&mut self) -> Option<Value> {
        match self.src.get(self.at)? {
            b'"' => self.string().map(Value::Str),
            b'n' => {
                if self.src.get(self.at..self.at + 4) != Some(b"null".as_slice()) {
                    return None;
                }
                self.at += 4;
                Some(Value::Null)
            }
            _ => {
                let start = self.at;
                self.eat(b'-');
                while matches!(self.src.get(self.at), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
                if self.at == start {
                    return None;
                }
                std::str::from_utf8(&self.src[start..self.at])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Value::Num)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A private ledger directory that removes itself.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("tma_slots_{tag}_{}_{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
        fn ledger(&self) -> Ledger {
            Ledger::open(&self.0).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(slot: &str, at_ms: u64, receipt: Option<Receipt>) -> Entry {
        Entry {
            slot: slot.to_string(),
            pane: "%1".to_string(),
            action: "approve".to_string(),
            device: None,
            at_ms,
            pid: 1,
            receipt,
        }
    }

    fn sent() -> Receipt {
        Receipt {
            outcome: "sent".to_string(),
            reason: None,
            exit_code: 0,
        }
    }

    #[test]
    fn a_record_survives_the_round_trip_with_its_awkward_characters() {
        let original = Entry {
            slot: "s\"1\n\u{1}\u{e9}".to_string(),
            pane: "%5".to_string(),
            action: "approve".to_string(),
            device: Some("phone\\a".to_string()),
            at_ms: 1_730_000_000_000,
            pid: 4242,
            receipt: Some(Receipt {
                outcome: "refused".to_string(),
                reason: Some("gated".to_string()),
                exit_code: 4,
            }),
        };
        let line = render_entry(&original);
        assert!(!line.contains('\n'), "a record must stay one line: {line}");
        assert_eq!(parse_entry(&line), Some(original));
    }

    #[test]
    fn a_pending_record_round_trips_with_no_receipt() {
        let pending = entry("s1", 7, None);
        assert_eq!(parse_entry(&render_entry(&pending)), Some(pending));
    }

    #[test]
    fn an_unknown_key_is_additive_but_a_broken_record_is_not() {
        let line = render_entry(&entry("s1", 7, Some(sent())));
        let widened = line.replace("{\"slot\"", "{\"future_key\":7,\"slot\"");
        assert_eq!(
            parse_entry(&widened).map(|e| e.slot),
            Some("s1".to_string()),
            "a key this build does not know must not be a torn record"
        );
        for broken in [
            &line[..line.len() - 1],
            &line[..line.len() / 2],
            "{\"slot\":\"s1\"}",
            "",
        ] {
            assert!(parse_entry(broken).is_none(), "parsed {broken:?}");
        }
        // A half-written receipt is torn, not pending.
        assert!(parse_entry(&line.replace(",\"exit_code\":0", "")).is_none());
    }

    #[test]
    fn a_torn_line_refuses_and_names_itself() {
        let dir = Dir::new("torn");
        let ledger = dir.ledger();
        std::fs::write(
            ledger.path(),
            format!(
                "{}\n{{\"slot\":\"s2\",\"pa\n",
                render_entry(&entry("s1", 1, Some(sent())))
            ),
        )
        .unwrap();
        match ledger.claim("s2", "%1", "approve", None, 10) {
            Err(LedgerError::Torn { line, .. }) => assert_eq!(line, 2),
            other => panic!(
                "a torn ledger must refuse, got {:?}",
                other.map(|_| "claim")
            ),
        }
    }

    #[test]
    fn a_receipt_answers_until_the_ttl_and_not_after() {
        let dir = Dir::new("ttl");
        let ledger = dir.ledger();
        let at = 1_000_000_000_000;
        match ledger.claim("s1", "%1", "approve", None, at).unwrap() {
            Claim::Claimed(guard) => guard.write(sent()).unwrap(),
            Claim::Hit(_) => panic!("the first claim on an empty ledger cannot hit"),
        }
        for (age, cached) in [(0, true), (TTL_MS - 1, true), (TTL_MS, false)] {
            let claim = ledger.claim("s1", "%1", "approve", None, at + age).unwrap();
            assert_eq!(
                matches!(claim, Claim::Hit(_)),
                cached,
                "an entry {age} ms old"
            );
        }
    }

    #[test]
    fn a_released_slot_is_claimable_and_a_written_one_is_not() {
        let dir = Dir::new("release");
        let ledger = dir.ledger();
        let Claim::Claimed(guard) = ledger.claim("s1", "%1", "approve", None, 1).unwrap() else {
            panic!("first claim");
        };
        guard.release().unwrap();
        assert!(
            ledger
                .receipts(&ReceiptFilter::default())
                .unwrap()
                .is_empty(),
            "a released slot leaves no receipt behind"
        );
        let Claim::Claimed(guard) = ledger.claim("s1", "%1", "approve", None, 2).unwrap() else {
            panic!("a released slot must be claimable again");
        };
        guard.write(sent()).unwrap();
        assert!(matches!(
            ledger.claim("s1", "%1", "approve", None, 3).unwrap(),
            Claim::Hit(_)
        ));
    }

    #[test]
    fn the_device_rides_along_without_joining_the_key() {
        let dir = Dir::new("device");
        let ledger = dir.ledger();
        let Claim::Claimed(guard) = ledger
            .claim("s1", "%1", "approve", Some("phone-a"), 1)
            .unwrap()
        else {
            panic!("first claim");
        };
        guard.write(sent()).unwrap();
        assert!(
            matches!(
                ledger
                    .claim("s1", "%1", "approve", Some("phone-b"), 2)
                    .unwrap(),
                Claim::Hit(_)
            ),
            "another device's claim on the same slot must hit the same receipt"
        );
        let stored = ledger.receipts(&ReceiptFilter::default()).unwrap();
        assert_eq!(stored[0].device.as_deref(), Some("phone-a"));
    }

    #[test]
    fn an_error_receipt_records_that_the_keystroke_may_have_landed() {
        let result = ActResult {
            action: "approve".to_string(),
            pane: "%1".to_string(),
            outcome: Outcome::Error("tmux said no".to_string()),
        };
        let receipt = Receipt::from_act_result(&result);
        assert_eq!(receipt.outcome, "error");
        assert_eq!(receipt.reason.as_deref(), Some(FIRED_UNKNOWN));
        assert_eq!(receipt.exit_code, 1);
    }

    #[test]
    fn only_the_size_cap_evicts_and_it_spares_the_in_flight() {
        let now = TTL_MS * 2;
        // Under the cap nothing is dropped, however old it is.
        let mut small = vec![
            entry("old", 0, Some(sent())),
            entry("new", now, Some(sent())),
        ];
        evict(&mut small, now);
        assert_eq!(small.len(), 2, "age alone never evicts");

        let mut full: Vec<Entry> = (0..MAX_ENTRIES + 10)
            .map(|i| entry(&format!("s{i}"), now - i as u64, Some(sent())))
            .collect();
        full.push(entry("expired", 0, Some(sent())));
        full.push(entry("in-flight", 0, None));
        evict(&mut full, now);
        assert_eq!(full.len(), MAX_ENTRIES, "the cap holds");
        assert!(
            full.iter().any(|e| e.slot == "in-flight"),
            "an in-flight claim is never evicted"
        );
        assert!(
            !full.iter().any(|e| e.slot == "expired"),
            "an expired receipt goes first when the cap is reached"
        );
        assert!(
            full.iter().any(|e| e.slot == "s0"),
            "the newest receipt survives"
        );
    }

    /// A reader looping over the file while writes land never sees a partial record. The
    /// reader takes no lock, so this is the rename's atomicity and nothing else.
    #[test]
    fn a_reader_never_observes_a_partial_record() {
        let dir = Dir::new("atomic");
        let ledger = dir.ledger();
        let path = ledger.path().to_path_buf();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut seen = 0u32;
            while !reader_stop.load(Ordering::Relaxed) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    assert!(parse_entry(line).is_some(), "partial record read: {line:?}");
                    seen += 1;
                }
            }
            seen
        });
        for i in 0..100u64 {
            let slot = format!("s{i}");
            match ledger
                .claim(&slot, "%1", "approve", None, 1000 + i)
                .unwrap()
            {
                Claim::Claimed(guard) => guard.write(sent()).unwrap(),
                Claim::Hit(_) => panic!("{slot} cannot already be claimed"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0, "the reader never read anything");
        assert_eq!(
            ledger.receipts(&ReceiptFilter::default()).unwrap().len(),
            100
        );
    }

    #[test]
    fn receipts_filter_by_slot_and_by_time() {
        let dir = Dir::new("filter");
        let ledger = dir.ledger();
        for (slot, at) in [("a", 10u64), ("b", 20), ("c", 30)] {
            let Claim::Claimed(guard) = ledger.claim(slot, "%1", "approve", None, at).unwrap()
            else {
                panic!("claim {slot}");
            };
            guard.write(sent()).unwrap();
        }
        let all = ledger.receipts(&ReceiptFilter::default()).unwrap();
        assert_eq!(
            all.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
            [10, 20, 30]
        );
        let one = ledger
            .receipts(&ReceiptFilter {
                slot: Some("b"),
                since_ms: None,
            })
            .unwrap();
        assert_eq!(one.len(), 1);
        let recent = ledger
            .receipts(&ReceiptFilter {
                slot: None,
                since_ms: Some(20),
            })
            .unwrap();
        assert_eq!(recent.len(), 2, "since_ms is inclusive");
        // An in-flight claim has no receipt to report.
        let _guard = ledger.claim("d", "%1", "approve", None, 40).unwrap();
        assert_eq!(ledger.receipts(&ReceiptFilter::default()).unwrap().len(), 3);
    }

    #[test]
    fn an_abandoned_claim_resolves_to_fired_unknown_rather_than_re_firing() {
        let dir = Dir::new("abandoned");
        let ledger = dir.ledger();
        // A claim from a pid that cannot be running: pid 0 is never a live process id here.
        let mut entries = vec![Entry {
            slot: "s1".to_string(),
            pane: "%1".to_string(),
            action: "approve".to_string(),
            device: None,
            at_ms: 1,
            pid: 0,
            receipt: None,
        }];
        {
            let _lock = ledger.lock(FlockOperation::LockExclusive).unwrap();
            ledger.write(&mut entries, 1).unwrap();
        }
        match ledger.claim("s1", "%1", "approve", None, 2).unwrap() {
            Claim::Hit(receipt) => {
                assert_eq!(receipt.reason.as_deref(), Some(FIRED_UNKNOWN));
                assert_eq!(receipt.exit_code, 1);
            }
            Claim::Claimed(_) => panic!("an abandoned claim must never be re-fired"),
        }
    }
}
