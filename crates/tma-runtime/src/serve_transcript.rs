//! The transcript reader, on the wire: [`tma_transcript`]'s events and cursors as
//! [`tma_proto`]'s, and its typed refusals as typed errors.
//!
//! Almost all of it is conversion, and that is deliberate. The reader's model carries no serde and
//! the wire's carries no I/O, so neither one leaks into the other and there is exactly one place
//! where a field of the first becomes a field of the second. What is not conversion is the two
//! rules a remote caller makes necessary:
//!
//! - **The device asks and the host clamps.** Every budget arrives from the far end of a pipe, so
//!   each field is capped at this host's own default rather than honoured: a frame budget is a
//!   promise to the network, and a caller cannot be allowed to raise it.
//! - **Cursors stay opaque.** A device only ever echoes back a token this host minted. The token is
//!   parseable here so a hand-edited one earns [`ErrorCode::CursorInvalid`] rather than a panic or,
//!   worse, a parse of whatever now sits at that offset.

use tma_proto::{
    Body, BodyKind, ErrorCode, ErrorFrame, EventHeader, EventKind, EventRequest, ResultStatus,
    SessionMeta, TurnKind, Window, WindowRequest,
};
use tma_transcript as tx;

/// Events per page when the request names no count. The measured window size behind R9's byte
/// bound: 200 headers came to 18.4 to 21.3 KiB across a 25x range of file sizes (E1).
pub const DEFAULT_LAST: u32 = 200;

/// The most a device may ask for in one page. The frame budget is the real bound on what crosses
/// the pipe; this bounds the scan behind it, so a `last` of four billion cannot walk a 44 MiB file.
pub const MAX_LAST: u32 = 1_000;

/// The pane whose transcript is being served, as the serve loop already resolved it.
#[derive(Clone, Copy, Debug)]
pub struct PaneTranscript<'a> {
    pub pane: &'a str,
    /// `@agent_name`, echoed on the window so a device need not remember which pane is which.
    pub agent: &'a str,
    /// The file discovery resolved for this pane.
    pub source: &'a tx::Source,
}

/// One page of headers, newest first.
pub fn window(
    reader: &mut tx::Reader,
    facts: &PaneTranscript,
    req: &WindowRequest,
) -> Result<Window, ErrorFrame> {
    let before = match &req.before {
        Some(cursor) => Some(parse_cursor(cursor.as_str())?),
        None => None,
    };
    let last = page_count(req.last);
    let page = reader
        .window(
            facts.source,
            &tx::WindowRequest::new(last)
                .before(before)
                .budget(budget(&req.budget)),
        )
        .map_err(|e| refusal(&e))?;
    Ok(Window {
        pane: facts.pane.to_string(),
        agent: facts.agent.to_string(),
        session: page.session.map(session),
        older: page.older.map(|c| c.to_string().into()),
        budget_truncated: page.budget_truncated,
        unknown: u32::try_from(page.unknown).unwrap_or(u32::MAX),
        events: page.events.into_iter().map(header).collect(),
    })
}

/// One event with its body. The body is not capped: a window read is what the header budget bounds,
/// and this is the request a device makes precisely because it wants the whole message.
pub fn event(
    reader: &mut tx::Reader,
    facts: &PaneTranscript,
    req: &EventRequest,
) -> Result<EventHeader, ErrorFrame> {
    let cursor = parse_cursor(req.cursor.as_str())?;
    let event = reader
        .body(facts.source, &cursor)
        .map_err(|e| refusal(&e))?;
    Ok(header(event))
}

/// A reader refusal as a typed error frame.
///
/// The two store refusals both map to `unsupported`, which is the accurate word for each: cursor's
/// file exists and is too thin to render, opencode's conversation is in a database this reader does
/// not open. Neither is a missing pane and neither is a caller mistake, so neither is `not-found`
/// or `bad-request`. An I/O failure is the host's own, so it is `internal` and the device may retry.
pub fn refusal(refusal: &tx::Refusal) -> ErrorFrame {
    let code = match refusal {
        tx::Refusal::NoTranscript => ErrorCode::NotFound,
        tx::Refusal::UnsupportedStore { .. } | tx::Refusal::StoreIncomplete { .. } => {
            ErrorCode::Unsupported
        }
        tx::Refusal::CursorInvalid => ErrorCode::CursorInvalid,
        // The record is real and this reader will not materialize it; a retry cannot help.
        tx::Refusal::RecordTooLarge(_) => ErrorCode::Unsupported,
        tx::Refusal::Io { .. } => ErrorCode::Internal,
    };
    ErrorFrame::new(code, refusal.to_string())
}

