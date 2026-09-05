//! Read an agent's own transcript store as normalized events, newest first, without loading the
//! file.
//!
//! tma already knows which pane an agent is in, what state it is in, and (since 0.5.12) where that
//! agent writes its conversation. This crate is the reader on the end of that pointer: it maps four
//! file-backed stores into one event vocabulary, serves a page at a time from the end of the file
//! backwards, and refuses in a way that names the reason rather than returning an empty page.
//!
//! ```no_run
//! use tma_transcript::{discovery, Reader, WindowRequest};
//!
//! let roots = discovery::StoreRoots::from_env().expect("HOME");
//! let facts = discovery::PaneFacts {
//!     agent: "claude".into(),
//!     transcript: Some("/path/to/session.jsonl".into()),
//!     ..Default::default()
//! };
//! let source = discovery::discover(&facts, &roots)?;
//! let mut reader = Reader::new();
//! let page = reader.window(&source, &WindowRequest::new(200))?;
//! for event in &page.events {
//!     println!("{} {}", event.kind.label(), event.cursor);
//! }
//! # Ok::<(), tma_transcript::Refusal>(())
//! ```
//!
//! ## What the readers can and cannot do
//!
//! Four stores are served: claude, codex, gemini and pi, all append-only JSONL. Two are refused,
//! and both refusals are deliberate. cursor-agent's transcript records the prompt, the prose and a
//! bare `tool_use`, with no tool results, no timestamps and no version stamp, so a window over it
//! renders as holes; OpenCode keeps everything in SQLite, which is a different reader and a
//! different dependency, tracked as its own workstream.
//!
//! Nothing here streams tokens. Every store writes settled records, so the finest grain available
//! is a whole message, and "the assistant is writing" comes from tma's own detection rather than
//! from the file. Nothing here decides that an agent is blocked, either: an unresolved tool call
//! means in-flight, and a slow tool, a permission prompt and a crashed process look identical from
//! the transcript alone.

mod adapters;
pub mod discovery;
mod json;
mod model;
mod reader;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use model::{
    Body, Budget, Cursor, CursorParseError, Event, EventKind, FileId, ResultStatus, SessionMeta,
    Store, TurnKind,
};
pub use reader::{Reader, Tail, Window, WindowRequest};

/// One transcript file and the store whose grammar it speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub store: Store,
    pub path: PathBuf,
}

/// Why a request was not served. Every variant is a fact about the pane or the file, never a
/// guess, and every one has a stable [`Refusal::code`] a caller can branch on.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// Nothing on the pane pointed at a transcript, and no store layout held one for its session.
    #[error("no transcript file found for this pane")]
    NoTranscript,
    /// OpenCode: the conversation is in SQLite, not a file this reader opens.
    #[error("{store} keeps its transcript in SQLite; that reader is a separate workstream")]
    UnsupportedStore { store: Store },
    /// cursor-agent: the file exists but does not hold enough of a conversation to render.
    #[error(
        "{store} writes no tool results, timestamps or version stamp, so a transcript window over \
         it would render as holes rather than as a conversation"
    )]
    StoreIncomplete { store: Store },
    /// The file behind the cursor was rewritten, truncated, or replaced.
    #[error("this cursor no longer addresses the file (it was rewritten, truncated or replaced)")]
    CursorInvalid,
    /// One record is larger than the reader will materialize for a body fetch.
    #[error("the record at this cursor is larger than the {0} byte cap")]
    RecordTooLarge(u64),
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl Refusal {
    /// The stable kebab-case reason, for a `--json` surface and for a client to branch on.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NoTranscript => "no-transcript",
            Refusal::UnsupportedStore { .. } => "unsupported-store",
            Refusal::StoreIncomplete { .. } => "store-incomplete",
            Refusal::CursorInvalid => "cursor-invalid",
            Refusal::RecordTooLarge(_) => "record-too-large",
            Refusal::Io { .. } => "io-error",
        }
    }

    /// The refusal a non-readable store earns.
    pub(crate) fn for_store(store: Store) -> Refusal {
        match store {
            Store::OpenCode => Refusal::UnsupportedStore { store },
            _ => Refusal::StoreIncomplete { store },
        }
    }

    pub(crate) fn io(path: &Path, source: std::io::Error) -> Refusal {
        Refusal::Io {
            path: path.display().to_string(),
            source,
        }
    }

    pub(crate) fn read(source: std::io::Error) -> Refusal {
        Refusal::Io {
            path: "read".into(),
            source,
        }
    }
}

impl From<CursorParseError> for Refusal {
    fn from(_: CursorParseError) -> Refusal {
        Refusal::CursorInvalid
    }
}
