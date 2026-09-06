//! The paging, cursor and budget behaviour: the half of the crate that has to be right before it
//! is allowed to be fast.

use std::fmt::Write as _;

use super::Scratch;
use crate::model::{Budget, Cursor, EventKind, Store};
use crate::{Reader, Refusal, Source, Tail, WindowRequest};

/// A claude-shaped file of `n` records, each padded to roughly `pad` bytes. The user text is the
/// record's own index, so a paged read can be checked against `0..n` exactly.
fn numbered(scratch: &Scratch, name: &str, n: usize, pad: usize) -> Source {
    let mut body = String::new();
    for i in 0..n {
        let filler = "x".repeat(pad);
        let _ = writeln!(
            body,
            r##"{{"type":"user","sessionId":"s-numbered","cwd":"/synthetic","version":"2.1.236","timestamp":"2026-01-01T00:00:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"#{i} {filler}"}}]}}}}"##
        );
    }
    Source {
        store: Store::Claude,
        path: scratch.write(name, &body),
        session: None,
    }
}

/// The record index a numbered event's preview carries.
fn index_of(event: &crate::Event) -> usize {
    let preview = event
        .preview
        .as_deref()
        .expect("a numbered record has a body");
    preview
        .strip_prefix('#')
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unparsable preview {preview:?}"))
}

/// A-232: paging backwards from `older` reaches the head with no duplicated and no skipped event.
#[test]
fn paging_backwards_covers_the_file_exactly_once() {
    let scratch = Scratch::new("paging");
    let source = numbered(&scratch, "session.jsonl", 500, 200);
    let mut reader = Reader::new();

    let mut seen = Vec::new();
    let mut before: Option<Cursor> = None;
    let mut pages = 0;
    loop {
        let page = reader
            .window(&source, &WindowRequest::new(37).before(before))
            .expect("a page");
        pages += 1;
        assert!(pages < 100, "paging must terminate");
        seen.extend(page.events.iter().map(index_of));
        match page.older {
            Some(older) => before = Some(older),
            None => break,
        }
    }
    assert!(pages > 10, "37 at a time over 500 records is many pages");
    seen.reverse();
    assert_eq!(seen, (0..500).collect::<Vec<_>>());
}

/// The same walk over a file whose records each yield several events, so the `before` cursor lands
/// mid-record: the earlier halves of that record must come back on the next page, not be skipped.
#[test]
fn paging_is_exact_across_multi_event_records() {
    let scratch = Scratch::new("multipart");
    let mut body = String::new();
    for i in 0..60 {
        let _ = writeln!(
            body,
            r#"{{"type":"assistant","sessionId":"s","version":"2.1.236","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"t{i}"}},{{"type":"text","text":"a{i}"}},{{"type":"tool_use","id":"c{i}","name":"Bash","input":{{"command":"x"}}}}]}}}}"#
        );
    }
    let source = Source {
        store: Store::Claude,
        path: scratch.write("multi.jsonl", &body),
        session: None,
    };
    let mut reader = Reader::new();

    let mut labels = Vec::new();
    let mut before = None;
    loop {
        // A page size that is not a multiple of three forces a mid-record boundary every page.
        let page = reader
            .window(&source, &WindowRequest::new(7).before(before))
            .expect("a page");
        labels.extend(page.events.iter().map(|e| e.kind.label()));
        match page.older {
            Some(older) => before = Some(older),
            None => break,
        }
    }
    assert_eq!(labels.len(), 180);
    labels.reverse();
    let want: Vec<&str> = (0..60)
        .flat_map(|_| ["thinking", "assistant_text", "tool_call"])
        .collect();
    assert_eq!(labels, want);
}

