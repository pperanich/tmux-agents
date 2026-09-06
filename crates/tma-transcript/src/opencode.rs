//! The OpenCode reader: one held read-only connection per database, and the same window, cursor
//! and budget contract the file readers serve.
//!
//! **The connection is held, and that is the whole design.** Experiment E2 drove 400 committed
//! appends past two readers: a fresh read-only connection per poll made 42 of the writer's own
//! commits fail with `database is locked`, and one long-lived connection made none fail, at 220
//! times the read throughput. A reader that breaks the agent it is watching is a much worse bug
//! than a missing transcript, so [`Reader`](crate::Reader) keeps one connection per database for
//! its own lifetime and never opens a second.
//!
//! **It is a library, not a co-process.** An earlier design shelled out to a held SQLite CLI child
//! and framed its results with a sentinel line. Linking SQLite deletes that protocol and the two
//! failure modes it owned: there is no result data that can imitate the frame, and no PATH lookup
//! that can come back empty. Nothing here spawns a process.
//!
//! **Two paths, because the store has two eras.** Sessions written after opencode's event-sourcing
//! cutover carry `event` rows keyed `(aggregate_id, seq)`, which is a better tail than any file
//! store offers: one integer, no "was the file rewritten" question. Sessions older than the cutover
//! have none, and only `message` and `part` hold them. So history is always served from
//! `message`/`part` ordered by `time_created`, and the event log is used for the forward tail, as a
//! change notification whose payload is then re-read from `part` rather than trusted.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::adapters::{opencode as adapter, Mapped};
use crate::model::{Budget, Cursor, Event, EventKind, FileId, SessionMeta, Store};
use crate::reader::{preview_of, Accumulator};
use crate::{Refusal, Tail, Window, WindowRequest};

/// How long a query waits on the writer's lock before giving up. Long enough to ride out a commit,
/// short enough that a poll cannot hang a caller: opencode's own connections use no timeout at all.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
/// Message timestamps fetched per backward step, the SQL analogue of the file reader's 64 KiB chunk.
const STAMP_BATCH: usize = 64;
/// Event-log rows read per tail poll.
const EVENT_BATCH: usize = 512;
/// Above every timestamp any row can carry, so an end-anchored window is the same query as a paged
/// one rather than a second, unbounded spelling of it.
const OPEN_STAMP: i64 = i64::MAX;

const SQL_HAS_TABLE: &str = "select 1 from sqlite_master where type = 'table' and name = ?1";
const SQL_STAMPS_BEFORE: &str = "select distinct time_created from message \
     where session_id = ?1 and time_created < ?2 order by time_created desc limit ?3";
const SQL_STAMPS_AFTER: &str = "select distinct time_created from message \
     where session_id = ?1 and time_created > ?2 order by time_created limit ?3";
const SQL_GROUP: &str = "select id, coalesce(json_extract(data, '$.role'), '') from message \
     where session_id = ?1 and time_created = ?2 order by id";
const SQL_PARTS: &str = "select id, data from part where message_id = ?1 order by id";
const SQL_SESSION: &str = "select data from session where id = ?1";
const SQL_MODEL: &str = "select json_extract(data, '$.modelID') from message \
     where session_id = ?1 and json_extract(data, '$.modelID') is not null \
     order by time_created desc limit 1";
const SQL_HIGH_WATER: &str = "select seq from event_sequence where aggregate_id = ?1";
const SQL_MAX_SEQ: &str = "select max(seq) from event where aggregate_id = ?1";
const SQL_EVENTS: &str = "select seq, type, data from event \
     where aggregate_id = ?1 and seq > ?2 order by seq limit ?3";
const SQL_MESSAGE_STAMP: &str = "select time_created from message where id = ?1";
const SQL_PART_MESSAGE: &str = "select message_id from part where id = ?1";
const SQL_MESSAGE_MEMO: &str =
    "select coalesce(max(time_updated), 0), count(*) from message where session_id = ?1";

