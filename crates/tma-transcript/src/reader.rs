//! The bounded reader: an end-anchored window that pages backwards, a forward tail, and a
//! single-record body fetch.
//!
//! Three things are carried over from the codex tail (`tma_runtime::rollout`) because they are
//! already right: the `(dev, ino, size, mtime, mtime_nsec)` memo, the `Unchanged` short-circuit
//! that makes a quiet pane cost one `stat`, and reading in bounded chunks with the partial leading
//! and trailing lines dropped. What is new is direction. That tail asks "what is the newest
//! `token_count`" and needs no stored position; this one asks "give me the 200 events before cursor
//! X", which needs an exact one, and answers it without ever reading the head of a 44 MiB file.
//!
//! The window walks **backwards** in chunks, keeping the bytes it has read but not yet attributed
//! to a complete line, so a record straddling a chunk boundary is never split and never read twice.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::adapters;
use crate::model::{Body, Budget, Cursor, Event, EventKind, FileId, SessionMeta, Store};
use crate::{Refusal, Source};

/// One backward step. Matches the codex tail's window so the two readers cost the same per step.
const CHUNK: u64 = 64 * 1024;
/// How far into a file the session header is looked for. Every store writes it in the first record.
const HEAD_BYTES: u64 = 64 * 1024;
/// The largest single record a body fetch will materialize.
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

/// What one window call asks for.
#[derive(Debug, Clone)]
pub struct WindowRequest {
    /// How many events to return, newest first.
    pub last: usize,
    /// Return only events strictly older than this one. `None` starts at the end of the file.
    pub before: Option<Cursor>,
    /// Drop bodies and cap every string leaf at [`Budget::header_bytes`]. The default, and what a
    /// remote caller always wants.
    pub headers_only: bool,
    pub budget: Budget,
}

impl WindowRequest {
    pub fn new(last: usize) -> WindowRequest {
        WindowRequest {
            last,
            before: None,
            headers_only: true,
            budget: Budget::default(),
        }
    }

    pub fn before(mut self, cursor: Option<Cursor>) -> WindowRequest {
        self.before = cursor;
        self
    }

    pub fn with_bodies(mut self) -> WindowRequest {
        self.headers_only = false;
        self
    }

    pub fn budget(mut self, budget: Budget) -> WindowRequest {
        self.budget = budget;
        self
    }
}

/// One page of a conversation, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub events: Vec<Event>,
    /// Pass back as [`WindowRequest::before`] for the next page. `None` means the head of the file
    /// is in this page.
    pub older: Option<Cursor>,
    /// A budget stopped the read before `last` events were found. The page is still exact; there
    /// is simply more behind `older`.
    pub budget_truncated: bool,
    /// Records in this page that no adapter arm claimed.
    pub unknown: u64,
    /// The session header, read from the head of the file on the first page only.
    pub session: Option<SessionMeta>,
}

/// The outcome of one forward tail poll.
#[derive(Debug, Clone, PartialEq)]
pub enum Tail {
    /// Byte-for-byte as last polled: no read was performed.
    Unchanged,
    /// New records since the stored offset, oldest first.
    Fresh {
        events: Vec<Event>,
        unknown: u64,
        /// The file was replaced or truncated, so the tail restarted from its head.
        restarted: bool,
    },
}

/// The file-identity tuple the memo compares. A change in any field forces a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileMemo {
    id: FileId,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

/// The reader's process-local state: the poll memo, one forward-tail offset per file identity, and
/// one held SQLite connection per OpenCode database.
#[derive(Default)]
pub struct Reader {
    memo: HashMap<PathBuf, FileMemo>,
    offsets: HashMap<FileId, u64>,
    /// Held for the reader's lifetime on purpose (E2): a reader that reconnects per poll makes the
    /// writing agent's own commits fail, which is a far worse bug than a missing transcript.
    #[cfg(feature = "opencode")]
    dbs: HashMap<PathBuf, crate::opencode::Db>,
    unknown: u64,
    stat_calls: u64,
    read_calls: u64,
}

impl Reader {
    pub fn new() -> Reader {
        Reader::default()
    }

