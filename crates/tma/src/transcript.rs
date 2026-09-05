//! `tma transcript`: serve one pane's conversation as normalized events.
//!
//! The read is bounded on purpose. A window is the last N events from the end of the file
//! backwards, carrying headers rather than bodies, so a 44 MiB claude session costs the same as a
//! 4 KiB pi one; `--before` walks further back a page at a time, and `--event` fetches the one body
//! a reader actually wants to see. Nothing here reads a whole file.

use std::process::ExitCode;

use tma_core::stamp::opt;
use tma_transcript::{
    discovery::{self, PaneFacts, StoreRoots},
    Cursor, Event, EventKind, Reader, Refusal, Source, Window, WindowRequest,
};

use crate::cli::TranscriptArgs;
use crate::json::{JsonWriter, JSON_SCHEMA};
use crate::{cli_support, tmux};

/// A typed refusal (no transcript, a store this reader does not serve, a stale cursor). Distinct
/// from a runtime failure so a script can tell "there is nothing to show" from "something broke".
const EXIT_REFUSED: u8 = 4;

pub(crate) fn run(args: TranscriptArgs, server: &tmux::Server) -> ExitCode {
    let tmux = tmux::Tmux::connect(server);
    let panes = match tmux.list_panes() {
        Ok(panes) => panes,
        Err(tmux::TmuxError::ServerGone) => return cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(pane) = panes.into_iter().find(|p| p.pane_id == args.pane) else {
        eprintln!("tma: no pane {}", args.pane);
        return ExitCode::from(3);
    };
    let Some(roots) = StoreRoots::from_env() else {
        eprintln!("tma: HOME is unset, so no agent store can be located");
        return ExitCode::FAILURE;
    };

    let facts = PaneFacts {
        agent: pane.options.get(opt::NAME).cloned().unwrap_or_default(),
        session: pane.options.get(opt::SESSION).cloned(),
        transcript: pane.options.get(opt::TRANSCRIPT).cloned(),
        cwd: pane.cwd.as_ref().map(Into::into),
    };
    match serve(&args, &facts, &roots) {
        Ok(()) => ExitCode::SUCCESS,
        Err(refusal) => {
            report_refusal(&args, &refusal);
            ExitCode::from(EXIT_REFUSED)
        }
    }
}

fn serve(args: &TranscriptArgs, facts: &PaneFacts, roots: &StoreRoots) -> Result<(), Refusal> {
    let mut source = discovery::discover(facts, roots)?;
    if let Some(child) = &args.subagent {
        source = discovery::child_source(&source, child)?;
    }
    let mut reader = Reader::new();

    if let Some(raw) = &args.event {
        let cursor: Cursor = raw.parse()?;
        let event = reader.body(&source, &cursor)?;
        if args.json {
            println!("{}", render_event_document(args, &source, &event));
        } else {
            print_body(&event);
        }
        return Ok(());
    }

    let before = args.before.as_deref().map(str::parse).transpose()?;
    let mut request = WindowRequest::new(args.last).before(before);
    if !args.headers {
        request = request.with_bodies();
    }
    let window = reader.window(&source, &request)?;
    if args.json {
        println!("{}", render_window_document(args, &source, &window));
    } else {
        print_window(&window);
    }
    Ok(())
}

/// Text mode: one line per event, newest first, with the body's first line. `older` rides stderr so
/// a piped run gets only the events.
fn print_window(window: &Window) {
    for event in &window.events {
        println!(
            "{}\t{}\t{}",
            event.kind.label(),
            event.ts.as_deref().unwrap_or("-"),
            summary(event)
        );
    }
    if let Some(older) = &window.older {
        eprintln!("tma: more before this page: --before {older}");
    }
    if window.budget_truncated {
        eprintln!("tma: the page stopped at the read budget, not at --last");
    }
}

fn print_body(event: &Event) {
    match &event.body {
        Some(body) => println!("{}", body.as_str()),
        None => println!("{}", summary(event)),
    }
}

/// The one line a list row shows: the body's first line, or the structural fact for an event that
/// has no body of its own.
fn summary(event: &Event) -> String {
    if let Some(preview) = &event.preview {
        return preview.clone();
    }
    match &event.kind {
        EventKind::SessionMeta(m) => format!(
            "{} {}",
            m.agent,
            m.version.as_deref().unwrap_or("unversioned")
        ),
        EventKind::TurnBoundary { kind, reason } => {
            format!("{} {}", kind.as_str(), reason.as_deref().unwrap_or(""))
        }
        EventKind::SubagentRef { child_id, .. } => format!("--subagent {child_id}"),
        EventKind::Compaction { kind } => kind.clone(),
        EventKind::Attachment { kind, bytes } => format!("{kind} ({bytes} bytes)"),
        EventKind::Bookkeeping { type_name } | EventKind::Unknown { type_name } => {
            type_name.clone()
        }
        _ => String::new(),
    }
}

fn render_window_document(args: &TranscriptArgs, source: &Source, window: &Window) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    write_envelope(&mut j, args, source);
    match &window.session {
        Some(m) => {
            j.key("session");
            j.begin_object();
            j.string("agent", &m.agent);
            opt_string(&mut j, "session_id", m.session_id.as_deref());
            opt_string(&mut j, "cwd", m.cwd.as_deref());
            opt_string(&mut j, "version", m.version.as_deref());
            opt_string(&mut j, "model", m.model.as_deref());
            j.end_object();
        }
        None => j.null("session"),
    }
    match &window.older {
        Some(c) => j.string("older", &c.to_string()),
        None => j.null("older"),
    }
    j.bool("budget_truncated", window.budget_truncated);
    j.number("unknown", window.unknown as i64);
    j.key("events");
    j.begin_array();
    for event in &window.events {
        write_event(&mut j, event);
    }
    j.end_array();
    j.end_object();
    j.finish()
}

