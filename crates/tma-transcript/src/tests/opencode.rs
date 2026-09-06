//! The OpenCode reader, against synthetic databases built here.
//!
//! Nothing in this file touches a real store. The schema is the committed one beside the
//! expectation (`fixtures/stores/opencode/<version>/opencode-<version>-schema.sql`), measured by E2
//! and the owed-verifications sweep, and every row below is generated.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rusqlite::{params, Connection};

use super::{assert_expected, describe, fixtures, Scratch};
use crate::discovery::{self, PaneFacts, StoreRoots};
use crate::model::{Budget, EventKind, ResultStatus, Store};
use crate::{Reader, Refusal, Source, Tail, WindowRequest};

/// The opencode version the committed expectation imitates.
const VERSION: &str = "1.18.18";
const SESSION: &str = "ses_synthetic01";

/// The store's shape, committed beside the expectation so it can be reviewed and so the integration
/// suite builds its database from the same DDL this one does.
const SCHEMA: &str =
    include_str!("../../fixtures/stores/opencode/1.18.18/opencode-1.18.18-schema.sql");

/// A synthetic `opencode.db`, and the writer that fills it.
struct Fixture {
    conn: Connection,
    path: PathBuf,
    seq: AtomicUsize,
}

impl Fixture {
    /// A database with the schema and WAL journalling, which is what opencode itself runs.
    fn create(path: &Path) -> Fixture {
        std::fs::create_dir_all(path.parent().expect("a parent directory"))
            .expect("create the store directory");
        let conn = Connection::open(path).expect("create the synthetic store");
        conn.pragma_update(None, "journal_mode", "wal")
            .expect("wal journalling");
        conn.execute_batch(SCHEMA).expect("the schema applies");
        Fixture {
            conn,
            path: path.to_path_buf(),
            seq: AtomicUsize::new(0),
        }
    }

    /// The same store with the event log dropped: a session written before opencode's cutover has
    /// only `message` and `part` behind it.
    fn without_event_log(self) -> Fixture {
        self.conn
            .execute_batch("drop table event; drop table event_sequence;")
            .expect("drop the event log");
        self
    }

    fn source(&self, session: &str) -> Source {
        Source {
            store: Store::OpenCode,
            path: self.path.clone(),
            session: Some(session.to_string()),
        }
    }

    fn session(&self, id: &str, ts: i64) {
        let data = format!(
            r#"{{"id":"{id}","title":"synthetic session","directory":"/synthetic/project","version":"{VERSION}"}}"#
        );
        self.conn
            .execute(
                "insert into session values (?1, null, ?2, ?2, ?3)",
                params![id, ts, data],
            )
            .expect("insert the session");
    }

    fn message(&self, id: &str, session: &str, ts: i64, role: &str) {
        let data = format!(
            r#"{{"id":"{id}","role":"{role}","sessionID":"{session}","modelID":"synthetic-model","providerID":"synthetic"}}"#
        );
        self.conn
            .execute(
                "insert into message values (?1, ?2, ?3, ?3, ?4)",
                params![id, session, ts, data],
            )
            .expect("insert the message");
    }

    fn part(&self, id: &str, message: &str, session: &str, ts: i64, data: &str) {
        self.conn
            .execute(
                "insert into part values (?1, ?2, ?3, ?4, ?4, ?5)",
                params![id, message, session, ts, data],
            )
            .expect("insert the part");
        self.log(
            session,
            "message.part.updated.1",
            &part_event(message, id, data),
        );
    }

    /// A part row mutating in place, which is what a tool call does as it runs.
    fn update_part(&self, id: &str, message: &str, session: &str, data: &str) {
        self.conn
            .execute("update part set data = ?2 where id = ?1", params![id, data])
            .expect("update the part");
        self.log(
            session,
            "message.part.updated.1",
            &part_event(message, id, data),
        );
    }