    /// Every unclaimed record this reader has emitted. A drift check watches this: an unknown is a
    /// hole in an adapter, and a store that grows a record type should raise it rather than error.
    pub fn unknown(&self) -> u64 {
        self.unknown
    }

    /// `fs::metadata` calls made. The seam for "a quiet pane's steady state is one stat".
    pub fn stat_calls(&self) -> u64 {
        self.stat_calls
    }

    /// Reads performed against a transcript file.
    pub fn read_calls(&self) -> u64 {
        self.read_calls
    }

    /// One page of `source`, newest first.
    pub fn window(&mut self, source: &Source, req: &WindowRequest) -> Result<Window, Refusal> {
        if !source.store.is_file_store() {
            return self.db_window(source, req);
        }
        let stat = self.stat(source)?;
        let scan_hi = match &req.before {
            Some(c) => {
                validate(c, &stat)?;
                c.offset
            }
            None => stat.size,
        };

        let mut file = open(&source.path)?;
        self.read_calls += 1;
        let mut acc = Accumulator::new(source.store, stat.id, stat.size, req);

        // Earlier events of the record the `before` cursor points into: the cursor addresses one
        // event of a multi-event record, so paging must not skip that record's earlier halves.
        if let Some(c) = &req.before {
            if c.part > 0 {
                let (line, _) = read_record(&mut file, c.offset, stat.size)?;
                acc.read_bytes += line.len() as u64;
                let events = acc.map_line(&line, c.offset);
                acc.push_newest_first(events.into_iter().take(c.part as usize));
            }
        }

        let mut lo_cur = scan_hi;
        let mut pending: Vec<u8> = Vec::new();
        let mut first_chunk = true;
        let mut reached_head = false;
        loop {
            if lo_cur == 0 {
                reached_head = true;
                break;
            }
            if acc.is_full() {
                break;
            }
            if acc.read_bytes >= req.budget.read_bytes {
                acc.truncated = true;
                break;
            }
            let want = CHUNK.min(req.budget.read_bytes - acc.read_bytes);
            let lo = lo_cur.saturating_sub(want);
            let mut chunk = read_range(&mut file, lo, lo_cur)?;
            acc.read_bytes += chunk.len() as u64;
            chunk.extend_from_slice(&pending);
            pending = chunk;
            lo_cur = lo;

            if first_chunk {
                first_chunk = false;
                // A read caught mid-append leaves an incomplete last line; drop it, the way the
                // codex tail's `clean_window` does.
                match pending.iter().rposition(|&b| b == b'\n') {
                    Some(i) => pending.truncate(i + 1),
                    None if lo_cur > 0 => continue, // nothing complete yet: widen
                    None => {}
                }
            }
            // At the head every byte is a complete line; otherwise the leading partial belongs to
            // a record whose start is still further back.
            let body_start = if lo_cur == 0 {
                0
            } else {
                match pending.iter().position(|&b| b == b'\n') {
                    Some(i) => i + 1,
                    None => continue,
                }
            };
            acc.absorb(&pending[body_start..], lo_cur + body_start as u64);
            pending.truncate(body_start);
        }

        let session = match req.before {
            Some(_) => None,
            None => self.head_session(source, &mut file)?,
        };
        Ok(acc.finish(reached_head, session, &mut self.unknown))
    }

    /// New records since the last tail poll, oldest first. `Unchanged` when the file's memo tuple
    /// is untouched, which is the whole point: a fleet of idle panes costs one `stat` each.
    pub fn tail(&mut self, source: &Source, budget: &Budget) -> Result<Tail, Refusal> {
        if !source.store.is_file_store() {
            return self.db_tail(source, budget);
        }
        let stat = self.stat(source)?;
        if self.memo.get(&source.path) == Some(&stat) {
            return Ok(Tail::Unchanged);
        }

        let stored = self.offsets.get(&stat.id).copied().unwrap_or(0);
        // A stored offset past the end means the file was rewritten under us: start over rather
        // than resume into the middle of a record that is no longer there.
        let restarted = stored > stat.size;
        let mut offset = if restarted { 0 } else { stored };

        let mut file = open(&source.path)?;
        self.read_calls += 1;
        let end = stat.size.min(offset + budget.read_bytes);
        let buf = read_range(&mut file, offset, end)?;
        // Only whole lines advance the offset, so a record split by the budget or by a live append
        // is re-read next poll rather than half-parsed.
        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => 0,
        };

