//! `tma transcript` acceptance on a scratch `tmux -L` server (killed on drop).
//!
//! Every case stamps a pane with `@agent_transcript` pointing at a COPY of a committed fixture in
//! the scratch workdir, so nothing here can reach a real agent store, and the copy can be truncated
//! under the reader without touching the corpus.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tma_test_support::Scratch;

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

fn transcript(s: &Scratch, args: &[&str]) -> Output {
    Command::new(s.bin())
        .arg("transcript")
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("TMA_CONFIG", s.config_path())
        .output()
        .expect("spawn tma transcript")
}

/// The committed corpus, which the tests copy from and never write to.
fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tma-transcript/fixtures")
        .join(rel)
}

/// Copy a fixture into the scratch workdir and stamp a pane at it. The stamp is the whole point:
/// discovery prefers `@agent_transcript` over any store walk, so the test never needs a fake HOME.
fn stamped_pane(s: &Scratch, agent: &str, rel: &str) -> (String, PathBuf) {
    let pane = s.new_pane();
    let copy = s.workdir.join(format!("{agent}-transcript.jsonl"));
    std::fs::copy(fixture(rel), &copy).expect("copy the fixture");
    s.set_opt(&pane, "@agent_name", agent);
    s.set_opt(&pane, "@agent_session", "synthetic-session");
    s.set_opt(&pane, "@agent_transcript", &copy.display().to_string());
    (pane, copy)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The text surface: one line per event, newest first, with the kind, the store's own timestamp,
/// and the body's first line.
#[test]
fn text_mode_prints_one_line_per_event_newest_first() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-text");
    let (pane, _) = stamped_pane(
        &s,
        "claude",
        "stores/claude/2.1.236/claude-2.1.236-session.jsonl",
    );

    let out = transcript(&s, &["--pane", &pane]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = stdout(&out);
    let lines: Vec<&str> = doc.lines().collect();
    assert_eq!(lines.len(), 12, "the fixture maps to twelve events");
    // Newest first: the fixture's last record is a user message, its first is one too.
    assert!(lines[0].starts_with("user_message\t2026-01-01T00:00:11"));
    assert!(lines[11].starts_with("user_message\t2026-01-01T00:00:01"));
    for line in &lines {
        assert_eq!(
            line.matches('\t').count(),
            2,
            "kind, time, first line: {line}"
        );
    }

    // `--last` counts back from the newest.
    let out = transcript(&s, &["--pane", &pane, "--last", "3"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out).lines().count(), 3);
}

/// The JSON surface: a schema-1 document whose `events` array carries opaque cursors, and whose
/// `--headers` form carries no bodies at all.
#[test]
fn json_mode_is_a_schema_one_document_with_opaque_cursors() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-json");
    let (pane, path) = stamped_pane(
        &s,
        "codex",
        "stores/codex/0.146.0/codex-0.146.0-rollout.jsonl",
    );

    let out = transcript(&s, &["--pane", &pane, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = stdout(&out);
    assert!(doc.starts_with(r#"{"schema":1,"pane":"#), "{doc}");
    assert!(doc.contains(&format!(r#""path":"{}""#, path.display())));
    assert!(doc.contains(r#""agent":"codex""#));
    assert!(
        doc.contains(r#""version":"0.146.0""#),
        "the session header rides the first page"
    );
    assert!(
        doc.contains(r#""older":null"#),
        "the whole fixture is one page"
    );
    assert!(doc.contains(r#""budget_truncated":false"#));
    assert!(doc.contains(r#""unknown":0"#));
    assert!(doc.contains(r#""kind":"tool_call""#) && doc.contains(r#""name":"shell""#));
    assert!(
        doc.contains(r#""cursor":"t1."#),
        "cursors are opaque tokens"
    );

    // `--headers` drops every body; the default local run keeps them.
    let headers = stdout(&transcript(&s, &["--pane", &pane, "--json", "--headers"]));
    assert!(
        !headers.contains(r#""body":{"#),
        "a header frame carries no bodies"
    );
    assert!(
        doc.contains(r#""body":{"kind":"text""#),
        "the default run carries them"
    );
}

/// A cursor from one call fetches that event's body on the next, and only that event's.
#[test]
fn a_cursor_fetches_one_event_body() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-body");
    let (pane, _) = stamped_pane(&s, "pi", "stores/pi/0.84.2/pi-0.84.2-tool-call.jsonl");

    let doc = stdout(&transcript(&s, &["--pane", &pane, "--json", "--headers"]));
    let cursor = first_cursor(&doc);
    let out = transcript(&s, &["--pane", &pane, "--event", &cursor, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let one = stdout(&out);
    assert!(one.contains(r#""event":{"#));
    assert!(
        one.contains(r#""body":{"#),
        "a body read carries the body: {one}"
    );
    assert!(
        !one.contains(r#""events":["#),
        "a body read is not a window"
    );

    // Text mode prints the body itself, which is what a `less` pipe wants.
    let text = stdout(&transcript(&s, &["--pane", &pane, "--event", &cursor]));
    assert!(!text.trim().is_empty());
}

/// Paging: the `older` cursor from one page is accepted as `--before` on the next, and the pages do
/// not overlap.
#[test]
fn older_cursors_page_backwards() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-paging");
    let (pane, _) = stamped_pane(
        &s,
        "gemini",
        "stores/gemini/0.46.0/gemini-0.46.0-chat.jsonl",
    );

    let first = stdout(&transcript(&s, &["--pane", &pane, "--json", "--last", "5"]));
    let older = older_cursor(&first).expect("a 47-record fixture has more than five events");
    let second = stdout(&transcript(
        &s,
        &["--pane", &pane, "--json", "--last", "5", "--before", &older],
    ));
    let page_one: Vec<String> = cursors(&first);
    let page_two: Vec<String> = cursors(&second);
    assert_eq!(page_one.len(), 5);
    assert_eq!(page_two.len(), 5);
    assert!(
        page_one.iter().all(|c| !page_two.contains(c)),
        "pages must not overlap"
    );
}

/// A stale cursor is a typed refusal with exit 4, not a mis-parse of whatever now sits at that
/// offset. The copy is truncated under the reader between the two calls.
#[test]
fn a_stale_cursor_refuses_with_exit_four() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-stale");
    let (pane, path) = stamped_pane(
        &s,
        "claude",
        "stores/claude/2.1.236/claude-2.1.236-session.jsonl",
    );
    let doc = stdout(&transcript(&s, &["--pane", &pane, "--json", "--last", "3"]));
    let older = older_cursor(&doc).expect("more than three events");

    let head: String = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .take(2)
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, head).unwrap();

    let out = transcript(&s, &["--pane", &pane, "--json", "--before", &older]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        stdout(&out).contains(r#""code":"cursor-invalid""#),
        "{}",
        stdout(&out)
    );
}

/// A store the reader will not serve, and a pane with no transcript at all, both refuse by name.
/// An empty window would read as "nothing happened", which is the failure this pins.
#[test]
fn a_refused_store_names_its_reason_with_exit_four() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-refused");
    let (pane, _) = stamped_pane(
        &s,
        "cursor",
        "stores/claude/2.1.236/claude-2.1.236-session.jsonl",
    );
    let out = transcript(&s, &["--pane", &pane, "--json"]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        stdout(&out).contains(r#""code":"store-incomplete""#),
        "{}",
        stdout(&out)
    );

    s.set_opt(&pane, "@agent_name", "opencode");
    let out = transcript(&s, &["--pane", &pane, "--json"]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        stdout(&out).contains(r#""code":"unsupported-store""#),
        "{}",
        stdout(&out)
    );

    // A pane carrying no agent at all: still a named refusal, still exit 4. An empty option reads
    // as absent, which is exactly what an unstamped pane looks like.
    s.set_opt(&pane, "@agent_name", "");
    s.set_opt(&pane, "@agent_transcript", "");
    s.set_opt(&pane, "@agent_session", "");
    let out = transcript(&s, &["--pane", &pane, "--json"]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        stdout(&out).contains(r#""code":"no-transcript""#),
        "{}",
        stdout(&out)
    );

    // A pane that does not exist is exit 3, the shared "target gone" code.
    let out = transcript(&s, &["--pane", "%9999"]);
    assert_eq!(out.status.code(), Some(3));
}

/// `--subagent` serves a claude child transcript as its own session; the parent window holds the
/// pointer and none of the child's events.
#[test]
fn a_subagent_is_served_as_its_own_session() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("transcript-subagent");
    let pane = s.new_pane();
    // The child lives in `<stem>/subagents/`, so the copy has to keep that shape.
    let parent = s.workdir.join("claude-2.1.236-session.jsonl");
    std::fs::copy(
        fixture("stores/claude/2.1.236/claude-2.1.236-session.jsonl"),
        &parent,
    )
    .unwrap();
    let children = s.workdir.join("claude-2.1.236-session/subagents");
    std::fs::create_dir_all(&children).unwrap();
    std::fs::copy(
        fixture("stores/claude/2.1.236/claude-2.1.236-session/subagents/agent-sub01.jsonl"),
        children.join("agent-sub01.jsonl"),
    )
    .unwrap();
    s.set_opt(&pane, "@agent_name", "claude");
    s.set_opt(&pane, "@agent_transcript", &parent.display().to_string());

    let parent_doc = stdout(&transcript(&s, &["--pane", &pane, "--json"]));
    assert!(parent_doc.contains(r#""kind":"subagent_ref""#));
    assert!(parent_doc.contains(r#""child_id":"sub01""#));
    assert!(
        !parent_doc.contains(r#""name":"Read""#),
        "the child's tool call must not be interleaved into the parent window"
    );

    let child_doc = stdout(&transcript(
        &s,
        &["--pane", &pane, "--json", "--subagent", "sub01"],
    ));
    assert!(child_doc.contains(r#""name":"Read""#));
    assert!(
        !child_doc.contains(r#""name":"Bash""#),
        "the parent's call is not the child's"
    );
}

/// Every `"cursor":"…"` in a document, in order.
fn cursors(doc: &str) -> Vec<String> {
    doc.match_indices(r#""cursor":""#)
        .map(|(i, m)| {
            let rest = &doc[i + m.len()..];
            rest[..rest.find('"').expect("a closed cursor string")].to_string()
        })
        .collect()
}

fn first_cursor(doc: &str) -> String {
    cursors(doc).into_iter().next().expect("at least one event")
}

/// The document's `older` cursor, or `None` when it is null.
fn older_cursor(doc: &str) -> Option<String> {
    let at = doc.find(r#""older":"#)? + r#""older":"#.len();
    let rest = &doc[at..];
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}
