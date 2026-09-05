//! The crate's own suite. It lives inside the crate rather than in `tests/` because the corpus
//! checks read the fixtures through the private JSON module: an inventory test that used a second
//! parser would be pinning that parser, not the reader.

mod adapters;
mod corpus;
mod reader;

use std::path::{Path, PathBuf};

use crate::model::{Event, EventKind};

/// `crates/tma-transcript/fixtures`.
pub(crate) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// A scratch directory of this test's own, removed by [`Scratch`]'s drop.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "tma-transcript-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Write a file, creating its parents.
    pub(crate) fn write(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the fixture's parent directory");
        }
        std::fs::write(&path, body).expect("write the fixture");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One event as one reviewable line: the kind, then the fields that identify it. This is the shape
/// the committed expectation files hold, so a diff reads as a conversation rather than as JSON.
pub(crate) fn describe(event: &Event) -> String {
    let detail = match &event.kind {
        EventKind::SessionMeta(m) => format!(
            "agent={} session={} version={} model={}",
            m.agent,
            m.session_id.as_deref().unwrap_or("-"),
            m.version.as_deref().unwrap_or("-"),
            m.model.as_deref().unwrap_or("-"),
        ),
        EventKind::UserMessage { bytes, attachments } => {
            format!("bytes={bytes} attachments={attachments}")
        }
        EventKind::AssistantText { bytes } => format!("bytes={bytes}"),
        EventKind::Thinking { bytes, redacted } => format!("bytes={bytes} redacted={redacted}"),
        EventKind::ToolCall {
            name,
            call_id,
            arg_keys,
            bytes,
        } => format!(
            "name={name} call_id={} args=[{}] bytes={bytes}",
            call_id.as_deref().unwrap_or("-"),
            arg_keys.join(",")
        ),
        EventKind::ToolResult {
            call_id,
            status,
            bytes,
        } => format!(
            "call_id={} status={} bytes={bytes}",
            call_id.as_deref().unwrap_or("-"),
            status.as_str()
        ),
        EventKind::PermissionRequest { tool, call_id } => {
            format!("tool={tool} call_id={}", call_id.as_deref().unwrap_or("-"))
        }
        EventKind::TurnBoundary { kind, reason } => format!(
            "kind={} reason={}",
            kind.as_str(),
            reason.as_deref().unwrap_or("-")
        ),
        EventKind::Usage {
            input,
            output,
            total,
            context_window,
            cost_usd,
        } => format!(
            "input={} output={} total={} window={} cost={}",
            opt(*input),
            opt(*output),
            opt(*total),
            opt(*context_window),
            cost_usd.map_or("-".to_string(), |c| format!("{c:.5}")),
        ),
        EventKind::SubagentRef {
            child_id,
            external_file,
        } => format!("child={child_id} external={external_file}"),
        EventKind::Compaction { kind } => format!("kind={kind}"),
        EventKind::Attachment { kind, bytes } => format!("kind={kind} bytes={bytes}"),
        EventKind::Bookkeeping { type_name } | EventKind::Unknown { type_name } => {
            format!("type={type_name}")
        }
    };
    format!("{}\t{detail}", event.kind.label())
}

fn opt(v: Option<u64>) -> String {
    v.map_or("-".to_string(), |n| n.to_string())
}

/// Compare against a committed expectation, or rewrite it under `TMA_TRANSCRIPT_BLESS=1`.
pub(crate) fn assert_expected(path: &Path, lines: &[String]) {
    let got = lines.join("\n") + "\n";
    if std::env::var_os("TMA_TRANSCRIPT_BLESS").is_some() {
        std::fs::write(path, &got).expect("write the expectation file");
        return;
    }
    let want = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}\nrun the suite with TMA_TRANSCRIPT_BLESS=1 to write it",
            path.display()
        )
    });
    assert_eq!(
        got,
        want,
        "\n{} is out of date; re-run with TMA_TRANSCRIPT_BLESS=1 and read the diff",
        path.display()
    );
}