        // A tail is unbounded in count (the budget already bounded the bytes) and keeps its bodies:
        // its consumer is the push path, not a remote page.
        let req = WindowRequest {
            last: usize::MAX,
            before: None,
            headers_only: false,
            budget: *budget,
        };
        let mut acc = Accumulator::new(source.store, stat.id, stat.size, &req);
        let events = acc.map_range(&buf[..complete], offset);
        offset += complete as u64;
        self.offsets.insert(stat.id, offset);
        // The memo is stamped only after a successful read, so a failed poll retries instead of
        // reporting the file unchanged next time.
        self.memo.insert(source.path.clone(), stat);
        self.unknown += acc.unknown;
        Ok(Tail::Fresh {
            events,
            unknown: acc.unknown,
            restarted,
        })
    }

    /// One event, with its body. The cursor is validated against the file first, so a stale one is
    /// a typed refusal rather than a parse of whatever now sits at that offset.
    pub fn body(&mut self, source: &Source, cursor: &Cursor) -> Result<Event, Refusal> {
        if !source.store.is_file_store() {
            return self.db_body(source, cursor);
        }
        let stat = self.stat(source)?;
        validate(cursor, &stat)?;
        let mut file = open(&source.path)?;
        self.read_calls += 1;
        let (line, complete) = read_record(&mut file, cursor.offset, stat.size)?;
        if !complete {
            return Err(Refusal::RecordTooLarge(MAX_RECORD_BYTES));
        }
        let req = WindowRequest::new(1).with_bodies();
        let mut acc = Accumulator::new(source.store, stat.id, stat.size, &req);
        let events = acc.map_line(&line, cursor.offset);
        self.unknown += acc.unknown;
        events
            .into_iter()
            .nth(cursor.part as usize)
            .ok_or(Refusal::CursorInvalid)
    }

    /// The session header from the head of the file. Bounded and separate from the window's own
    /// budget: it is one small read of the first record, not part of the page.
    fn head_session(
        &mut self,
        source: &Source,
        file: &mut File,
    ) -> Result<Option<SessionMeta>, Refusal> {
        self.read_calls += 1;
        let buf = read_range(file, 0, HEAD_BYTES)?;
        for line in buf.split_inclusive(|&b| b == b'\n') {
            let Ok(text) = std::str::from_utf8(strip_eol(line)) else {
                continue;
            };
            if let Ok(v) = crate::json::parse(text.trim()) {
                if let Some(meta) = adapters::session_meta(source.store, &v) {
                    return Ok(Some(meta));
                }
            }
        }
        Ok(None)
    }

    /// The held connection for this database, opened on first use.
    #[cfg(feature = "opencode")]
    fn db(&mut self, source: &Source) -> Result<&mut crate::opencode::Db, Refusal> {
        if !self.dbs.contains_key(&source.path) {
            self.stat_calls += 1;
            let db = crate::opencode::Db::open(&source.path)?;
            self.dbs.insert(source.path.clone(), db);
        }
        Ok(self
            .dbs
            .get_mut(&source.path)
            .expect("the connection was just inserted"))
    }

    #[cfg(feature = "opencode")]
    fn db_window(&mut self, source: &Source, req: &WindowRequest) -> Result<Window, Refusal> {
        let session = source.session.clone().ok_or(Refusal::NoTranscript)?;
        self.read_calls += 1;
        let mut unknown = 0;
        let window = self.db(source)?.window(&session, req, &mut unknown)?;
        self.unknown += unknown;
        Ok(window)
    }

    #[cfg(feature = "opencode")]
    fn db_tail(&mut self, source: &Source, budget: &Budget) -> Result<Tail, Refusal> {
        let session = source.session.clone().ok_or(Refusal::NoTranscript)?;
        self.read_calls += 1;
        let mut unknown = 0;
        let tail = self.db(source)?.tail(&session, budget, &mut unknown)?;
        self.unknown += unknown;
        Ok(tail)
    }

    #[cfg(feature = "opencode")]
    fn db_body(&mut self, source: &Source, cursor: &Cursor) -> Result<Event, Refusal> {
        let session = source.session.clone().ok_or(Refusal::NoTranscript)?;
        self.read_calls += 1;
        let mut unknown = 0;
        let event = self.db(source)?.body(&session, cursor, &mut unknown)?;
        self.unknown += unknown;
        Ok(event)
    }

    // Compiled out, the database stores answer the way every unreadable store does: by name.
    #[cfg(not(feature = "opencode"))]
    fn db_window(&mut self, source: &Source, _req: &WindowRequest) -> Result<Window, Refusal> {
        Err(Refusal::for_store(source.store))
    }

    #[cfg(not(feature = "opencode"))]
    fn db_tail(&mut self, source: &Source, _budget: &Budget) -> Result<Tail, Refusal> {
        Err(Refusal::for_store(source.store))
    }

    #[cfg(not(feature = "opencode"))]
    fn db_body(&mut self, source: &Source, _cursor: &Cursor) -> Result<Event, Refusal> {
        Err(Refusal::for_store(source.store))
    }

    fn stat(&mut self, source: &Source) -> Result<FileMemo, Refusal> {
        if !source.store.is_readable() {
            return Err(Refusal::for_store(source.store));
        }
        self.stat_calls += 1;
        let meta = fs::metadata(&source.path).map_err(|e| Refusal::io(&source.path, e))?;
        Ok(FileMemo {
            id: FileId {
                dev: meta.dev(),
                ino: meta.ino(),
            },
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

/// A cursor is valid only while it still addresses the same bytes: the same file, never shortened,
/// and an offset still inside it. Compaction, truncation and an inode change all fail here.
fn validate(cursor: &Cursor, stat: &FileMemo) -> Result<(), Refusal> {
    let same_file = cursor.file == stat.id;
    let only_grew = stat.size >= cursor.size;
    let inside = cursor.offset < stat.size;
    if same_file && only_grew && inside {
        Ok(())
    } else {
        Err(Refusal::CursorInvalid)
    }
}

/// Collects events newest-first under the request's budgets. Shared with the OpenCode reader, which
/// produces its events from SQL rows rather than from lines but owes the caller the same budgets.
pub(crate) struct Accumulator<'a> {
    store: Store,
    id: FileId,
    size: u64,
    req: &'a WindowRequest,
    events: Vec<Event>,
    frame: usize,
    pub(crate) read_bytes: u64,
    pub(crate) unknown: u64,
    pub(crate) truncated: bool,
    /// An event was reached and not kept, so there is a page behind this one. Tracked here rather
    /// than inferred from "did the scan reach the head", because filling the requested count is the
    /// common way a page ends and the scan may have reached the head in the same chunk.
    more: bool,
}

impl<'a> Accumulator<'a> {
    pub(crate) fn new(
        store: Store,
        id: FileId,
        size: u64,
        req: &'a WindowRequest,
    ) -> Accumulator<'a> {
        Accumulator {
            store,
            id,
            size,
            req,
            events: Vec::new(),
            frame: 0,
            read_bytes: 0,
            unknown: 0,
            truncated: false,
            more: false,
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.events.len() >= self.req.last || self.truncated
    }

    /// Map a run of complete lines starting at `base`, then append them newest-first.
    fn absorb(&mut self, bytes: &[u8], base: u64) {
        let events = self.map_range(bytes, base);
        self.push_newest_first(events.into_iter());
    }

    /// Map complete lines in file order.
    fn map_range(&mut self, bytes: &[u8], base: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut off = base;
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            out.extend(self.map_line(line, off));
            off += line.len() as u64;
        }
        out
    }

    /// One record to its events, in the order they belong in the stream. A line that will not parse
    /// becomes a single `Unknown` rather than a dropped record: a hole that shows is recoverable,
    /// a hole that does not is not.
    fn map_line(&mut self, line: &[u8], offset: u64) -> Vec<Event> {
        let trimmed = strip_eol(line);
        if trimmed.iter().all(u8::is_ascii_whitespace) {
            return Vec::new();
        }
        let cursor_at = |part: usize| Cursor {
            file: self.id,
            size: self.size,
            offset,
            part: part as u32,
        };
        let parsed = std::str::from_utf8(trimmed)
            .ok()
            .and_then(|t| crate::json::parse(t.trim()).ok());
        let Some(value) = parsed else {
            self.unknown += 1;
            return vec![Event {
                cursor: cursor_at(0),
                ts: None,
                kind: EventKind::Unknown {
                    type_name: format!("{}/unparsable", self.store),
                },
                preview: None,
                body: None,
            }];
        };
        let (ts, mapped) = adapters::map_record(self.store, &value);
        mapped
            .into_iter()
            .enumerate()
            .map(|(part, m)| {
                if matches!(m.kind, EventKind::Unknown { .. }) {
                    self.unknown += 1;
                }
                Event {
                    cursor: cursor_at(part),
                    ts: ts.clone(),
                    preview: m.body.as_ref().map(preview_of),
                    kind: m.kind,
                    body: m.body,
                }
            })
            .collect()
    }

    /// Append events that are older than everything collected so far, newest of them first, under
    /// the `before` filter and the frame budget.
    pub(crate) fn push_newest_first(&mut self, events: impl DoubleEndedIterator<Item = Event>) {
        for event in events.rev() {
            // The `before` filter runs before the count, so a record's own earlier halves are
            // skipped without being mistaken for a page that is already full.
            if let Some(b) = &self.req.before {
                if event.cursor.position() >= b.position() {
                    continue;
                }
            }
            if self.is_full() {
                self.more = true;
                return;
            }
            let event = if self.req.headers_only {
                let header = event.to_header(&self.req.budget);
                let cost = header.header_cost();
                // A frame budget that stopped the first event would return an empty page, which is
                // worse than one oversized header.
                if !self.events.is_empty() && self.frame + cost > self.req.budget.frame_bytes {
                    self.truncated = true;
                    self.more = true;
                    return;
                }
                self.frame += cost;
                header
            } else {
                event
            };
            self.events.push(event);
        }
    }

    pub(crate) fn finish(
        self,
        reached_head: bool,
        session: Option<SessionMeta>,
        total_unknown: &mut u64,
    ) -> Window {
        let more = self.more || !reached_head;
        let older = more.then(|| self.events.last().map(|e| e.cursor)).flatten();
        *total_unknown += self.unknown;
        Window {
            events: self.events,
            older,
            budget_truncated: self.truncated,
            unknown: self.unknown,
            session,
        }
    }
}

/// A body's first line, which is what a list row shows.
pub(crate) fn preview_of(body: &Body) -> String {
    body.as_str()
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_string()
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn open(path: &Path) -> Result<File, Refusal> {
    File::open(path).map_err(|e| Refusal::io(path, e))
}

/// A tolerant ranged read: fewer bytes than asked for is a concurrent truncation, not an error.
fn read_range(file: &mut File, lo: u64, hi: u64) -> Result<Vec<u8>, Refusal> {
    if hi <= lo {
        return Ok(Vec::new());
    }
    read_range_io(file, lo, hi).map_err(Refusal::read)
}

fn read_range_io(file: &mut File, lo: u64, hi: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(lo))?;
    let len = hi - lo;
    let mut buf = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// One record starting at `offset`, up to its newline. The bool is false when no newline was found
/// within [`MAX_RECORD_BYTES`], which is the only way a body fetch refuses.
fn read_record(file: &mut File, offset: u64, size: u64) -> Result<(Vec<u8>, bool), Refusal> {
    let end = size.min(offset + MAX_RECORD_BYTES);
    let buf = read_range(file, offset, end)?;
    match buf.iter().position(|&b| b == b'\n') {
        Some(i) => Ok((buf[..=i].to_vec(), true)),
        // The last record of a file legitimately has no trailing newline.
        None if end == size => Ok((buf, true)),
        None => Ok((buf, false)),
    }
}