/// A device's cursor token, or the refusal a token this host did not mint earns.
fn parse_cursor(token: &str) -> Result<tx::Cursor, ErrorFrame> {
    token
        .parse::<tx::Cursor>()
        .map_err(|_| refusal(&tx::Refusal::CursorInvalid))
}

/// How many events one page returns: the request's count, defaulted and clamped. Zero is nobody's
/// request, so it reads as one rather than as an empty page a device would take for the end.
fn page_count(asked: Option<u32>) -> usize {
    asked.unwrap_or(DEFAULT_LAST).clamp(1, MAX_LAST) as usize
}

/// The requested budget, clamped at this host's own ceilings in every dimension.
fn budget(asked: &tma_proto::Budget) -> tx::Budget {
    tx::Budget {
        header_bytes: (asked.header_bytes as usize).min(tx::Budget::DEFAULT_HEADER_BYTES),
        read_bytes: asked.read_bytes.min(tx::Budget::DEFAULT_READ_BYTES),
        frame_bytes: (asked.frame_bytes as usize).min(tx::Budget::DEFAULT_FRAME_BYTES),
    }
}

fn header(event: tx::Event) -> EventHeader {
    EventHeader {
        cursor: event.cursor.to_string().into(),
        kind: kind(event.kind),
        ts: event.ts,
        preview: event.preview,
        body: event.body.map(body),
    }
}

fn body(value: tx::Body) -> Body {
    match value {
        tx::Body::Text(text) => Body {
            kind: BodyKind::Text,
            text,
        },
        tx::Body::Json(text) => Body {
            kind: BodyKind::Json,
            text,
        },
    }
}

fn session(meta: tx::SessionMeta) -> SessionMeta {
    SessionMeta {
        agent: meta.agent,
        session_id: meta.session_id,
        cwd: meta.cwd,
        version: meta.version,
        model: meta.model,
    }
}