fn render_event_document(args: &TranscriptArgs, source: &Source, event: &Event) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    write_envelope(&mut j, args, source);
    j.key("event");
    write_event(&mut j, event);
    j.end_object();
    j.finish()
}

/// The refusal document, so `--json` always answers in one shape. Without `--json` it is the same
/// sentence on stderr, prefixed like every other tma diagnostic.
fn report_refusal(args: &TranscriptArgs, refusal: &Refusal) {
    if !args.json {
        eprintln!("tma: {refusal}");
        return;
    }
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.string("pane", &args.pane);
    j.key("refusal");
    j.begin_object();
    j.string("code", refusal.code());
    j.string("message", &refusal.to_string());
    j.end_object();
    j.end_object();
    println!("{}", j.finish());
}

fn write_envelope(j: &mut JsonWriter, args: &TranscriptArgs, source: &Source) {
    j.number("schema", JSON_SCHEMA);
    j.string("pane", &args.pane);
    j.string("agent", source.store.as_str());
    j.string("path", &source.path.display().to_string());
}

/// One event object. The keys are additive per kind, which is what keeps the schema at 1 as stores
/// grow fields: a consumer switches on `kind` and reads the keys it knows.
fn write_event(j: &mut JsonWriter, event: &Event) {
    j.begin_object();
    j.string("cursor", &event.cursor.to_string());
    j.string("kind", event.kind.label());
    opt_string(j, "ts", event.ts.as_deref());
    opt_string(j, "preview", event.preview.as_deref());
    match &event.kind {
        EventKind::SessionMeta(m) => {
            j.string("agent", &m.agent);
            opt_string(j, "session_id", m.session_id.as_deref());
            opt_string(j, "cwd", m.cwd.as_deref());
            opt_string(j, "version", m.version.as_deref());
            opt_string(j, "model", m.model.as_deref());
        }
        EventKind::UserMessage { bytes, attachments } => {
            j.number("bytes", *bytes as i64);
            j.number("attachments", *attachments as i64);
        }
        EventKind::AssistantText { bytes } => j.number("bytes", *bytes as i64),
        EventKind::Thinking { bytes, redacted } => {
            j.number("bytes", *bytes as i64);
            j.bool("redacted", *redacted);
        }
        EventKind::ToolCall {
            name,
            call_id,
            arg_keys,
            bytes,
        } => {
            j.string("name", name);
            opt_string(j, "call_id", call_id.as_deref());
            j.key("arg_keys");
            j.begin_array();
            for k in arg_keys {
                j.raw_string(k);
            }
            j.end_array();
            j.number("bytes", *bytes as i64);
        }
        EventKind::ToolResult {
            call_id,
            status,
            bytes,
        } => {
            opt_string(j, "call_id", call_id.as_deref());
            j.string("status", status.as_str());
            j.number("bytes", *bytes as i64);
        }
        EventKind::PermissionRequest { tool, call_id } => {
            j.string("tool", tool);
            opt_string(j, "call_id", call_id.as_deref());
        }
        EventKind::TurnBoundary { kind, reason } => {
            j.string("boundary", kind.as_str());
            opt_string(j, "reason", reason.as_deref());
        }
        EventKind::Usage {
            input,
            output,
            total,
            context_window,
            cost_usd,
        } => {
            opt_number(j, "input", *input);
            opt_number(j, "output", *output);
            opt_number(j, "total", *total);
            opt_number(j, "context_window", *context_window);
            match cost_usd {
                Some(c) => j.money("cost_usd", *c),
                None => j.null("cost_usd"),
            }
        }
        EventKind::SubagentRef {
            child_id,
            external_file,
        } => {
            j.string("child_id", child_id);
            j.bool("external_file", *external_file);
        }
        EventKind::Compaction { kind } => j.string("compaction", kind),
        EventKind::Attachment { kind, bytes } => {
            j.string("attachment", kind);
            j.number("bytes", *bytes as i64);
        }
        EventKind::Bookkeeping { type_name } | EventKind::Unknown { type_name } => {
            j.string("type_name", type_name)
        }
    }
    match &event.body {
        Some(body) => {
            j.key("body");
            j.begin_object();
            j.string("kind", body.kind());
            j.string("text", body.as_str());
            j.end_object();
        }
        None => j.null("body"),
    }
    j.end_object();
}

fn opt_string(j: &mut JsonWriter, key: &str, value: Option<&str>) {
    match value {
        Some(v) => j.string(key, v),
        None => j.null(key),
    }
}

/// A token count past `i64::MAX` is not a number any store writes; clamping keeps the writer's
/// signed API honest rather than wrapping.
fn opt_number(j: &mut JsonWriter, key: &str, value: Option<u64>) {
    match value {
        Some(v) => j.number(key, i64::try_from(v).unwrap_or(i64::MAX)),
        None => j.null(key),
    }
}
