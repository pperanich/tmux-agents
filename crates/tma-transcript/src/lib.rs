//! Read an agent's own transcript store as normalized events, newest first, without loading the
//! file.
//!
//! tma already knows which pane an agent is in, what state it is in, and (since 0.5.12) where that
//! agent writes its conversation. This crate is the reader on the end of that pointer: it maps four
//! file-backed stores and one SQLite store into one event vocabulary, serves a page at a time from
//! the end backwards, and refuses in a way that names the reason rather than returning an empty
//! page.
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
//! Five stores are served. Four are append-only JSONL (claude, codex, gemini, pi) and one is a
//! SQLite database (opencode), read through the default-off `opencode` feature. One is refused, and
//! the refusal is deliberate: cursor-agent's transcript records the prompt, the prose and a bare
//! `tool_use`, with no tool results, no timestamps and no version stamp, so a window over it renders
//! as holes.
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
#[cfg(feature = "opencode")]
mod opencode;
mod reader;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use model::{
    Body, Budget, Cursor, CursorParseError, Event, EventKind, FileId, ResultStatus, SessionMeta,
    Store, TurnKind,
};
pub use reader::{Reader, Tail, Window, WindowRequest};

/// One transcript, the store whose grammar it speaks, and where to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub store: Store,
    /// The transcript file, or for OpenCode the database holding every session.
    pub path: PathBuf,
    /// Which session inside `path`. Only OpenCode needs it, because its database holds them all;
    /// the file stores put one session per path and leave this `None`.
    pub session: Option<String>,
}

/// Why a request was not served. Every variant is a fact about the pane or the file, never a
/// guess, and every one has a stable [`Refusal::code`] a caller can branch on.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// Nothing on the pane pointed at a transcript, and no store layout held one for its session.
    #[error("no transcript file found for this pane")]
    NoTranscript,
    /// OpenCode against a build with the `opencode` feature compiled out.
    #[error(
        "{store} keeps its transcript in SQLite, and this build was compiled without the \
             SQLite reader (the `opencode` feature)"
    )]
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

    /// A SQLite failure, reported as an I/O refusal: what a caller can act on is that the store did
    /// not read, not which of SQLite's result codes came back.
    #[cfg(feature = "opencode")]
    pub(crate) fn db(path: &Path, source: rusqlite::Error) -> Refusal {
        Refusal::Io {
            path: path.display().to_string(),
            source: std::io::Error::other(source),
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