/// One open database, and what a forward tail has already sent from it.
pub(crate) struct Db {
    conn: Connection,
    path: PathBuf,
    /// The database's identity when it was opened. A cursor minted against a different file is
    /// refused rather than resolved against rows that were never the ones it addressed.
    id: FileId,
    has_event_log: bool,
    tails: HashMap<String, TailState>,
}

/// What one session's tail has served. Keyed by part id and event kind because a part is updated in
/// place: the same row is a pending call, then a running one, then a completed one.
#[derive(Default)]
struct TailState {
    seq: i64,
    stamp: i64,
    memo: Option<(i64, i64)>,
    seen: HashMap<(String, &'static str), usize>,
}

/// One message-timestamp group's events, oldest first, with the part each came from.
#[derive(Default)]
struct Group {
    events: Vec<(String, Event)>,
    bytes: u64,
    unknown: u64,
}

impl Db {
    /// Open read-only. `SQLITE_OPEN_READ_ONLY` plus `mode=ro` is belt and braces on purpose: the one
    /// thing this reader must never do is write to, or checkpoint, a database another process owns.
    pub(crate) fn open(path: &Path) -> Result<Db, Refusal> {
        let meta = std::fs::metadata(path).map_err(|e| Refusal::io(path, e))?;
        let id = FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI;
        let conn =
            Connection::open_with_flags(uri(path), flags).map_err(|e| Refusal::db(path, e))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| Refusal::db(path, e))?;
        let mut db = Db {
            conn,
            path: path.to_path_buf(),
            id,
            has_event_log: false,
            tails: HashMap::new(),
        };
        db.has_event_log = db.has_table("event")?;
        Ok(db)
    }

