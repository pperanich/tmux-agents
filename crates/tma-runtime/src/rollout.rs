//! Codex rollout tail (the context pull path): the bounded, end-anchored read of a Codex session's rollout
//! JSONL, keyed to a pane by the hook-registered `@agent_session`. The pure parse
//! ([`tma_core::parse_context`] for `codex-rollout-jsonl`) lives in the core; this module owns the
//! I/O edge — file discovery, the bounded backward-scan read, the process-local memo, and the guarded
//! stamp — pure parsers stay in `tma-core`, the tail lives in this `tma-runtime` edge.
//!
//! There is **no persisted offset**: a stored byte offset would be meaningless across the
//! per-session dated rollout files' rotation, and end-anchored reads need no state. The only state is
//! an in-memory [`RolloutTail`] memo of `(file identity, size, mtime, last result)` so a quiet pane's
//! steady state is one stat call, not a repeating 1 MiB scan of a rollout with no `token_count`.
//!
//! Discovery maps a pane to its rollout file via the session id in the filename under the dated
//! `<CODEX_HOME>/sessions/YYYY/MM/DD/` layout. Its stability across Codex versions is the
//! fixture-treatment caveat tracked in ACTIONS.md open question 6; the path is fail-safe (a wrong or
//! missing file simply stamps nothing, leaving the gauge absent/stale — never a wrong gauge).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tma_core::stamp::opt;
use tma_core::{codex_rollout_model, parse_context, Channel};

use tma_tmux::stamp;
use tma_tmux::tmux::{PaneRecord, Tmux};

use crate::manifests::LoadedManifest;

/// The end-anchored read window (last 64 KiB from EOF).
pub const TAIL_WINDOW: u64 = 64 * 1024;
/// The backward-scan cap (widen to 1 MiB before giving up).
pub const SCAN_CAP: u64 = 1024 * 1024;
/// The compiled-in parser id the Codex `[telemetry.context]` channel names.
const CODEX_FORMAT: &str = "codex-rollout-jsonl";

/// The newest reading from one tail poll: the context percent (`None` = no `token_count` record in the
/// scanned window) and a best-effort model name (`None` = no model record in the window).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailResult {
    pub pct: Option<u8>,
    pub model: Option<String>,
}

/// The outcome of polling one session's rollout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailPoll {
    /// The file is byte-for-byte as last polled (same identity/size/mtime): no read was performed.
    Unchanged,
    /// A fresh read of the tail window; carries the newest reading (and best-effort model).
    Fresh(TailResult),
    /// No rollout file was found for this session (or it vanished): nothing to stamp.
    Missing,
}

/// The process-local memo: a per-session discovered path plus a per-file `(identity, size,
/// mtime, last result)` record, so an unchanged file is skipped on the cheap stat alone. In-memory
/// only — not a persisted offset, so the no-stored-state argument and rotation immunity stand.
#[derive(Default)]
pub struct RolloutTail {
    /// session id ⇒ resolved rollout path (so discovery does not re-walk every cycle).
    paths: HashMap<String, PathBuf>,
    /// resolved path ⇒ (file identity/size/mtime, last parsed result).
    files: HashMap<PathBuf, (FileMemo, TailResult)>,
    stat_calls: u64,
    read_calls: u64,
    discover_calls: u64,
}