/// A-229 and A-230: a 44 MiB-class file with 78 KiB records is served in bounded chunks. The read
/// budget bites before 200 events do, so the page is short, flagged, and has a usable `older`.
#[test]
fn a_large_file_is_served_in_bounded_chunks() {
    let scratch = Scratch::new("large");
    // 78 KiB per record against a 45 MiB file: the shape the spike measured for a claude session.
    let source = numbered(&scratch, "big.jsonl", 600, 78 * 1024);
    let size = std::fs::metadata(&source.path).unwrap().len();
    assert!(size > 44 * 1024 * 1024, "the fixture must be 44 MiB-class");

    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(200))
        .expect("a page");
    assert!(page.budget_truncated, "the read budget must bite first");
    assert!(page.events.len() < 200, "a short page, not a whole file");
    assert!(!page.events.is_empty());
    assert!(page.older.is_some(), "a truncated page must be resumable");
    assert!(
        reader.read_calls() < 40,
        "the window must not walk the file: {} reads",
        reader.read_calls()
    );

    // A-230: the headers of a full 200-event page stay inside the frame budget. The spike measured
    // 94 to 109 bytes per event across a 25x range of file sizes; 200 of those plus a preview each
    // is what the 32 KiB has to cover.
    let small = numbered(&scratch, "many.jsonl", 400, 20);
    let page = reader
        .window(&small, &WindowRequest::new(200))
        .expect("a page");
    assert_eq!(page.events.len(), 200);
    let frame: usize = page.events.iter().map(|e| e.header_cost()).sum();
    assert!(
        frame <= Budget::DEFAULT_FRAME_BYTES,
        "200 headers measured {frame} bytes, over the 32 KiB frame budget"
    );
}

/// A-231: a window carries no bodies and no string leaf past the header budget, and the body of one
/// cursor comes back in full on a second call.
#[test]
fn a_window_carries_headers_and_a_body_comes_back_by_cursor() {
    let scratch = Scratch::new("headers");
    let long = "y".repeat(4000);
    let body = format!(
        r#"{{"type":"assistant","sessionId":"s","version":"2.1.236","message":{{"role":"assistant","content":[{{"type":"text","text":"first line {long}\nsecond line"}}]}}}}"#
    );
    let source = Source {
        store: Store::Claude,
        path: scratch.write("long.jsonl", &format!("{body}\n")),
        session: None,
    };
    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10))
        .expect("a page");
    assert_eq!(page.events.len(), 1);
    let header = &page.events[0];
    assert_eq!(header.body, None, "a window frame carries no bodies");
    assert!(
        header.preview.as_ref().unwrap().len() <= Budget::DEFAULT_HEADER_BYTES,
        "no string leaf may exceed the header budget"
    );

    let full = reader.body(&source, &header.cursor).expect("the body");
    let text = full.body.expect("the body is present on a body read");
    assert!(text.as_str().starts_with("first line yyyy"));
    assert!(text.as_str().ends_with("second line"));
    assert_eq!(text.as_str().len(), 4000 + "first line \nsecond line".len());
}

/// The header budget is configurable, and every string leaf honours it, not just the preview.
#[test]
fn the_header_budget_is_configurable() {
    let scratch = Scratch::new("budget");
    let long_name = "N".repeat(300);
    let body = format!(
        r#"{{"type":"assistant","sessionId":"s","version":"2.1.236","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{long_name}","name":"{long_name}","input":{{"command":"x"}}}}]}}}}"#
    );
    let source = Source {
        store: Store::Claude,
        path: scratch.write("tool.jsonl", &format!("{body}\n")),
        session: None,
    };
    let mut reader = Reader::new();
    for cap in [16usize, 64, 256] {
        let req = WindowRequest::new(10).budget(Budget {
            header_bytes: cap,
            ..Budget::default()
        });
        let page = reader.window(&source, &req).expect("a page");
        match &page.events[0].kind {
            EventKind::ToolCall { name, call_id, .. } => {
                assert_eq!(name.len(), cap);
                assert_eq!(call_id.as_ref().unwrap().len(), cap);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert!(page.events[0].preview.as_ref().unwrap().len() <= cap);
    }
}

/// A-233: a cursor whose file was rewritten, truncated, or replaced is refused by name. Never a
/// parse of whatever now sits at that offset.
#[test]
fn a_stale_cursor_is_refused_rather_than_mis_parsed() {
    let scratch = Scratch::new("stale");
    let source = numbered(&scratch, "session.jsonl", 40, 100);
    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10))
        .expect("a page");
    let cursor = page.older.expect("a resumable page");

    // Truncation: the same inode, fewer bytes.
    let kept: String = std::fs::read_to_string(&source.path)
        .unwrap()
        .lines()
        .take(5)
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&source.path, &kept).unwrap();
    assert!(matches!(
        reader.window(&source, &WindowRequest::new(10).before(Some(cursor))),
        Err(Refusal::CursorInvalid)
    ));
    assert!(matches!(
        reader.body(&source, &cursor),
        Err(Refusal::CursorInvalid)
    ));

    // Replacement: a new inode at the same path, long enough that the offset is still inside it.
    let replacement = numbered(&scratch, "replacement.jsonl", 40, 100);
    std::fs::rename(&replacement.path, &source.path).unwrap();
    assert!(matches!(
        reader.window(&source, &WindowRequest::new(10).before(Some(cursor))),
        Err(Refusal::CursorInvalid)
    ));

    // A fresh end-anchored window still works: the refusal is about the cursor, not the file.
    let fresh = reader
        .window(&source, &WindowRequest::new(10))
        .expect("a fresh window after the rewrite");
    assert_eq!(fresh.events.len(), 10);
}

