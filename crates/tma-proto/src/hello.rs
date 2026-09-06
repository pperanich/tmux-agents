//! The handshake. One round trip that fixes the schema and states what the device may do.

use serde::{Deserialize, Serialize};

vocabulary! {
    /// What a paired device is allowed to ask for. Granted on the host at pairing time, restated in
    /// the hello response so the app can grey out what it will only be refused for.
    pub enum Scope {
        /// Read the fleet, cards, receipts and transcripts.
        Read = "read",
        /// Answer a permission prompt or a question.
        ActAnswer = "act:answer",
        /// Answer with an option that grants every following action of its class.
        ActAlways = "act:always",
        /// Send the agent a line of the user's own text.
        ActSteer = "act:steer",
    }
}

/// What the device says about itself. The schema it speaks rides the frame envelope, which is where
/// [`crate::RequestFrame::accept_hello`] reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The client's name, for the host's audit line.
    pub app: String,
    pub app_version: String,
    /// The device's public-key fingerprint, as the pairing record names it.
    pub device: String,
}

/// What the host says back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    pub host: String,
    pub tma_version: String,
    /// How stale a row may get before the device should re-read rather than trust it. The freshness
    /// threshold has no other home on the wire.
    pub reconcile_interval_ms: u64,
    pub scopes: Vec<Scope>,
}
