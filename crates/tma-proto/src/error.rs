//! Typed refusals. A request the host will not serve gets one of these, never a dropped connection
//! and never a parse failure.

use serde::{Deserialize, Serialize};

vocabulary! {
    /// Why the host refused a frame. Open, so a newer host's code degrades to "refused, and here is
    /// the sentence" rather than to a parse error on the device.
    pub open enum ErrorCode {
        /// The hello named a schema this build does not implement.
        UnsupportedSchema = "unsupported-schema",
        /// The frame parsed but does not say something this protocol can act on.
        BadRequest = "bad-request",
        /// The pane, slot or cursor named does not exist here.
        NotFound = "not-found",
        /// The device's granted scopes do not cover this request.
        ScopeDenied = "scope-denied",
        /// A well-formed request for something this host cannot do (an unreadable transcript store,
        /// an agent with no such action).
        Unsupported = "unsupported",
        /// The host failed. The device may retry.
        Internal = "internal",
    }
}

/// A refusal, in one shape for every reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub code: ErrorCode,
    /// One sentence for a person. A device branches on `code` and never on this.
    pub message: String,
}

impl ErrorFrame {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorFrame {
        ErrorFrame {
            code,
            message: message.into(),
        }
    }
}