/// The forward tail keeps the codex tail's memo and short-circuit: an unchanged file costs one stat
/// and no read, and a rewrite restarts rather than resuming into bytes that moved.
#[test]
fn the_forward_tail_memoizes_and_restarts_on_a_rewrite() {
    let scratch = Scratch::new("tail");
    let source = numbered(&scratch, "session.jsonl", 5, 40);
    let mut reader = Reader::new();
    let budget = Budget::default();

    let Tail::Fresh { events, .. } = reader.tail(&source, &budget).unwrap() else {
        panic!("the first poll must read");
    };
    assert_eq!(events.len(), 5);
    let reads = reader.read_calls();
    assert_eq!(reader.tail(&source, &budget).unwrap(), Tail::Unchanged);
    assert_eq!(
        reader.read_calls(),
        reads,
        "an unchanged file is not re-read"
    );
    assert_eq!(reader.stat_calls(), 2, "steady state is one stat per poll");

    // Append: only the new records come back.
    let extra = numbered(&scratch, "extra.jsonl", 3, 40);
    let appended = std::fs::read_to_string(&extra.path).unwrap();
    let mut existing = std::fs::read_to_string(&source.path).unwrap();
    existing.push_str(&appended);
    std::fs::write(&source.path, &existing).unwrap();
    let Tail::Fresh {
        events, restarted, ..
    } = reader.tail(&source, &budget).unwrap()
    else {
        panic!("an appended file must read");
    };
    assert_eq!(events.len(), 3);
    assert!(!restarted);

    // Truncation to fewer bytes than the stored offset: start over and say so.
    std::fs::write(&source.path, "").unwrap();
    let appended_again = numbered(&scratch, "again.jsonl", 2, 40);
    std::fs::copy(&appended_again.path, &source.path).unwrap();
    let Tail::Fresh {
        events, restarted, ..
    } = reader.tail(&source, &budget).unwrap()
    else {
        panic!("a rewritten file must read");
    };
    assert!(restarted, "a shrunk file restarts the tail");
    assert_eq!(events.len(), 2);
}

/// A half-written last line is never half-parsed: the tail leaves it for the next poll and the
/// window drops it, the way the codex tail's `clean_window` does.
#[test]
fn a_record_caught_mid_write_is_left_alone() {
    let scratch = Scratch::new("partial");
    let source = numbered(&scratch, "session.jsonl", 4, 40);
    let mut whole = std::fs::read_to_string(&source.path).unwrap();
    whole.push_str(r#"{"type":"user","sessionId":"s","mess"#);
    std::fs::write(&source.path, &whole).unwrap();

    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10))
        .expect("a page");
    assert_eq!(page.events.len(), 4, "the partial line is not an event");
    assert_eq!(page.unknown, 0, "and it is not a hole either");

    let Tail::Fresh { events, .. } = reader.tail(&source, &Budget::default()).unwrap() else {
        panic!("the first poll must read");
    };
    assert_eq!(events.len(), 4);
}