    /// One event-log row, and the aggregate's new high-water mark.
    fn log(&self, aggregate: &str, ty: &str, data: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) as i64 + 1;
        self.conn
            .execute(
                "insert into event values (?1, ?2, ?3, ?4, ?5)",
                params![format!("evt_{seq:06}"), aggregate, seq, ty, data],
            )
            .expect("insert the event");
        self.conn
            .execute(
                "insert into event_sequence values (?1, ?2) \
                 on conflict(aggregate_id) do update set seq = ?2",
                params![aggregate, seq],
            )
            .expect("stamp the high-water mark");
    }

    /// The committed fixture's conversation: every part type the adapter claims, once.
    fn conversation(&self, session: &str) {
        self.session(session, 1_000);
        self.log(
            session,
            "session.created.1",
            &format!(r#"{{"sessionID":"{session}"}}"#),
        );

        self.message("msg_0001", session, 1_000, "user");
        self.part(
            "prt_0001",
            "msg_0001",
            session,
            1_000,
            r#"{"type":"text","text":"synthetic prompt"}"#,
        );

        self.message("msg_0002", session, 2_000, "assistant");
        self.part(
            "prt_0002",
            "msg_0002",
            session,
            2_000,
            r#"{"type":"step-start"}"#,
        );
        self.part(
            "prt_0003",
            "msg_0002",
            session,
            2_000,
            r#"{"type":"reasoning","text":"synthetic reasoning"}"#,
        );
        self.part(
            "prt_0004",
            "msg_0002",
            session,
            2_000,
            r#"{"type":"text","text":"synthetic answer"}"#,
        );
        self.part("prt_0005", "msg_0002", session, 2_000, &completed_tool());
        self.part("prt_0006", "msg_0002", session, 2_000, &rejected_tool());
        self.part(
            "prt_0007",
            "msg_0002",
            session,
            2_000,
            r#"{"type":"step-finish","finishReason":"stop","cost":0.0123,"tokens":{"input":1200,"output":340,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
        );

        self.message("msg_0003", session, 3_000, "assistant");
        self.part(
            "prt_0008",
            "msg_0003",
            session,
            3_000,
            r#"{"type":"tool","tool":"task","callID":"call_task","state":{"status":"completed","metadata":{"sessionID":"ses_synthetic02"}}}"#,
        );
        self.part(
            "prt_0009",
            "msg_0003",
            session,
            3_000,
            r#"{"type":"file","mime":"text/plain","filename":"synthetic.txt","url":"file:///synthetic.txt"}"#,
        );
        self.part(
            "prt_0010",
            "msg_0003",
            session,
            3_000,
            r#"{"type":"compaction","messageID":"msg_0002"}"#,
        );
        self.part(
            "prt_0011",
            "msg_0003",
            session,
            3_000,
            r#"{"type":"agent","name":"synthetic"}"#,
        );
    }
}

fn part_event(message: &str, part: &str, data: &str) -> String {
    format!(
        r#"{{"properties":{{"part":{{"id":"{part}","messageID":"{message}"}}}},"info":{data}}}"#
    )
}

fn completed_tool() -> String {
    r#"{"type":"tool","tool":"bash","callID":"call_ok","state":{"status":"completed","input":{"command":"printf tma-probe-ok","description":"synthetic"},"output":"tma-probe-ok","time":{"start":2000,"end":2010}}}"#.to_string()
}

/// The shape a denied call settles into: `error` status, with the user's own feedback quoted.
fn rejected_tool() -> String {
    r#"{"type":"tool","tool":"write","callID":"call_denied","state":{"status":"error","input":{"filePath":"/synthetic/out.txt"},"error":"The user rejected permission to use this specific tool call with the following feedback: do not write that"}}"#.to_string()
}

/// A tool part at one point in its life, so a test can walk it through its states.
fn tool_at(status: &str) -> String {
    let settled = match status {
        "completed" => r#","output":"tma-probe-ok""#,
        "error" => r#","error":"denied""#,
        _ => "",
    };
    format!(
        r#"{{"type":"tool","tool":"bash","callID":"call_walk","state":{{"status":"{status}","input":{{"command":"printf tma-probe-ok"}}{settled}}}}}"#
    )
}