/// The file-identity tuple the memo compares: a size decrease or identity change invalidates it and
/// forces a rescan; an unchanged tuple skips the read entirely.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileMemo {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl RolloutTail {
    pub fn new() -> RolloutTail {
        RolloutTail::default()
    }

    /// `fs::metadata` calls made — the acceptance-test seam for "quiet-pane steady state is one stat".
    pub fn stat_calls(&self) -> u64 {
        self.stat_calls
    }
    /// Tail reads performed (a changed/new file only); the memo keeps this flat on a quiet pane.
    pub fn read_calls(&self) -> u64 {
        self.read_calls
    }
    /// Directory walks performed (a new/vanished session only); the per-session path cache keeps this flat.
    pub fn discover_calls(&self) -> u64 {
        self.discover_calls
    }

    /// Resolve `session_id` to its rollout file (cached) and poll it. `Missing` when no file resolves.
    pub fn poll_session(&mut self, session_id: &str, codex_home: &Path) -> TailPoll {
        match self.resolve(session_id, codex_home) {
            Some(path) => self.poll(&path),
            None => TailPoll::Missing,
        }
    }

    /// The cached path for `session_id`, re-discovering once when there is no cache entry or the
    /// cached file has vanished (rotation to a new dated file).
    fn resolve(&mut self, session_id: &str, codex_home: &Path) -> Option<PathBuf> {
        if let Some(p) = self.paths.get(session_id).cloned() {
            if p.exists() {
                return Some(p);
            }
            self.paths.remove(session_id);
            self.files.remove(&p);
        }
        self.discover_calls += 1;
        let found = discover_rollout(session_id, codex_home)?;
        self.paths.insert(session_id.to_string(), found.clone());
        Some(found)
    }

    /// Poll a specific rollout path: one stat, then a bounded read + parse only when the file changed.
    pub fn poll(&mut self, path: &Path) -> TailPoll {
        self.stat_calls += 1;
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return TailPoll::Missing,
        };
        let memo = FileMemo {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        };
        if self.files.get(path).is_some_and(|(prev, _)| *prev == memo) {
            return TailPoll::Unchanged;
        }
        self.read_calls += 1;
        let result = read_result(path, memo.size);
        self.files
            .insert(path.to_path_buf(), (memo, result.clone()));
        TailPoll::Fresh(result)
    }
}

/// Read the bounded end-anchored window and parse the newest reading + best-effort model. A read
/// failure yields an empty result (nothing to stamp) — the tail must never error the cycle.
fn read_result(path: &Path, size: u64) -> TailResult {
    let blob = read_scan(path, size).unwrap_or_default();
    TailResult {
        pct: parse_context(CODEX_FORMAT, &blob).and_then(|r| r.pct),
        model: codex_rollout_model(&blob),
    }
}

/// Read the last [`TAIL_WINDOW`] bytes, widening the end-anchored window by [`TAIL_WINDOW`] each step
/// (up to [`SCAN_CAP`]) until the cleaned window holds a `token_count` record — a single heavy turn
/// can append far more than 64 KiB of tool output after the last one, and without the backward scan
/// the gauge would freeze precisely while context grows fastest. Past the cap the newest window
/// is returned as-is (parse yields nothing, the gauge goes stale honestly).
fn read_scan(path: &Path, size: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut window = TAIL_WINDOW;
    loop {
        let start = size.saturating_sub(window);
        let len = size - start;
        file.seek(SeekFrom::Start(start))?;
        // A tolerant read (not `read_exact`): a concurrent truncation between the stat and this read
        // yields fewer bytes rather than an error; the memo catches the shrink next cycle.
        let mut buf = Vec::with_capacity(len as usize);
        (&mut file).take(len).read_to_end(&mut buf)?;
        let blob = clean_window(&buf, start > 0);
        let has_record = parse_context(CODEX_FORMAT, &blob).is_some();
        if has_record || start == 0 || window >= SCAN_CAP {
            return Ok(blob);
        }
        window = (window + TAIL_WINDOW).min(SCAN_CAP);
    }
}

/// Trim partial lines from an end-anchored byte window: drop the leading partial
/// line unless the window starts at the file head, and always drop a trailing partial line (a read
/// caught mid-write). A window with no complete line yields empty.
fn clean_window(bytes: &[u8], drop_leading: bool) -> String {
    let s = String::from_utf8_lossy(bytes);
    let start = if drop_leading {
        match s.find('\n') {
            Some(i) => i + 1,
            None => return String::new(), // one giant partial line: nothing complete
        }
    } else {
        0
    };
    let end = match s.rfind('\n') {
        Some(i) => i + 1, // keep through the last newline; drop any trailing partial
        None => return String::new(),
    };
    if end <= start {
        String::new()
    } else {
        s[start..end].to_string()
    }
}

/// `<CODEX_HOME>` (else `~/.codex`), the root of the dated `sessions/` rollout tree. `None` when
/// neither is set (nothing to tail).
pub fn codex_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".codex"))
}