/// A-239 and the OpenCode half: a store the reader will not serve is a typed refusal that names
/// the reason, never an empty window that reads as "nothing happened".
#[test]
fn a_refused_store_names_its_reason() {
    let scratch = Scratch::new("refused");
    let source = Source {
        store: Store::Cursor,
        path: scratch.write("chat.jsonl", "{}\n"),
        session: None,
    };
    let err = Reader::new()
        .window(&source, &WindowRequest::new(10))
        .expect_err("a refusal");
    assert_eq!(err.code(), "store-incomplete");
    assert!(
        err.to_string().contains(Store::Cursor.as_str()),
        "the refusal must name the store: {err}"
    );
}

/// The other half, on a build with the SQLite reader compiled out: an OpenCode pane is refused by
/// name rather than served an empty window, the same as cursor-agent is.
#[test]
#[cfg(not(feature = "opencode"))]
fn opencode_is_refused_by_name_without_its_reader() {
    let scratch = Scratch::new("refused-opencode");
    let source = Source {
        store: Store::OpenCode,
        path: scratch.write("opencode.db", ""),
        session: Some("ses_x".into()),
    };
    let err = Reader::new()
        .window(&source, &WindowRequest::new(10))
        .expect_err("a refusal");
    assert_eq!(err.code(), "unsupported-store");
    assert!(
        err.to_string().contains(Store::OpenCode.as_str()),
        "the refusal must name the store: {err}"
    );
}

/// Discovery prefers the stamped path, falls back to each store's layout, and refuses by name when
/// neither holds anything.
#[test]
fn discovery_prefers_the_stamp_then_walks_the_store() {
    use crate::discovery::{discover, PaneFacts, StoreRoots};

    let scratch = Scratch::new("discovery");
    let roots = StoreRoots::under(scratch.path());
    let session = "0000-session";

    let stamped = scratch.write("elsewhere/stamped.jsonl", "{}\n");
    let facts = PaneFacts {
        agent: "claude".into(),
        session: Some(session.into()),
        transcript: Some(stamped.display().to_string()),
        cwd: None,
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, stamped);

    // No stamp: the layout walk finds it under the cwd slug.
    let laid_out = scratch.write(
        &format!(".claude/projects/-synthetic-workdir/{session}.jsonl"),
        "{}\n",
    );
    let facts = PaneFacts {
        transcript: None,
        cwd: Some("/synthetic/workdir".into()),
        ..facts
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, laid_out);

    // A stamp pointing at a file that is gone falls through to the walk rather than failing.
    let facts = PaneFacts {
        transcript: Some(scratch.join("gone.jsonl").display().to_string()),
        ..facts
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, laid_out);

    // Codex's dated tree, walked newest-first by the session id in the filename.
    let rollout = scratch.write(
        &format!(".codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-{session}.jsonl"),
        "{}\n",
    );
    let facts = PaneFacts {
        agent: "codex".into(),
        session: Some(session.into()),
        transcript: None,
        cwd: None,
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, rollout);

    // Gemini's projectHash tree, matched on the header's own sessionId.
    let chat = scratch.write(
        ".gemini/tmp/0123456789abcdef/chats/session-2026-01-01-abcd1234.jsonl",
        &format!("{{\"kind\":\"main\",\"sessionId\":\"{session}\"}}\n"),
    );
    let facts = PaneFacts {
        agent: "gemini".into(),
        ..facts
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, chat);

    // pi's `--slug--` directory and `<iso>_<session>.jsonl` filename.
    let pi = scratch.write(
        &format!(".pi/agent/sessions/--synthetic-workdir--/2026-01-01_{session}.jsonl"),
        "{}\n",
    );
    let facts = PaneFacts {
        agent: "pi".into(),
        cwd: Some("/synthetic/workdir".into()),
        ..facts
    };
    assert_eq!(discover(&facts, &roots).unwrap().path, pi);

    // Nothing anywhere, and an agent with no store at all, are both named refusals.
    let facts = PaneFacts {
        agent: "claude".into(),
        session: Some("no-such-session".into()),
        transcript: None,
        cwd: None,
    };
    assert_eq!(
        discover(&facts, &roots).unwrap_err().code(),
        "no-transcript"
    );
    let facts = PaneFacts {
        agent: "aider".into(),
        ..facts
    };
    assert_eq!(
        discover(&facts, &roots).unwrap_err().code(),
        "no-transcript"
    );
}