/// A-234's opencode arm: the emitted sequence equals the committed expectation, the store maps with
/// no unknowns, and the session header comes off the `session` row.
#[test]
fn the_synthetic_store_maps_to_the_committed_sequence() {
    let scratch = Scratch::new("opencode-corpus");
    let store = Fixture::create(&scratch.join("share/opencode/opencode.db"));
    store.conversation(SESSION);

    let source = store.source(SESSION);
    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10_000).with_bodies())
        .expect("the synthetic store must be readable");
    assert_eq!(page.older, None, "the whole session must fit in one page");
    assert_eq!(page.unknown, 0, "the fixture must map cleanly");

    let meta = page.session.as_ref().expect("a session header");
    assert_eq!(meta.agent, "opencode");
    assert_eq!(meta.session_id.as_deref(), Some(SESSION));
    assert_eq!(meta.cwd.as_deref(), Some("/synthetic/project"));
    assert_eq!(
        meta.version.as_deref(),
        Some(VERSION),
        "the in-store stamp must agree with the fixture directory"
    );

    let mut events = page.events;
    events.reverse();
    let lines: Vec<String> = events.iter().map(describe).collect();
    assert_expected(
        &fixtures().join(format!(
            "stores/opencode/{VERSION}/opencode-{VERSION}-session.expected.txt"
        )),
        &lines,
    );
}

/// A-237: a part row observed at `pending`, then `running`, then `completed` is one tool call whose
/// status advances, not three events. The status while a permission is pending is `running`, not
/// `pending`, so the walk covers both.
#[test]
fn a_part_updated_in_place_is_one_advancing_tool_call() {
    let scratch = Scratch::new("opencode-advance");
    let store = Fixture::create(&scratch.join("share/opencode/opencode.db"));
    store.session(SESSION, 1_000);
    store.message("msg_0001", SESSION, 1_000, "assistant");
    store.part("prt_0001", "msg_0001", SESSION, 1_000, &tool_at("pending"));

    let source = store.source(SESSION);
    let mut reader = Reader::new();
    let mut labels = Vec::new();
    let mut statuses = Vec::new();
    for status in ["pending", "running", "completed"] {
        if status != "pending" {
            store.update_part("prt_0001", "msg_0001", SESSION, &tool_at(status));
        }
        let Tail::Fresh { events, .. } = reader
            .tail(&source, &Budget::default())
            .expect("a tail poll")
        else {
            panic!("the store changed, so the poll must not report it unchanged");
        };
        for event in &events {
            labels.push(event.kind.label());
            if let EventKind::ToolResult { status, .. } = &event.kind {
                statuses.push(*status);
            }
        }
    }
    assert_eq!(
        labels,
        vec!["tool_call", "tool_result"],
        "three observations of one row are one call and one result"
    );
    assert_eq!(statuses, vec![ResultStatus::Ok]);

    // A window over the settled row is the same two events, under one cursor each.
    let page = reader
        .window(&source, &WindowRequest::new(10).with_bodies())
        .expect("a window");
    let kinds: Vec<&str> = page.events.iter().map(|e| e.kind.label()).collect();
    assert_eq!(kinds, vec!["tool_result", "tool_call"]);
    let body = page.events[0].body.as_ref().expect("the result's body");
    assert_eq!(body.as_str(), "tma-probe-ok");
}

