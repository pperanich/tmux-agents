//! One arm per store, on its pinned fixture, plus the three quirks that would each silently
//! corrupt a rendered conversation if an adapter got them wrong.

use std::path::{Path, PathBuf};

use super::{assert_expected, describe, fixtures};
use crate::model::{Body, EventKind, ResultStatus, Store};
use crate::{Reader, Source, WindowRequest};

/// Every event in a fixture, oldest first and with bodies, which is what an expectation pins.
fn read_all(store: Store, path: &Path) -> (Vec<crate::Event>, u64) {
    let source = Source {
        store,
        path: path.to_path_buf(),
    };
    let mut reader = Reader::new();
    let page = reader
        .window(&source, &WindowRequest::new(10_000).with_bodies())
        .expect("the fixture must be readable");
    assert_eq!(page.older, None, "the whole fixture must fit in one page");
    let mut events = page.events;
    events.reverse();
    (events, page.unknown)
}

fn fixture(rel: &str) -> PathBuf {
    fixtures().join(rel)
}

/// A-234: the emitted sequence equals the committed expectation, for every file store.
#[test]
fn each_store_maps_its_fixture_to_the_committed_sequence() {
    for (store, rel) in [
        (
            Store::Claude,
            "stores/claude/2.1.236/claude-2.1.236-session.jsonl",
        ),
        (
            Store::Codex,
            "stores/codex/0.146.0/codex-0.146.0-rollout.jsonl",
        ),
        (
            Store::Gemini,
            "stores/gemini/0.46.0/gemini-0.46.0-chat.jsonl",
        ),
        (Store::Pi, "stores/pi/0.84.2/pi-0.84.2-tool-call.jsonl"),
    ] {
        let path = fixture(rel);
        let (events, unknown) = read_all(store, &path);
        assert_eq!(unknown, 0, "{store} fixture must map cleanly");
        let lines: Vec<String> = events.iter().map(describe).collect();
        assert_expected(&path.with_extension("expected.txt"), &lines);
    }
}

/// A-234's pi arm, which is not claude's. pi's tool call is `{"type":"toolCall", …, "arguments"}`
/// and its result is hoisted to a fourth message **role** with the correlation id at message level.
/// An adapter that dispatched on block type first would map that result's plain `text` block to
/// assistant prose: the tool's output rendered as the model's own words, with no unknown to alert
/// on. This asserts the role won.
#[test]
fn pi_routes_tool_calls_and_results_by_role_not_by_block_type() {
    let (events, _) = read_all(
        Store::Pi,
        &fixture("stores/pi/0.84.2/pi-0.84.2-tool-call.jsonl"),
    );
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCall { name, call_id, .. } => Some((name.clone(), call_id.clone())),
            _ => None,
        })
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult {
                call_id, status, ..
            } => Some((call_id.clone(), *status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("read".to_string(), Some("read_0".to_string())),
            ("bash".to_string(), Some("bash_0".to_string())),
        ]
    );
    assert_eq!(
        results,
        vec![
            (Some("read_0".to_string()), ResultStatus::Ok),
            (Some("bash_0".to_string()), ResultStatus::Error),
        ],
        "the results must correlate by the message-level toolCallId, error flag intact"
    );
    // The failure this exists to catch: three assistant-text events instead of one.
    let prose = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::AssistantText { .. }))
        .count();
    assert_eq!(
        prose, 1,
        "a tool result must never render as assistant prose"
    );

    // pi is the only store that writes cost, so the usage event carries it.
    let costs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Usage { cost_usd, .. } => *cost_usd,
            _ => None,
        })
        .collect();
    assert_eq!(costs, vec![0.00111, 0.0012, 0.00135]);
}