    /// One page of a session, newest first.
    pub(crate) fn window(
        &mut self,
        session: &str,
        req: &WindowRequest,
        unknown: &mut u64,
    ) -> Result<Window, Refusal> {
        let mut hi = match &req.before {
            Some(c) => {
                self.validate(c)?;
                // The cursor addresses one event of a group, so the group is re-read and its
                // earlier events are still owed, exactly as a multi-event JSONL record's are.
                stamp_of(c).saturating_add(1)
            }
            None => OPEN_STAMP,
        };
        let mut acc = Accumulator::new(Store::OpenCode, self.id, 0, req);
        let mut reached_head = false;
        'batches: loop {
            if acc.is_full() {
                break;
            }
            if acc.read_bytes >= req.budget.read_bytes {
                acc.truncated = true;
                break;
            }
            let stamps = self.stamps(SQL_STAMPS_BEFORE, session, hi, STAMP_BATCH)?;
            if stamps.is_empty() {
                reached_head = true;
                break;
            }
            let partial = stamps.len() < STAMP_BATCH;
            for ts in stamps {
                let group = self.group(session, ts)?;
                acc.read_bytes += group.bytes;
                acc.unknown += group.unknown;
                acc.push_newest_first(group.events.into_iter().map(|(_, e)| e));
                hi = ts;
                if acc.is_full() {
                    break 'batches;
                }
                if acc.read_bytes >= req.budget.read_bytes {
                    acc.truncated = true;
                    break 'batches;
                }
            }
            if partial {
                reached_head = true;
                break;
            }
        }
        let meta = match req.before {
            Some(_) => None,
            None => self.session_meta(session)?,
        };
        Ok(acc.finish(reached_head, meta, unknown))
    }

    /// One event, with its body. The group is re-read and re-mapped, so what comes back is the row's
    /// state now rather than its state when the cursor was minted.
    pub(crate) fn body(
        &mut self,
        session: &str,
        cursor: &Cursor,
        unknown: &mut u64,
    ) -> Result<Event, Refusal> {
        self.validate(cursor)?;
        let group = self.group(session, stamp_of(cursor))?;
        *unknown += group.unknown;
        group
            .events
            .into_iter()
            .nth(cursor.part as usize)
            .map(|(_, event)| event)
            .ok_or(Refusal::CursorInvalid)
    }

    /// What has changed since the last poll, oldest first.
    pub(crate) fn tail(
        &mut self,
        session: &str,
        budget: &Budget,
        unknown: &mut u64,
    ) -> Result<Tail, Refusal> {
        match self.high_water(session)? {
            Some(high) => self.event_tail(session, high, budget, unknown),
            None => self.history_tail(session, budget, unknown),
        }
    }

    /// The post-cutover tail. The event row says *that* something changed; the current `part` row
    /// says *what* it now is, which is what makes a call updated three times one advancing event
    /// rather than three.
    fn event_tail(
        &mut self,
        session: &str,
        high: i64,
        budget: &Budget,
        unknown: &mut u64,
    ) -> Result<Tail, Refusal> {
        let stored = self.state(session).seq;
        if high == stored {
            return Ok(Tail::Unchanged);
        }
        // A high-water mark below the stored one means the aggregate was rewritten under us.
        let restarted = high < stored;
        if restarted {
            *self.state(session) = TailState::default();
        }
        let from = if restarted { 0 } else { stored };

        let mut touched: Vec<(i64, i64)> = Vec::new();
        let mut last_seq = from;
        for (seq, ty, data) in self.events_since(session, from, EVENT_BATCH)? {
            last_seq = seq;
            let Some(ts) = self.event_stamp(&ty, &data)? else {
                continue;
            };
            if !touched.iter().any(|(_, seen)| *seen == ts) {
                touched.push((seq, ts));
            }
        }

        let mut events = Vec::new();
        let mut fresh_unknown = 0;
        let mut bytes = 0;
        for (seq, ts) in touched {
            if bytes >= budget.read_bytes {
                // Stop before this group, and leave the seq that introduced it unread.
                last_seq = seq.saturating_sub(1);
                break;
            }
            let group = self.group(session, ts)?;
            bytes += group.bytes;
            fresh_unknown += group.unknown;
            events.extend(self.fresh(session, group));
        }
        self.state(session).seq = last_seq;
        *unknown += fresh_unknown;
        Ok(Tail::Fresh {
            events,
            unknown: fresh_unknown,
            restarted,
        })
    }

    /// The pre-cutover tail: a forward walk of `time_created`, for a session written before opencode
    /// kept an event log. Those sessions are history and no longer grow, which is what makes the
    /// walk enough. A part edited in place in such a session is seen by a window read, not here.
    fn history_tail(
        &mut self,
        session: &str,
        budget: &Budget,
        unknown: &mut u64,
    ) -> Result<Tail, Refusal> {
        let memo = self.message_memo(session)?;
        let state = self.state(session);
        if state.memo == Some(memo) {
            return Ok(Tail::Unchanged);
        }
        // Fewer messages than the last poll saw means the session was rewritten under us.
        let restarted = matches!(state.memo, Some((_, count)) if memo.1 < count);
        if restarted {
            *state = TailState::default();
        }
        let from = state.stamp;

        let stamps = self.stamps(SQL_STAMPS_AFTER, session, from, STAMP_BATCH)?;
        let complete = stamps.len() < STAMP_BATCH;
        let mut events = Vec::new();
        let mut fresh_unknown = 0;
        let mut bytes = 0;
        let mut mark = from;
        let mut stopped = false;
        for ts in stamps {
            if bytes >= budget.read_bytes {
                stopped = true;
                break;
            }
            let group = self.group(session, ts)?;
            bytes += group.bytes;
            fresh_unknown += group.unknown;
            events.extend(self.fresh(session, group));
            mark = ts;
        }
        let state = self.state(session);
        state.stamp = mark;
        // The memo is what makes the next poll free, so it is stamped only when the walk actually
        // finished: a poll stopped by the budget or by its batch has to come back.
        if complete && !stopped {
            state.memo = Some(memo);
        }
        *unknown += fresh_unknown;
        Ok(Tail::Fresh {
            events,
            unknown: fresh_unknown,
            restarted,
        })
    }

    /// The events of a group this session's tail has not served yet.
    fn fresh(&mut self, session: &str, group: Group) -> Vec<Event> {
        let state = self.state(session);
        let mut out = Vec::new();
        for (part, event) in group.events {
            let label = event.kind.label();
            let len = event.body.as_ref().map_or(0, |b| b.as_str().len());
            let key = (part, label);
            match state.seen.get(&key) {
                // A call's name and arguments never change, and a result is minted once, at the
                // settle. Re-sending either is exactly what turns one tool call into three events.
                Some(_) if matches!(label, "tool_call" | "tool_result") => continue,
                Some(seen) if *seen == len => continue,
                _ => {}
            }
            state.seen.insert(key, len);
            out.push(event);
        }
        out
    }

    fn state(&mut self, session: &str) -> &mut TailState {
        self.tails.entry(session.to_string()).or_default()
    }

    /// Every event of one message-timestamp group, oldest first. The group rather than the message
    /// is the unit a cursor addresses, so two messages written in the same millisecond share one
    /// numbering and the pair `(time_created, index)` stays a total order over the session.
    fn group(&self, session: &str, ts: i64) -> Result<Group, Refusal> {
        let mut group = Group::default();
        for (message, role) in self.messages_at(session, ts)? {
            for (part, data) in self.parts_of(&message)? {
                group.bytes += data.len() as u64;
                let mapped = match crate::json::parse(&data) {
                    Ok(value) => adapter::map_part(&role, &value),
                    Err(_) => vec![Mapped::unknown("opencode/part/unparsable".into())],
                };
                for m in mapped {
                    if matches!(m.kind, EventKind::Unknown { .. }) {
                        group.unknown += 1;
                    }
                    let index = u32::try_from(group.events.len()).unwrap_or(u32::MAX);
                    group.events.push((
                        part.clone(),
                        Event {
                            cursor: Cursor {
                                file: self.id,
                                // A database has no meaningful "size when the cursor was minted":
                                // it shrinks on checkpoint without losing a row. Identity carries
                                // the whole staleness check instead.
                                size: 0,
                                offset: u64::try_from(ts).unwrap_or(0),
                                part: index,
                            },
                            ts: Some(ts.to_string()),
                            preview: m.body.as_ref().map(preview_of),
                            kind: m.kind,
                            body: m.body,
                        },
                    ));
                }
            }
        }
        Ok(group)
    }

    /// The session header opencode keeps as a row, plus the model the newest message named.
    fn session_meta(&self, session: &str) -> Result<Option<SessionMeta>, Refusal> {
        let data: Option<String> = self
            .conn
            .query_row(SQL_SESSION, params![session], |r| r.get(0))
            .optional()
            .map_err(|e| self.failed(e))?;
        let Some(data) = data else {
            return Ok(None);
        };
        let model: Option<String> = self
            .conn
            .query_row(SQL_MODEL, params![session], |r| r.get(0))
            .optional()
            .map_err(|e| self.failed(e))?;
        let parsed = crate::json::parse(&data).unwrap_or(crate::json::Value::Null);
        Ok(Some(adapter::session_meta(session, &parsed, model)))
    }

    /// The message timestamp an event row points at, or `None` for an event that is not about one.
    fn event_stamp(&self, ty: &str, data: &str) -> Result<Option<i64>, Refusal> {
        if !ty.starts_with("message.") {
            return Ok(None);
        }
        let Ok(value) = crate::json::parse(data) else {
            return Ok(None);
        };
        let at = |path: &[&str]| {
            value
                .path(path)
                .and_then(crate::json::Value::as_str)
                .map(str::to_string)
        };
        // The payload's own shape has moved before and will again, so several spellings are tried
        // and none of them is trusted for content: only for which row to go and read.
        if let Some(message) = at(&["properties", "info", "id"])
            .or_else(|| at(&["info", "id"]))
            .or_else(|| at(&["properties", "part", "messageID"]))
            .or_else(|| at(&["part", "messageID"]))
            .or_else(|| at(&["messageID"]))
        {
            return self.message_stamp(&message);
        }
        let Some(part) = at(&["properties", "part", "id"])
            .or_else(|| at(&["part", "id"]))
            .or_else(|| at(&["partID"]))
        else {
            return Ok(None);
        };
        let message: Option<String> = self
            .conn
            .query_row(SQL_PART_MESSAGE, params![part], |r| r.get(0))
            .optional()
            .map_err(|e| self.failed(e))?;
        match message {
            Some(message) => self.message_stamp(&message),
            None => Ok(None),
        }
    }

    fn message_stamp(&self, message: &str) -> Result<Option<i64>, Refusal> {
        self.conn
            .query_row(SQL_MESSAGE_STAMP, params![message], |r| r.get(0))
            .optional()
            .map_err(|e| self.failed(e))
    }

    /// The aggregate's high-water mark, or `None` for a session with no event rows at all, which is
    /// what a pre-cutover session looks like.
    fn high_water(&self, session: &str) -> Result<Option<i64>, Refusal> {
        if !self.has_event_log {
            return Ok(None);
        }
        if self.has_table("event_sequence")? {
            let seq: Option<i64> = self
                .conn
                .query_row(SQL_HIGH_WATER, params![session], |r| r.get(0))
                .optional()
                .map_err(|e| self.failed(e))?;
            if seq.is_some() {
                return Ok(seq);
            }
        }
        self.conn
            .query_row(SQL_MAX_SEQ, params![session], |r| r.get(0))
            .map_err(|e| self.failed(e))
    }

    fn events_since(
        &self,
        session: &str,
        seq: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>, Refusal> {
        let mut stmt = self.conn.prepare(SQL_EVENTS).map_err(|e| self.failed(e))?;
        let rows = stmt
            .query_map(params![session, seq, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|e| self.failed(e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| self.failed(e))
    }

    /// `(the newest `time_updated`, the message count)`: the cheap poll that answers "nothing has
    /// changed" without reading a row of content.
    fn message_memo(&self, session: &str) -> Result<(i64, i64), Refusal> {
        self.conn
            .query_row(SQL_MESSAGE_MEMO, params![session], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| self.failed(e))
    }

    fn stamps(
        &self,
        sql: &str,
        session: &str,
        bound: i64,
        limit: usize,
    ) -> Result<Vec<i64>, Refusal> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| self.failed(e))?;
        let rows = stmt
            .query_map(params![session, bound, limit as i64], |r| r.get(0))
            .map_err(|e| self.failed(e))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(|e| self.failed(e))
    }

    /// The messages of one timestamp, with the role read out of the JSON: there is no `role` column.
    fn messages_at(&self, session: &str, ts: i64) -> Result<Vec<(String, String)>, Refusal> {
        let mut stmt = self.conn.prepare(SQL_GROUP).map_err(|e| self.failed(e))?;
        let rows = stmt
            .query_map(params![session, ts], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| self.failed(e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| self.failed(e))
    }

    fn parts_of(&self, message: &str) -> Result<Vec<(String, String)>, Refusal> {
        let mut stmt = self.conn.prepare(SQL_PARTS).map_err(|e| self.failed(e))?;
        let rows = stmt
            .query_map(params![message], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| self.failed(e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| self.failed(e))
    }

    fn has_table(&self, name: &str) -> Result<bool, Refusal> {
        self.conn
            .query_row(SQL_HAS_TABLE, params![name], |r| r.get::<_, i64>(0))
            .optional()
            .map(|hit| hit.is_some())
            .map_err(|e| self.failed(e))
    }

    /// A cursor addresses rows in one database. A cursor minted against another is refused rather
    /// than resolved against whatever now sits at that timestamp.
    fn validate(&self, cursor: &Cursor) -> Result<(), Refusal> {
        if cursor.file == self.id {
            Ok(())
        } else {
            Err(Refusal::CursorInvalid)
        }
    }

    fn failed(&self, source: rusqlite::Error) -> Refusal {
        Refusal::db(&self.path, source)
    }
}

fn stamp_of(cursor: &Cursor) -> i64 {
    i64::try_from(cursor.offset).unwrap_or(i64::MAX)
}

/// `file:<path>?mode=ro`. Only the three characters SQLite would otherwise read as URI syntax are
/// escaped; the crate is unix-only, so there is no drive letter to think about.
fn uri(path: &Path) -> String {
    let mut out = String::from("file:");
    for c in path.to_string_lossy().chars() {
        match c {
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            _ => out.push(c),
        }
    }
    out.push_str("?mode=ro");
    out
}