/// A-243: the held-connection rule. A writer commits 400 appends with no busy timeout of its own,
/// exactly as opencode's own connections do, while a held read-only reader polls as fast as it can.
/// The writer must not fail once, and the reader must see every row.
#[test]
fn a_held_reader_never_makes_the_writer_fail() {
    const APPENDS: usize = 400;
    let scratch = Scratch::new("opencode-concurrency");
    let path = scratch.join("share/opencode/opencode.db");
    let store = Fixture::create(&path);
    store.session(SESSION, 1_000);
    let source = store.source(SESSION);
    drop(store);

    // Held means held from before the writing starts. Opening a connection is the one moment a
    // read-only reader can take a lock the writer wants, and with no busy timeout on the writer's
    // side that costs a commit: measured at roughly one collision per 25 opens, which is the whole
    // of what the per-poll shape does wrong, 1,149 times over. This poll is that one open.
    let mut reader = Reader::new();
    let mut seen = 0;
    let mut polls = 0;
    poll(&mut reader, &source, &mut seen, &mut polls);
    assert_eq!(seen, 0, "the store holds no appends yet");

    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let conn = Connection::open(&writer_path).expect("the writer opens");
        conn.busy_timeout(Duration::from_millis(0))
            .expect("no busy timeout, the way opencode runs");
        conn.pragma_update(None, "synchronous", "normal")
            .expect("wal synchronous");
        let mut failures = 0;
        for i in 1..=APPENDS {
            let ts = 2_000 + i as i64;
            let (message, part) = (format!("msg_{i:06}"), format!("prt_{i:06}"));
            let data = format!(r#"{{"type":"text","text":"append {i}"}}"#);
            let result = (|| -> rusqlite::Result<()> {
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "insert into message values (?1, ?2, ?3, ?3, ?4)",
                    params![
                        message,
                        SESSION,
                        ts,
                        format!(r#"{{"id":"{message}","role":"assistant"}}"#)
                    ],
                )?;
                tx.execute(
                    "insert into part values (?1, ?2, ?3, ?4, ?4, ?5)",
                    params![part, message, SESSION, ts, data],
                )?;
                tx.execute(
                    "insert into event values (?1, ?2, ?3, ?4, ?5)",
                    params![
                        format!("evt_{i:06}"),
                        SESSION,
                        i as i64,
                        "message.part.updated.1",
                        part_event(&message, &part, &data)
                    ],
                )?;
                tx.execute(
                    "insert into event_sequence values (?1, ?2) \
                     on conflict(aggregate_id) do update set seq = ?2",
                    params![SESSION, i as i64],
                )?;
                tx.commit()
            })();
            if result.is_err() {
                failures += 1;
            }
        }
        failures
    });

    /// One poll. `false` once the store reports itself unchanged, which is the reader saying it has
    /// caught up: draining on that rather than on a clock keeps the test off the wall clock.
    fn poll(reader: &mut Reader, source: &Source, seen: &mut usize, polls: &mut usize) -> bool {
        *polls += 1;
        match reader
            .tail(source, &Budget::default())
            .expect("a tail poll")
        {
            Tail::Fresh { events, .. } => {
                *seen += events
                    .iter()
                    .filter(|e| matches!(e.kind, EventKind::AssistantText { .. }))
                    .count();
                true
            }
            Tail::Unchanged => false,
        }
    }
    while !writer.is_finished() {
        poll(&mut reader, &source, &mut seen, &mut polls);
        // The loop stays hot, which is the arm E2 measured, but it does not hold a core away from
        // the writer on a machine already running the rest of the suite.
        std::thread::yield_now();
    }
    let failures = writer.join().expect("the writer thread");
    // Then drain what landed after the last poll, to the point where the store says there is
    // nothing left. The cap is only so a reader that stopped advancing fails instead of spinning.
    while poll(&mut reader, &source, &mut seen, &mut polls) {
        assert!(polls < 100_000, "the drain must converge, not spin");
    }
    assert_eq!(
        failures, 0,
        "a held reader must not fail the writer's commits ({seen} rows seen over {polls} polls)"
    );
    assert_eq!(
        seen, APPENDS,
        "the reader must see every row: {seen} of {APPENDS} over {polls} polls, \
         {failures} writer failures"
    );
}

