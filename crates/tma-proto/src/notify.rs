//! The push payload, which is defined here although v1 ships no first-party sender.

use serde::{Deserialize, Serialize};

use crate::state::State;

/// What a notification may carry off the machine. Five fields, no sixth.
///
/// It is navigate-only by construction: no action, no option, no request id, so nothing on the
/// notification path can be acted on without a live connection. The type exists in v1 without a
/// sender because it is the constraint a forwarder would have to be built against, and a forwarder
/// that learns delivery metadata and nothing else is only possible if this shape is fixed first.
///
/// Not to be confused with the host's `[notify]` sink payload, which is a different surface with a
/// wider field list going to a carrier the user configured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyPayload {
    pub host: String,
    pub pane: String,
    pub agent: String,
    pub state: State,
    /// The binder value, absolute epoch ms. Deliberately not the `since_ms` age the host's own
    /// notification payload carries: an age cannot be compared for equality against a stamp.
    pub episode_ms: u64,
}