/// Find the rollout file for `session_id` under `<codex_home>/sessions/YYYY/MM/DD/`.
/// The date is unknown, so every dated leaf is scanned and the newest-mtime `rollout-*<session_id>*.jsonl`
/// wins (a resumed session can leave more than one). `None` when none matches — the caller stamps nothing.
pub fn discover_rollout(session_id: &str, codex_home: &Path) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for day in dated_dirs(&sessions) {
        let Ok(rd) = fs::read_dir(&day) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if !is_rollout_for(&path, session_id) {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(bt, _)| mtime >= *bt) {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The `YYYY/MM/DD` leaf directories under `sessions/`, newest date first (a small three-level walk).
fn dated_dirs(sessions: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for year in subdirs_desc(sessions) {
        for month in subdirs_desc(&year) {
            out.extend(subdirs_desc(&month));
        }
    }
    out
}

/// The immediate subdirectories of `dir`, reverse-sorted (so numeric date components come newest first).
fn subdirs_desc(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return Vec::new(),
    };
    v.sort();
    v.reverse();
    v
}

/// Whether `path`'s filename is a rollout file carrying `session_id` (`rollout-<ts>-<session_id>.jsonl`).
fn is_rollout_for(path: &Path, session_id: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            name.starts_with("rollout-") && name.ends_with(".jsonl") && name.contains(session_id)
        })
}

/// Poll every Codex file-tail pane's rollout once and stamp `@agent_context_pct`
/// through the same evidence-time guard as the push path, plus the best-effort `@agent_model` label.
/// A pane with no `@agent_session`, no `file-tail` context channel, no rollout file, or an unchanged
/// file stamps nothing. `home` is `<CODEX_HOME>` (the caller reads it once per cycle).
pub fn poll_context_tails(
    tmux: &Tmux,
    panes: &[PaneRecord],
    manifests: &[LoadedManifest],
    tail: &mut RolloutTail,
    home: &Path,
    now: u64,
) {
    let mut guarded: Option<bool> = None;
    for rec in panes {
        let Some(name) = rec.options.get(opt::NAME).filter(|v| !v.is_empty()) else {
            continue;
        };
        let Some(session) = rec.options.get(opt::SESSION).filter(|v| !v.is_empty()) else {
            continue;
        };
        if !is_codex_file_tail(manifests, name) {
            continue;
        }
        let TailPoll::Fresh(result) = tail.poll_session(session, home) else {
            continue; // Unchanged / Missing: nothing to stamp
        };
        if let Some(pct) = result.pct {
            let g = *guarded.get_or_insert_with(|| stamp::guarded_writes_supported(tmux, panes));
            // Codex reports no footprint count (see `parse_codex_rollout`): the gauge only.
            let _ = stamp::apply_context(tmux, panes, &rec.pane_id, Some(pct), None, now, g);
        }
        if let Some(model) = result.model {
            let cmd = tma_core::render::set_pane_option(&rec.pane_id, opt::MODEL, &model);
            let _ = tmux.apply(&[cmd]);
        }
    }
}