/// The one place a reader event kind becomes a wire event kind. Exhaustive, so a kind added to
/// either side has to be answered for here rather than degrading to `unknown` in silence.
fn kind(kind: tx::EventKind) -> EventKind {
    match kind {
        tx::EventKind::SessionMeta(meta) => EventKind::SessionMeta(session(meta)),
        tx::EventKind::UserMessage { bytes, attachments } => EventKind::UserMessage {
            bytes: bytes as u64,
            attachments: attachments as u32,
        },
        tx::EventKind::AssistantText { bytes } => EventKind::AssistantText {
            bytes: bytes as u64,
        },
        tx::EventKind::Thinking { bytes, redacted } => EventKind::Thinking {
            bytes: bytes as u64,
            redacted,
        },
        tx::EventKind::ToolCall {
            name,
            call_id,
            arg_keys,
            bytes,
        } => EventKind::ToolCall {
            name,
            call_id,
            arg_keys,
            bytes: bytes as u64,
        },
        tx::EventKind::ToolResult {
            call_id,
            status,
            bytes,
        } => EventKind::ToolResult {
            call_id,
            status: match status {
                tx::ResultStatus::Ok => ResultStatus::Ok,
                tx::ResultStatus::Error => ResultStatus::Error,
                tx::ResultStatus::Pending => ResultStatus::Pending,
                tx::ResultStatus::Unknown => ResultStatus::Unknown,
            },
            bytes: bytes as u64,
        },
        tx::EventKind::PermissionRequest { tool, call_id } => {
            EventKind::PermissionRequest { tool, call_id }
        }
        tx::EventKind::TurnBoundary { kind, reason } => EventKind::TurnBoundary {
            boundary: match kind {
                tx::TurnKind::Start => TurnKind::Start,
                tx::TurnKind::End => TurnKind::End,
            },
            reason,
        },
        tx::EventKind::Usage {
            input,
            output,
            total,
            context_window,
            cost_usd,
        } => EventKind::Usage {
            input,
            output,
            total,
            context_window,
            cost_usd,
        },
        tx::EventKind::SubagentRef {
            child_id,
            external_file,
        } => EventKind::SubagentRef {
            child_id,
            external_file,
        },
        tx::EventKind::Compaction { kind } => EventKind::Compaction { compaction: kind },
        tx::EventKind::Attachment { kind, bytes } => EventKind::Attachment {
            attachment: kind,
            bytes: bytes as u64,
        },
        tx::EventKind::Bookkeeping { type_name } => EventKind::Bookkeeping { type_name },
        tx::EventKind::Unknown { type_name } => EventKind::Unknown { type_name },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A claude-shaped synthetic transcript, in the grammar the reader's own fixture speaks:
    /// claude writes no header record, so every line carries the session envelope, and `count`
    /// user/assistant turns each end in a tool call and its result. `filler` pads the prose, which
    /// is what makes a page hit the frame budget rather than the event count.
    fn write_fixture(path: &Path, count: usize, filler: usize) {
        const ENVELOPE: &str = "\"sessionId\":\"00000000-0000-0000-0000-0000000cl001\",\
                                \"cwd\":\"/synthetic/workdir\",\"version\":\"2.1.236\",\
                                \"timestamp\":\"2026-01-01T00:00:01.000Z\"";
        let pad = "p".repeat(filler);
        let mut out = String::new();
        for i in 0..count {
            out.push_str(&format!(
                "{{\"type\":\"user\",{ENVELOPE},\
                 \"message\":{{\"role\":\"user\",\"content\":\"m{i} {pad}\"}}}}\n"
            ));
            out.push_str(&format!(
                "{{\"type\":\"assistant\",{ENVELOPE},\
                 \"message\":{{\"role\":\"assistant\",\"content\":[\
                 {{\"type\":\"text\",\"text\":\"a{i} {pad}\"}},\
                 {{\"type\":\"tool_use\",\"id\":\"toolu_{i}\",\"name\":\"Bash\",\
                 \"input\":{{\"command\":\"echo {i}\"}}}}]}}}}\n"
            ));
            out.push_str(&format!(
                "{{\"type\":\"user\",{ENVELOPE},\
                 \"message\":{{\"role\":\"user\",\"content\":[\
                 {{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_{i}\",\
                 \"is_error\":false,\"content\":\"ok {pad}\"}}]}}}}\n"
            ));
        }
        std::fs::write(path, out).expect("write the fixture");
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "tma-serve-transcript-{tag}-{}-{}",
                std::process::id(),
                crate::now_ms()
            ));
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            Scratch(dir)
        }

        /// A readable claude source over a fresh fixture.
        fn source(&self, count: usize, filler: usize) -> tx::Source {
            let path = self.0.join("session.jsonl");
            write_fixture(&path, count, filler);
            tx::Source {
                store: tx::Store::Claude,
                path,
                session: None,
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn facts(source: &tx::Source) -> PaneTranscript<'_> {
        PaneTranscript {
            pane: "%1",
            agent: "claude",
            source,
        }
    }

    fn request(last: Option<u32>) -> WindowRequest {
        WindowRequest {
            pane: "%1".to_string(),
            last,
            before: None,
            budget: tma_proto::Budget::default(),
        }
    }

    /// Every kind the reader can emit reaches the wire as its own variant. The fixture covers the
    /// six a claude session writes; the rest are pinned by the exhaustive conversion below.
    #[test]
    fn a_window_converts_the_reader_page_to_the_wire() {
        let scratch = Scratch::new("convert");
        let source = scratch.source(2, 0);
        let page = window(&mut tx::Reader::new(), &facts(&source), &request(None))
            .expect("the fixture is readable");

        assert_eq!((page.pane.as_str(), page.agent.as_str()), ("%1", "claude"));
        let meta = page.session.expect("the head record is the session header");
        assert_eq!(meta.version.as_deref(), Some("2.1.236"));
        assert_eq!(page.unknown, 0, "every record maps");
        assert_eq!(page.older, None, "the head is in this page");
        assert!(!page.budget_truncated);

        let kinds: Vec<&str> = page.events.iter().map(|e| label(&e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                "tool_result",
                "tool_call",
                "assistant_text",
                "user_message",
                "tool_result",
                "tool_call",
                "assistant_text",
                "user_message",
            ],
            "newest first"
        );
        assert!(
            page.events.iter().all(|e| e.body.is_none()),
            "a window read carries no bodies"
        );
        match &page.events[1].kind {
            EventKind::ToolCall { name, call_id, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(call_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// Each of the fourteen wire kinds is produced by the conversion and by nothing else, so a
    /// kind added on either side fails to compile here rather than landing as `unknown`.
    #[test]
    fn every_reader_kind_has_its_own_wire_kind() {
        let cases = [
            (
                tx::EventKind::SessionMeta(tx::SessionMeta::default()),
                "session_meta",
            ),
            (
                tx::EventKind::UserMessage {
                    bytes: 1,
                    attachments: 2,
                },
                "user_message",
            ),
            (tx::EventKind::AssistantText { bytes: 3 }, "assistant_text"),
            (
                tx::EventKind::Thinking {
                    bytes: 4,
                    redacted: true,
                },
                "thinking",
            ),
            (
                tx::EventKind::ToolCall {
                    name: "Bash".into(),
                    call_id: Some("c".into()),
                    arg_keys: vec!["command".into()],
                    bytes: 5,
                },
                "tool_call",
            ),
            (
                tx::EventKind::ToolResult {
                    call_id: Some("c".into()),
                    status: tx::ResultStatus::Pending,
                    bytes: 6,
                },
                "tool_result",
            ),
            (
                tx::EventKind::PermissionRequest {
                    tool: "Bash".into(),
                    call_id: None,
                },
                "permission_request",
            ),
            (
                tx::EventKind::TurnBoundary {
                    kind: tx::TurnKind::End,
                    reason: Some("stop".into()),
                },
                "turn_boundary",
            ),
            (
                tx::EventKind::Usage {
                    input: Some(7),
                    output: None,
                    total: None,
                    context_window: None,
                    cost_usd: Some(0.25),
                },
                "usage",
            ),
            (
                tx::EventKind::SubagentRef {
                    child_id: "child".into(),
                    external_file: true,
                },
                "subagent_ref",
            ),
            (
                tx::EventKind::Compaction {
                    kind: "manual".into(),
                },
                "compaction",
            ),
            (
                tx::EventKind::Attachment {
                    kind: "image".into(),
                    bytes: 8,
                },
                "attachment",
            ),
            (
                tx::EventKind::Bookkeeping {
                    type_name: "set".into(),
                },
                "bookkeeping",
            ),
            (
                tx::EventKind::Unknown {
                    type_name: "novel".into(),
                },
                "unknown",
            ),
        ];
        for (from, want) in cases {
            assert_eq!(label(&kind(from)), want);
        }

        // The three renames are the ones a careless conversion would silently swap.
        assert!(matches!(
            kind(tx::EventKind::Compaction { kind: "m".into() }),
            EventKind::Compaction { compaction } if compaction == "m"
        ));
        assert!(matches!(
            kind(tx::EventKind::Attachment { kind: "i".into(), bytes: 1 }),
            EventKind::Attachment { attachment, .. } if attachment == "i"
        ));
        assert!(matches!(
            kind(tx::EventKind::TurnBoundary {
                kind: tx::TurnKind::Start,
                reason: None
            }),
            EventKind::TurnBoundary {
                boundary: TurnKind::Start,
                ..
            }
        ));
    }

    /// A-229 and A-230's conversion half: a frame budget that runs out ends the page with the flag
    /// up and an `older` cursor that pages on, rather than a short page that reads as the end.
    #[test]
    fn the_frame_budget_truncates_and_leaves_a_usable_cursor() {
        let scratch = Scratch::new("budget");
        let source = scratch.source(40, 200);
        let mut reader = tx::Reader::new();
        let req = WindowRequest {
            budget: tma_proto::Budget {
                frame_bytes: 512,
                ..tma_proto::Budget::default()
            },
            ..request(Some(200))
        };
        let page = window(&mut reader, &facts(&source), &req).expect("readable");
        assert!(page.budget_truncated, "the byte budget ended the scan");
        assert!(page.events.len() < 120, "not the whole file");
        let older = page.older.expect("there is more behind this page");

        // The cursor pages on, and the next page starts strictly older than the first ended.
        let next = window(
            &mut reader,
            &facts(&source),
            &WindowRequest {
                before: Some(older.clone()),
                ..req.clone()
            },
        )
        .expect("readable");
        assert!(!next.events.is_empty());
        assert_ne!(next.events[0].cursor, older);
        assert_eq!(next.session, None, "the header rides the first page only");
    }

    /// The device asks and the host clamps: a budget above this host's ceilings is cut to them,
    /// and one below is honoured as asked.
    #[test]
    fn a_budget_is_clamped_in_every_dimension() {
        let asked = tma_proto::Budget {
            header_bytes: u32::MAX,
            read_bytes: u64::MAX,
            frame_bytes: u32::MAX,
        };
        let got = budget(&asked);
        assert_eq!(got.header_bytes, tx::Budget::DEFAULT_HEADER_BYTES);
        assert_eq!(got.read_bytes, tx::Budget::DEFAULT_READ_BYTES);
        assert_eq!(got.frame_bytes, tx::Budget::DEFAULT_FRAME_BYTES);

        let small = budget(&tma_proto::Budget {
            header_bytes: 16,
            read_bytes: 4096,
            frame_bytes: 1024,
        });
        assert_eq!(
            (small.header_bytes, small.read_bytes, small.frame_bytes),
            (16, 4096, 1024)
        );
    }

    /// `last` is defaulted and clamped the same way a budget is, and an honoured count comes back
    /// exactly. The ceiling is not observable end to end on purpose: the frame budget bites first,
    /// which is the point of having both.
    #[test]
    fn the_page_count_is_bounded_and_defaulted() {
        assert_eq!(page_count(None), DEFAULT_LAST as usize);
        assert_eq!(
            page_count(Some(0)),
            1,
            "a zero-event page is nobody's request"
        );
        assert_eq!(page_count(Some(u32::MAX)), MAX_LAST as usize);
        assert_eq!(page_count(Some(37)), 37);

        let scratch = Scratch::new("count");
        let source = scratch.source(20, 0);
        let page =
            window(&mut tx::Reader::new(), &facts(&source), &request(Some(3))).expect("readable");
        assert_eq!(page.events.len(), 3);
        assert!(page.older.is_some(), "a short page still pages on");
    }

    /// An event read returns the one event with its body; a window read returned it without.
    #[test]
    fn an_event_read_carries_the_body() {
        let scratch = Scratch::new("body");
        let source = scratch.source(1, 0);
        let mut reader = tx::Reader::new();
        let page = window(&mut reader, &facts(&source), &request(None)).expect("readable");
        let call = page
            .events
            .iter()
            .find(|e| matches!(e.kind, EventKind::ToolCall { .. }))
            .expect("the fixture has a tool call");

        let got = event(
            &mut reader,
            &facts(&source),
            &EventRequest {
                pane: "%1".to_string(),
                cursor: call.cursor.clone(),
            },
        )
        .expect("the cursor is live");
        let body = got.body.expect("a body read populates it");
        assert_eq!(body.kind, BodyKind::Json);
        assert!(body.text.contains("echo 0"), "{}", body.text);
        assert_eq!(got.cursor, call.cursor);
    }

    /// A-233's conversion half. Three ways a cursor stops addressing its bytes, and a fourth a
    /// device could type by hand, all one code the app can act on: drop it and re-anchor.
    #[test]
    fn a_stale_or_forged_cursor_is_one_typed_code() {
        let scratch = Scratch::new("stale");
        let source = scratch.source(4, 0);
        let mut reader = tx::Reader::new();
        let page = window(&mut reader, &facts(&source), &request(None)).expect("readable");
        let cursor = page.events[0].cursor.clone();

        // Rewritten shorter: the size half of the cursor no longer holds.
        write_fixture(&source.path, 1, 0);
        let err = event(
            &mut reader,
            &facts(&source),
            &EventRequest {
                pane: "%1".to_string(),
                cursor: cursor.clone(),
            },
        )
        .expect_err("the file was rewritten");
        assert_eq!(err.code, ErrorCode::CursorInvalid);
        assert_eq!(err.code.token(), "cursor-invalid");

        let err = window(
            &mut reader,
            &facts(&source),
            &WindowRequest {
                before: Some(cursor),
                ..request(None)
            },
        )
        .expect_err("paging from it fails the same way");
        assert_eq!(err.code, ErrorCode::CursorInvalid);

        for forged in ["", "t2.1.1.1.1.1", "t1.z.1.1.1.1", "../etc/passwd"] {
            let err = window(
                &mut reader,
                &facts(&source),
                &WindowRequest {
                    before: Some(forged.to_string().into()),
                    ..request(None)
                },
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::CursorInvalid, "{forged:?}");
        }
    }

    /// A store the reader will not serve becomes a typed refusal carrying the reader's own
    /// sentence. An empty window would read as "nothing happened", which is the failure R20 names.
    ///
    /// OpenCode's answer is a build fact, and the test asks the reader rather than assuming one:
    /// without the SQLite reader compiled in the store itself is refused `unsupported`, and with it
    /// (the binary's own build, so also a whole-workspace test run) an absent database is a missing
    /// transcript. Typed either way, which is the property, and never an empty page.
    #[test]
    fn a_refused_store_is_a_typed_error_not_an_empty_window() {
        let scratch = Scratch::new("stores");
        for (store, needle) in [
            (tx::Store::Cursor, "would render as holes"),
            (tx::Store::OpenCode, "SQLite"),
        ] {
            let source = tx::Source {
                store,
                path: scratch.0.join("nothing.jsonl"),
                session: None,
            };
            let err = window(&mut tx::Reader::new(), &facts(&source), &request(None))
                .expect_err("this store is refused");
            if store.is_readable() {
                assert_eq!(err.code, ErrorCode::NotFound, "{store}");
                continue;
            }
            assert_eq!(err.code, ErrorCode::Unsupported, "{store}");
            assert!(err.message.contains(needle), "{}", err.message);
        }
    }

    /// The rest of the refusal set, mapped where a device can act on it: a pane with no transcript
    /// is `not-found`, and the host's own I/O failure is `internal` so a device may retry.
    #[test]
    fn the_remaining_refusals_map_to_actionable_codes() {
        assert_eq!(
            refusal(&tx::Refusal::NoTranscript).code,
            ErrorCode::NotFound
        );
        assert_eq!(
            refusal(&tx::Refusal::RecordTooLarge(8)).code,
            ErrorCode::Unsupported
        );
        let io = tx::Refusal::Io {
            path: "/gone".to_string(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(refusal(&io).code, ErrorCode::Internal);

        // A missing file is the host's read failing, not a caller mistake.
        let scratch = Scratch::new("missing");
        let source = tx::Source {
            store: tx::Store::Claude,
            path: scratch.0.join("absent.jsonl"),
            session: None,
        };
        let err = window(&mut tx::Reader::new(), &facts(&source), &request(None))
            .expect_err("no such file");
        assert_eq!(err.code, ErrorCode::Internal);
    }

    /// The wire kind's own tag, which is what a device switches on.
    fn label(kind: &EventKind) -> &'static str {
        match kind {
            EventKind::SessionMeta(_) => "session_meta",
            EventKind::UserMessage { .. } => "user_message",
            EventKind::AssistantText { .. } => "assistant_text",
            EventKind::Thinking { .. } => "thinking",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolResult { .. } => "tool_result",
            EventKind::PermissionRequest { .. } => "permission_request",
            EventKind::TurnBoundary { .. } => "turn_boundary",
            EventKind::Usage { .. } => "usage",
            EventKind::SubagentRef { .. } => "subagent_ref",
            EventKind::Compaction { .. } => "compaction",
            EventKind::Attachment { .. } => "attachment",
            EventKind::Bookkeeping { .. } => "bookkeeping",
            EventKind::Unknown { .. } => "unknown",
        }
    }
}
