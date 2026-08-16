//! Action manifests: user-declared actions the broker fires into an agent pane. Two kinds under
//! one TOML form — `keys` (a guarded key sequence) and `exec` (a guarded process spawn with
//! context env). This module owns the schema, the validating loader, and the pure applicability +
//! gate evaluation over a snapshot row; the broker, the pane lock, and process spawning are I/O and
//! live above the core.
//!
//! The gate vocabulary is closed and shared with `tma act --list` and the broker: an action
//! is `Fireable` or `Refused` with one of `wrong-agent` / `no-coverage` / `requires-unmet` /
//! `gated`. `locked` is a broker-time verdict, not a gate outcome, so it is absent here.
//!
//! The parent holds the public schema types (`ActionManifest`, the transport and kind enums, and
//! the `ActionError` boundary); `schema` owns the TOML loader and structural rules, `gate` the
//! applicability and gate evaluation.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::manifest::Version;

mod gate;
mod schema;

pub use gate::{ContextKeys, GateInput, GateOutcome, RefusalReason, Requirement, When};
use schema::StructuralRule;

/// A parsed, validated action manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionManifest {
    /// The minimum engine version this manifest requires.
    pub min_engine_version: Version,
    /// The action name, equal to the filename stem and a safe machine token (it rides the
    /// single-flight lock value, so it must never carry a comma or format metacharacter).
    pub name: String,
    /// Human label for surfaces (`display-menu`, deck). Free text, not a token.
    pub label: String,
    pub kind: ActionKind,
    /// Optional gate; absent means always fireable for the applicable agents. Present keys
    /// are ANDed.
    pub when: Option<When>,
    /// Applicability for an `exec` action: empty means all agents. Never set on a `keys` action
    /// (its applicability comes from the `[keys]` table instead).
    pub agents: Vec<String>,
    /// Context keys that must be non-empty for the gate to pass.
    pub requires: Vec<Requirement>,
    /// The action wants a second factor before firing; enforcement is per-surface.
    pub confirm: bool,
    /// Run detached under a supervisor rather than synchronously (exec only).
    pub detach: bool,
    /// Synchronous execution bound and lock expiry base (exec).
    pub timeout_ms: u64,
    /// Detached execution bound and lock expiry base for `detach = true` (exec).
    pub detach_timeout_ms: u64,
    /// The shell command (`exec` only), passed to `sh -c` verbatim with no substitution.
    pub command: Option<String>,
    /// Per-agent key sequences (`keys` only): agent name ⇒ one `send-keys` argument list. An agent
    /// with no entry cannot receive the action.
    pub keys: BTreeMap<String, Vec<String>>,
    /// Per-agent API-channel transports (`keys` kind only): agent name ⇒ a built-in operation
    /// the broker delivers over HTTP instead of keystrokes. Applicability is the union of `keys` and
    /// `api`; an agent in both is a parse error (no silent transport fallback).
    pub api: BTreeMap<String, ApiTransport>,
}

/// One agent's API-channel transport: a closed built-in operation, extended only with
/// captured evidence like key sequences. v1 ships exactly `permission-reply` (OpenCode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiTransport {
    pub op: ApiOp,
    /// The reply verdict for a `permission-reply` op.
    pub reply: ApiReply,
}

/// The closed API operation vocabulary. Unknown values are a parse error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ApiOp {
    /// Answer a pending OpenCode permission prompt (`POST {base}/permission/{id}/reply`).
    #[serde(rename = "permission-reply")]
    PermissionReply,
}

impl ApiOp {
    pub const fn token(self) -> &'static str {
        match self {
            ApiOp::PermissionReply => "permission-reply",
        }
    }
}

/// The closed `permission-reply` verdict vocabulary, the OpenCode wire values. Unknown
/// values are a parse error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiReply {
    Once,
    Always,
    Reject,
}

impl ApiReply {
    /// The wire token sent in the reply body (`{"reply": "<token>"}`).
    pub const fn token(self) -> &'static str {
        match self {
            ApiReply::Once => "once",
            ApiReply::Always => "always",
            ApiReply::Reject => "reject",
        }
    }
}

/// The two action kinds under one manifest form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// A guarded key sequence delivered into the pane.
    Keys,
    /// A guarded process spawn with context env.
    Exec,
}

/// Action manifest load/validation errors. Every variant names the offending file.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("{file}: {source}")]
    // Exposing `toml::de::Error` publicly is deliberate: this crate feeds the tma binary, the toml
    // version is workspace-pinned, and an opaque wrapper would cost more than it protects.
    Parse {
        file: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{file}: invalid min_engine_version: {reason}")]
    BadVersion { file: String, reason: String },
    #[error(
        "{file}: manifest requires engine {required} but this engine is {engine} \
         (upgrade tma to load it)"
    )]
    EngineTooOld {
        file: String,
        required: Version,
        engine: Version,
    },
    #[error("{file}: name {name:?} must equal the filename stem {stem:?}")]
    NameMismatch {
        file: String,
        name: String,
        stem: String,
    },
    #[error(
        "{file}: {field} token {token:?} is not a valid machine token \
         (lowercase a-z, digits, and _- only — no glyphs or format metacharacters)"
    )]
    BadToken {
        file: String,
        field: &'static str,
        token: String,
    },
    #[error("{file}: {rule}")]
    Structural { file: String, rule: StructuralRule },
    #[error("{file}: {reason}")]
    BadContextBound { file: String, reason: String },
    #[error(
        "{file}: agent {agent:?} appears in both [keys] and [api]; one action cannot deliver two \
         transports to the same agent"
    )]
    AgentInBothTransports { file: String, agent: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_vocabulary_tokens_are_pinned() {
        assert_eq!(ApiOp::PermissionReply.token(), "permission-reply");
        assert_eq!(ApiReply::Once.token(), "once");
        assert_eq!(ApiReply::Always.token(), "always");
        assert_eq!(ApiReply::Reject.token(), "reject");
    }
}