/// Whether `agent`'s loaded manifest declares a `file-tail` context channel with the Codex parser.
fn is_codex_file_tail(manifests: &[LoadedManifest], agent: &str) -> bool {
    manifests
        .iter()
        .find(|m| m.name == agent)
        .and_then(|m| m.manifest.telemetry.as_ref())
        .and_then(|t| t.context.as_ref())
        .is_some_and(|c| c.channel == Channel::FileTail && c.format == CODEX_FORMAT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A single rollout `token_count` line for `total_tokens` / `window` (⇒ `pct`).
    fn token_count_line(total: u64, window: u64) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"total_tokens":{total}}},"last_token_usage":{{"total_tokens":1}},"model_context_window":{window}}}}}}}"#
        )
    }

    fn write_file(path: &Path, body: &str) {
        let mut f = File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tma-rollout-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn clean_window_drops_leading_and_trailing_partials() {
        // Mid-file window: leading partial before the first \n and trailing partial after the last \n
        // are both dropped, leaving only the complete middle line.
        let bytes = b"tail-of-line-1\ncomplete-line-2\nhead-of-lin";
        assert_eq!(clean_window(bytes, true), "complete-line-2\n");
        // At the file head, the leading partial is kept (it is a real first line).
        assert_eq!(
            clean_window(bytes, false),
            "tail-of-line-1\ncomplete-line-2\n"
        );
        // A window that is one giant partial line (no newline) yields nothing complete.
        assert_eq!(clean_window(b"no-newline-here", true), "");
    }

    #[test]
    fn tail_reads_pct_and_memo_skips_unchanged() {
        let dir = tmpdir("memo");
        let path = dir.join("rollout.jsonl");
        write_file(&path, &format!("{}\n", token_count_line(136_000, 272_000)));
        let mut tail = RolloutTail::new();

        // First poll: one stat, one read, pct computed (136000/272000 = 50%).
        let first = tail.poll(&path);
        assert_eq!(
            first,
            TailPoll::Fresh(TailResult {
                pct: Some(50),
                model: None
            })
        );
        assert_eq!(tail.stat_calls(), 1);
        assert_eq!(tail.read_calls(), 1);

        // Second poll of the unchanged file: the memo skips the read — one more stat, no more reads.
        assert_eq!(tail.poll(&path), TailPoll::Unchanged);
        assert_eq!(tail.stat_calls(), 2, "steady state is one stat per poll");
        assert_eq!(tail.read_calls(), 1, "unchanged file is not re-read");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_finds_newest_record_across_a_chunk_boundary() {
        // The only token_count record sits far from EOF: >64 KiB of trailing tool-output padding
        // pushes it out of the first window, so the backward scan must widen to find it.
        let dir = tmpdir("chunk");
        let path = dir.join("rollout.jsonl");
        let mut body = String::new();
        body.push_str(&token_count_line(204_000, 272_000)); // 75%
        body.push('\n');
        let pad_line = format!("{}\n", "x".repeat(200)); // no token_count
        while body.len() < (TAIL_WINDOW as usize) + 4096 {
            body.push_str(&pad_line);
        }
        write_file(&path, &body);

        let mut tail = RolloutTail::new();
        match tail.poll(&path) {
            TailPoll::Fresh(r) => assert_eq!(
                r.pct,
                Some(75),
                "the backward scan must find the record beyond the first 64 KiB window"
            ),
            other => panic!("expected Fresh, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_with_no_token_count_yields_no_reading() {
        let dir = tmpdir("norecord");
        let path = dir.join("rollout.jsonl");
        write_file(
            &path,
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5-codex\"}}\n",
        );
        let mut tail = RolloutTail::new();
        match tail.poll(&path) {
            // No gauge (the surfaces grey/leave the stored value), but the model is still read.
            TailPoll::Fresh(r) => {
                assert_eq!(r.pct, None);
                assert_eq!(r.model.as_deref(), Some("gpt-5-codex"));
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_matches_the_session_id_under_the_dated_layout() {
        let home = tmpdir("home");
        let sid = "019f99c3-7c57-7963-98e9-f496a7978257";
        let day = home.join("sessions/2026/07/28");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join(format!("rollout-2026-07-28T18-03-01-{sid}.jsonl"));
        write_file(&file, "{}\n");
        // A decoy for a different session must not match.
        write_file(
            &day.join("rollout-2026-07-28T09-00-00-deadbeef-0000-0000-0000-000000000000.jsonl"),
            "{}\n",
        );

        assert_eq!(
            discover_rollout(sid, &home).as_deref(),
            Some(file.as_path())
        );
        assert_eq!(discover_rollout("no-such-session", &home), None);

        // The per-session path is cached (one discovery), and re-discovers only after the file vanishes.
        let mut tail = RolloutTail::new();
        write_file(&file, &format!("{}\n", token_count_line(272_000, 272_000)));
        assert!(matches!(tail.poll_session(sid, &home), TailPoll::Fresh(_)));
        assert_eq!(tail.discover_calls(), 1);
        assert_eq!(tail.poll_session(sid, &home), TailPoll::Unchanged);
        assert_eq!(tail.discover_calls(), 1, "the path cache avoids re-walking");

        let _ = std::fs::remove_dir_all(&home);
    }
}
