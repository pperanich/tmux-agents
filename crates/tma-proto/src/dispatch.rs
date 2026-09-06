//! Dispatch and its receipt: what a device asks the host to fire, and what it gets back.

use serde::{Deserialize, Serialize};

use crate::card::Binder;

vocabulary! {
    /// What a dispatch ended as. The host's closed `outcome` vocabulary, copied token for token
    /// (`tma_runtime::broker::Outcome`).
    ///
    /// `Sent` proves dispatch, not effect: the tmux client exited 0 after handing over the keys and
    /// nothing re-read the pane. `Timeout` is exec-only and unreachable from a device, so the
    /// reachable set here is `sent`, `replied`, `refused`, `vanished`, `error`; it stays in the
    /// vocabulary because the tokens are one contract, not two.
    pub open enum Outcome {
        Sent = "sent",
        /// An API-channel answer landed. Kept distinct from `sent` so a pinned meaning cannot
        /// change under a script.
        Replied = "replied",
        Exited = "exited",
        Spawned = "spawned",
        Timeout = "timeout",
        Refused = "refused",
        Vanished = "vanished",
        Error = "error",
    }
}

vocabulary! {
    /// Why a dispatch refused, or which target vanished.
    ///
    /// A superset of the host's two reason sets (`tma_core::RefusalReason` and
    /// `tma_runtime::broker::Refusal`, which is where `locked` lives) plus the vanished targets and
    /// the ledger's `fired-unknown`. `scope-denied` is the one token the host has no producer for
    /// yet: it is the remote-authorization refusal, and it exists here first because the serve loop
    /// is what will emit it.
    pub open enum Reason {
        WrongAgent = "wrong-agent",
        NoCoverage = "no-coverage",
        RequiresUnmet = "requires-unmet",
        Gated = "gated",
        /// The single-flight lock is held. The one refusal that leaves the slot open.
        Locked = "locked",
        EpisodeChanged = "episode-changed",
        /// The pane no longer carries the quoted request, or the server answered 404 for it. One
        /// word for both; the `outcome` beside it says which happened.
        RequestGone = "request-gone",
        PaneGone = "pane-gone",
        Sigil = "sigil",
        ControlBytes = "control-bytes",
        TooLong = "too-long",
        Empty = "empty",
        /// The dispatch's effect is genuinely unknown, so a retry replays this rather than firing.
        FiredUnknown = "fired-unknown",
        /// The device's granted scopes do not name this action class.
        ScopeDenied = "scope-denied",
    }
}

/// Fire one action at one pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    /// The idempotency key. A repeat returns the cached receipt and sends nothing.
    pub slot: String,
    pub host: String,
    pub pane: String,
    pub action: String,
    pub binder: Binder,
    /// Populated only for a `text` action. A `keys` action carrying it is a protocol error the host
    /// refuses, which preserves "no payload on a keys action" across this surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Populated only for a question reply: one list of picked options per question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<Vec<String>>>,
    /// Which device asked, for "who approved this". Never part of the slot key: idempotency across
    /// devices is the point, so device B's replay of a slot returns device A's receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// What one slot resolved to. The ledger's record, as it leaves the machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub slot: String,
    pub pane: String,
    pub action: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub reason: Option<Reason>,
    /// The exit code the original dispatch returned, replayed verbatim.
    pub exit_code: i32,
    /// True when this receipt was replayed from the ledger rather than earned by this call.
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub device: Option<String>,
    /// When the slot was claimed, epoch ms.
    pub at_ms: u64,
}

impl Receipt {
    /// Whether the slot is spent. False only for `locked`, the one refusal a retry can clear;
    /// derived rather than carried so it cannot disagree with the reason beside it.
    pub fn terminal(&self) -> bool {
        self.reason.as_ref() != Some(&Reason::Locked)
    }
}

/// Read the ledger back: by slot, by claim time, or both.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Claimed at or after this epoch-ms instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
}

/// What the ledger answered. A device reconciles every in-flight slot with this on a re-dial,
/// before it re-offers a control, instead of dispatching in order to learn whether the last
/// dispatch landed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipts {
    pub receipts: Vec<Receipt>,
}