/// A-235: one turn written to both codex channels renders once. Prose comes from `event_msg`, the
/// tool call from `response_item`, and each channel's mirror of the other is bookkeeping.
#[test]
fn codex_collapses_its_two_channels_to_one_turn() {
    let (events, _) = read_all(
        Store::Codex,
        &fixture("stores/codex/0.146.0/codex-0.146.0-rollout.jsonl"),
    );
    let count = |f: fn(&EventKind) -> bool| events.iter().filter(|e| f(&e.kind)).count();
    assert_eq!(
        count(|k| matches!(k, EventKind::UserMessage { .. })),
        1,
        "the response_item mirror must not double the user message"
    );
    assert_eq!(
        count(|k| matches!(k, EventKind::AssistantText { .. })),
        1,
        "the response_item mirror must not double the assistant message"
    );
    assert_eq!(count(|k| matches!(k, EventKind::Thinking { .. })), 1);

    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCall { name, call_id, .. } => Some((name.clone(), call_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![("shell".to_string(), Some("call_0001".to_string()))],
        "tool calls come from response_item only; the exec_command_* pair is its mirror"
    );
    assert_eq!(count(|k| matches!(k, EventKind::ToolResult { .. })), 1);
}

/// A-236: gemini restates its whole history in `$set` records. Nineteen real messages against
/// twenty-five restatements must render as nineteen, not forty-four.
#[test]
fn gemini_renders_messages_once_despite_its_restatements() {
    let path = fixture("stores/gemini/0.46.0/gemini-0.46.0-chat.jsonl");
    let restatements = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| l.contains("\"$set\""))
        .count();
    assert_eq!(restatements, 25, "the fixture's own premise");

    let (events, _) = read_all(Store::Gemini, &path);
    let messages = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::UserMessage { .. } | EventKind::AssistantText { .. }
            )
        })
        .count();
    assert_eq!(messages, 19);
    // The restatements are counted, not drawn, and not mistaken for unknowns.
    let dropped = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Bookkeeping { type_name } if type_name == "gemini/$set"))
        .count();
    assert_eq!(dropped, 25);
}

/// A-238: a claude `Task` call points at a child file, and that child is served as its own session
/// rather than interleaved into the parent's window.
#[test]
fn a_claude_subagent_is_its_own_session() {
    let parent = Source {
        store: Store::Claude,
        path: fixture("stores/claude/2.1.236/claude-2.1.236-session.jsonl"),
    };
    let (events, _) = read_all(Store::Claude, &parent.path);
    let refs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SubagentRef { child_id, .. } => Some(child_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(refs, vec!["sub01".to_string()]);

    let listed = crate::discovery::subagents(&parent);
    assert_eq!(listed.len(), 1, "one child file beside the parent");
    let child = crate::discovery::child_source(&parent, "sub01").expect("resolve the child");
    let (child_events, _) = read_all(Store::Claude, &child.path);

    // The parent's window holds the pointer and none of the child's events.
    let parent_tools: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCall { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(parent_tools, vec!["Bash".to_string()]);
    let child_tools: Vec<_> = child_events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCall { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(child_tools, vec!["Read".to_string()]);
}

/// The claude arm proper: content blocks read in their record's role, sidecars named rather than
/// counted as holes, and a string `content` handled beside an array one.
#[test]
fn claude_maps_blocks_sidecars_and_boundaries() {
    let (events, unknown) = read_all(
        Store::Claude,
        &fixture("stores/claude/2.1.236/claude-2.1.236-session.jsonl"),
    );
    assert_eq!(unknown, 0);
    let labels: Vec<&str> = events.iter().map(|e| e.kind.label()).collect();
    assert_eq!(
        labels,
        vec![
            "user_message",
            "thinking",
            "tool_call",
            "tool_result",
            "subagent_ref",
            "tool_result",
            "assistant_text",
            "attachment",
            "bookkeeping",
            "turn_boundary",
            "compaction",
            "user_message",
        ]
    );
    // The tool call's body is its input, not its name.
    let call = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::ToolCall { .. }))
        .unwrap();
    assert_eq!(
        call.body,
        Some(Body::Json(
            r#"{"command":"xxxxx","description":"xxxxx xxxxx"}"#.into()
        ))
    );
}

/// A-240: an unseen version parses without error and raises the unknown counter; A-241's other half
/// is in `corpus.rs`.
#[test]
fn an_unseen_record_type_counts_instead_of_failing() {
    let (events, unknown) = read_all(Store::Claude, &fixture("drift/claude-99.0.0-drift.jsonl"));
    assert_eq!(unknown, 2, "both invented record types must be counted");
    let holes: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Unknown { type_name } => Some(type_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        holes,
        vec!["claude/frame-link", "claude/artifact-comment-monitor"]
    );
    // The record the adapter does know still maps, so drift degrades rather than blanks the view.
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::UserMessage { .. })));
}

/// A record that will not parse becomes a visible hole, never a silently dropped line.
#[test]
fn a_malformed_record_is_a_counted_hole() {
    let scratch = super::Scratch::new("malformed");
    let good = r#"{"type":"user","sessionId":"s","version":"2.1.236","message":{"role":"user","content":[{"type":"text","text":"x"}]}}"#;
    let path = scratch.write(
        "session.jsonl",
        &format!("{good}\n{{\"type\":\"user\"\n{good}\n"),
    );
    let (events, unknown) = read_all(Store::Claude, &path);
    assert_eq!(unknown, 1);
    assert_eq!(
        events.iter().map(|e| e.kind.label()).collect::<Vec<_>>(),
        vec!["user_message", "unknown", "user_message"]
    );
}