/// A-246: a read-only open succeeds against a database whose `-wal` is stale and whose `-shm` is
/// gone, and again when neither the file nor its directory is writable. The rows that live only in
/// the WAL come back both times.
#[test]
fn a_read_only_open_recovers_a_stale_wal_without_an_shm() {
    let scratch = Scratch::new("opencode-stale-wal");
    let live = scratch.join("live/opencode.db");
    let store = Fixture::create(&live);
    // No checkpoint, so the rows below stay in the -wal rather than reaching the database file.
    store
        .conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .expect("no autocheckpoint");
    store.conversation(SESSION);
    assert!(
        std::fs::metadata(live.with_extension("db-wal"))
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "the fixture must leave rows in the -wal"
    );

    // The copy is what a crashed opencode leaves behind: a database, a stale -wal, no -shm.
    let crashed_dir = scratch.join("crashed");
    std::fs::create_dir_all(&crashed_dir).expect("the crashed directory");
    let crashed = crashed_dir.join("opencode.db");
    std::fs::copy(&live, &crashed).expect("copy the database");
    std::fs::copy(
        live.with_extension("db-wal"),
        crashed.with_extension("db-wal"),
    )
    .expect("copy the wal");
    assert!(
        !crashed.with_extension("db-shm").exists(),
        "the -shm must not have been copied"
    );

    let read = |path: &Path| -> usize {
        let source = Source {
            store: Store::OpenCode,
            path: path.to_path_buf(),
            session: Some(SESSION.to_string()),
        };
        Reader::new()
            .window(&source, &WindowRequest::new(1_000))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .events
            .len()
    };
    let want = read(&live);
    assert!(
        want > 0,
        "the live store must have events to compare against"
    );
    assert_eq!(read(&crashed), want, "the stale wal's rows must be visible");

    // And again with nothing writable. Restored immediately so the scratch directory can be
    // removed even if a later assertion fails.
    let mode = |path: &Path, bits: u32| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .expect("set the permissions");
    };
    mode(&crashed, 0o444);
    mode(&crashed.with_extension("db-wal"), 0o444);
    mode(&crashed_dir, 0o555);
    let events = std::panic::catch_unwind(|| read(&crashed));
    mode(&crashed_dir, 0o755);
    mode(&crashed, 0o644);
    mode(&crashed.with_extension("db-wal"), 0o644);
    assert_eq!(
        events.expect("a read-only file in a read-only directory must still open"),
        want
    );
}

/// A-246's other half, and A-245's. The connection string never asks SQLite to assume there is no
/// writer (that would be `immutable=1`, and there *is* a writer), and nothing in the reader shells
/// out: opencode transcripts need no external binary on `PATH`, which is what linking SQLite rather
/// than driving a `sqlite3` child buys. A-244's co-process framing has nothing left to go wrong.
#[test]
fn the_reader_asks_for_no_binary_and_no_immutable_database() {
    for source in [
        include_str!("../opencode.rs"),
        include_str!("../adapters/opencode.rs"),
    ] {
        assert!(
            !source.contains("immutable"),
            "a connection string must never claim the database has no writer"
        );
        for spawned in ["Command", "std::process", "sqlite3"] {
            assert!(
                !source.contains(spawned),
                "the reader must not reach for {spawned}: it links SQLite, it does not drive one"
            );
        }
    }

    // The functional half: a store reads with nothing but this process.
    let scratch = Scratch::new("opencode-no-binary");
    let store = Fixture::create(&scratch.join("share/opencode/opencode.db"));
    store.conversation(SESSION);
    let page = Reader::new()
        .window(&store.source(SESSION), &WindowRequest::new(10))
        .expect("a window");
    assert!(!page.events.is_empty());
}

/// Paging backwards over a session covers it exactly once, and a cursor keeps addressing the same
/// event when it is fetched again for its body.
#[test]
fn paging_backwards_covers_the_session_exactly_once() {
    let scratch = Scratch::new("opencode-paging");
    let store = Fixture::create(&scratch.join("share/opencode/opencode.db"));
    store.session(SESSION, 1_000);
    for i in 0..120 {
        let (message, ts) = (format!("msg_{i:04}"), 2_000 + i as i64);
        store.message(&message, SESSION, ts, "assistant");
        // Three parts per message, so a page boundary lands inside a group.
        for p in 0..3 {
            store.part(
                &format!("prt_{i:04}_{p}"),
                &message,
                SESSION,
                ts,
                &format!(r##"{{"type":"text","text":"#{i}.{p}"}}"##),
            );
        }
    }

    let source = store.source(SESSION);
    let mut reader = Reader::new();
    let mut previews = Vec::new();
    let mut before = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 100, "paging must terminate");
        let page = reader
            .window(&source, &WindowRequest::new(7).before(before))
            .expect("a page");
        previews.extend(page.events.iter().map(|e| {
            e.preview
                .clone()
                .unwrap_or_else(|| panic!("every text part has a preview"))
        }));
        match page.older {
            Some(older) => before = Some(older),
            None => break,
        }
    }
    assert!(pages > 10, "7 at a time over 360 events is many pages");
    previews.reverse();
    let want: Vec<String> = (0..120)
        .flat_map(|i| (0..3).map(move |p| format!("#{i}.{p}")))
        .collect();
    assert_eq!(previews, want);

    // The body of one cursor comes back as the event that cursor addressed.
    let page = reader
        .window(&source, &WindowRequest::new(5))
        .expect("a page");
    let event = &page.events[1];
    let body = reader.body(&source, &event.cursor).expect("the body");
    assert_eq!(body.preview, event.preview);
    assert_eq!(
        body.body.expect("a body read carries one").as_str(),
        "#119.1"
    );
}

