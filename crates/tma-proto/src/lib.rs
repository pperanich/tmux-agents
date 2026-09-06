//! The tma remote wire protocol: the frames a device and `tma serve` exchange.
//!
//! One crate, linked by the host and by the app alike, so there is one implementation of the
//! protocol and no second parser to keep in agreement. Types, their versioning discipline, and the
//! golden corpus under `vectors/` are the whole of it: no I/O, no tmux, no serve loop.
//!
//! # The rules a change has to hold
//!
//! 1. **Additive only; [`SCHEMA`] stays 1.** A new field is optional, defaulted on read, and
//!    skipped on write when it has nothing to say. A removal or a re-typing is `schema = 2`.
//! 2. **Closed vocabularies grow, they never re-mean.** A new [`Detail`], [`Outcome`], [`Reason`],
//!    [`OptionKind`] or [`ErrorCode`] token is additive because both sides have an `Other` arm.
//!    Changing what an existing token means is a schema bump.
//! 3. **Unknown fields are ignored; unknown enum variants are preserved.** The two have opposite
//!    correct answers on purpose: dropping a field loses detail, dropping a variant changes meaning.
//! 4. **The device states its schema in the hello.** A schema this build does not implement is
//!    refused with [`ErrorCode::UnsupportedSchema`], not with a parse failure.
//! 5. **Every type and every enum variant has a vector.** `tests/vectors.rs` enumerates this
//!    crate's own source and fails on one that does not.
//!
//! `README.md` beside this file walks through adding a field and its vector.

#[macro_use]
mod vocabulary;

mod card;
mod dispatch;
mod error;
mod fleet;
mod frame;
mod hello;
mod money;
mod notify;
mod state;
mod transcript;

#[cfg(test)]
mod props;

pub use card::{
    Binder, Card, CardRequest, Extraction, Lane, OptionKind, PendingCall, PermissionCard,
    PermissionOption, Question, QuestionCard, QuestionOption,
};
pub use dispatch::{Dispatch, Outcome, Reason, Receipt, Receipts, ReceiptsRequest};
pub use error::{ErrorCode, ErrorFrame};
pub use fleet::{Edge, FleetRow, Quota, Selector, Snapshot, SnapshotRequest, Subscribe};
pub use frame::{Request, RequestFrame, Response, ResponseFrame};
pub use hello::{Hello, HelloOk, Scope};
pub use notify::NotifyPayload;
pub use state::{Detail, State, StateFilter};
pub use transcript::{
    Body, BodyKind, Budget, Cursor, EventHeader, EventKind, EventRequest, ResultStatus,
    SessionMeta, TurnKind, Window, WindowRequest,
};

/// The wire version. One integer, stated once per frame, in the envelope.
pub const SCHEMA: u32 = 1;

/// A frame that could not be read, or a value that could not be written. Opaque on purpose: the
/// codec is this crate's business, and a caller that matched on it would be coupled to it.
#[derive(Debug, thiserror::Error)]
#[error("protocol frame: {0}")]
pub struct Error(#[from] serde_json::Error);

/// One frame as a wire line: compact, no trailing newline.
pub fn encode<T: serde::Serialize>(value: &T) -> Result<String, Error> {
    Ok(serde_json::to_string(value)?)
}

/// One wire line back into its frame. Unknown fields are ignored, which is rule 3's read half.
pub fn decode<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, Error> {
    Ok(serde_json::from_str(line)?)
}