/// A cursor minted against another database is refused by name rather than resolved against
/// whatever now sits at that timestamp.
#[test]
fn a_cursor_from_another_database_is_refused() {
    let scratch = Scratch::new("opencode-cursor");
    let first = Fixture::create(&scratch.join("one/opencode.db"));
    first.conversation(SESSION);
    let second = Fixture::create(&scratch.join("two/opencode.db"));
    second.conversation(SESSION);

    let mut reader = Reader::new();
    let page = reader
        .window(&first.source(SESSION), &WindowRequest::new(3))
        .expect("a page");
    let cursor = page.older.expect("a resumable page");
    let other = second.source(SESSION);
    assert!(matches!(
        reader.window(&other, &WindowRequest::new(3).before(Some(cursor))),
        Err(Refusal::CursorInvalid)
    ));
    assert!(matches!(
        reader.body(&other, &cursor),
        Err(Refusal::CursorInvalid)
    ));
}

/// A pre-cutover session has no event log at all, and is served from `message` and `part` alone:
/// the window is identical, and the tail falls back to walking the messages forward.
#[test]
fn a_session_with_no_event_log_is_still_served() {
    let scratch = Scratch::new("opencode-precutover");
    let store = Fixture::create(&scratch.join("share/opencode/opencode.db")).without_event_log();
    store.session(SESSION, 1_000);
    store.message("msg_0001", SESSION, 1_000, "user");
    store
        .conn
        .execute(
            "insert into part values ('prt_0001', 'msg_0001', ?1, 1000, 1000, ?2)",
            params![SESSION, r#"{"type":"text","text":"pre-cutover prompt"}"#],
        )
        .expect("insert the part");

    let source = store.source(SESSION);
    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10).with_bodies())
        .expect("a window over a pre-cutover session");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.unknown, 0);

    let Tail::Fresh { events, .. } = reader.tail(&source, &Budget::default()).expect("a tail")
    else {
        panic!("the first poll of a session has its history to send");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        reader.tail(&source, &Budget::default()),
        Ok(Tail::Unchanged)
    ));
}

/// Discovery resolves an opencode pane to the one database, from the `ses_*` id the plugin stamps.
/// Without that id there is nothing to open, and the refusal says so.
#[test]
fn an_opencode_pane_resolves_to_the_store_database() {
    let scratch = Scratch::new("opencode-discovery");
    let roots = StoreRoots::under(scratch.path());
    let store = Fixture::create(&roots.opencode.join("opencode.db"));
    store.conversation(SESSION);

    let facts = PaneFacts {
        agent: "opencode".into(),
        session: Some(SESSION.into()),
        ..Default::default()
    };
    let found = discovery::discover(&facts, &roots).expect("the store must be found");
    assert_eq!(found.store, Store::OpenCode);
    assert_eq!(found.path, roots.opencode.join("opencode.db"));
    assert_eq!(found.session.as_deref(), Some(SESSION));

    let no_session = PaneFacts {
        agent: "opencode".into(),
        ..Default::default()
    };
    assert!(matches!(
        discovery::discover(&no_session, &roots),
        Err(Refusal::NoTranscript)
    ));
}
