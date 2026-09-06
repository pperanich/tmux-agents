//! Cards: what the pane is asking, typed so that the wrong affordance cannot be expressed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::Detail;

/// What a blocked pane wants, one variant per kind of asking.
///
/// The card is typed by variant rather than by a detail string beside a generic option list, so an
/// informational card has no field in which an approve control could be expressed. That is the
/// plan-dialog bug class made unrepresentable rather than merely unhandled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "card", rename_all = "kebab-case")]
pub enum Card {
    Permission(PermissionCard),
    Question(QuestionCard),
    /// plan, trust, an unknown detail: something to read and nothing to fire.
    Informational {
        detail: Detail,
        /// The card's own short label, computed on the host. Not the notification payload key of
        /// the same name.
        headline: String,
    },
    /// The pane is not asking anything.
    None,
}

/// Ask for the card a pane is showing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRequest {
    pub pane: String,
}

vocabulary! {
    /// Which transport produced the card, so a structured card is visibly distinguishable from a
    /// scraped one and a receipt's reader can tell what "approved" meant on that pane.
    pub enum Lane {
        /// Read off the pane's screen and answered with keystrokes.
        Screen = "screen",
        /// Read from and answered over the agent's own HTTP surface.
        Api = "api",
        /// Answered through a blocking agent hook: the tool call arrives as data, unwrapped.
        Hook = "hook",
    }
}

vocabulary! {
    /// How much of the dialog the host could recover.
    ///
    /// `Wrapped` and `Failed` both degrade the card to open-on-host, and not merely because a label
    /// might be truncated: a wrapped consent line is **not invertible**. A capture carries no signal
    /// telling a break inside a token from a break on a space, so rejoining guesses, and a wrong
    /// guess yields a different filesystem path inside a consent string.
    pub enum Extraction {
        Exact = "exact",
        Wrapped = "wrapped",
        Failed = "failed",
    }
}

vocabulary! {
    /// What an option would do, which is the only thing the host gates on.
    ///
    /// Never gate on the label. An `AllowAlways` option is drawn only behind the second deliberate
    /// interaction R25 requires, and an option whose kind is `Other` is rendered without being
    /// offered, which is how cursor's Run Everything stays visible without becoming a button.
    pub open enum OptionKind {
        AllowOnce = "allow-once",
        AllowAlways = "allow-always",
        RejectOnce = "reject-once",
        RejectAlways = "reject-always",
    }
}

/// One option the dialog is offering, in ACP's shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub kind: OptionKind,
    /// The dialog's own line, verbatim. This is what the user reads and what the host never
    /// branches on.
    pub name: String,
    /// What a reply quotes: a digit on the screen lane, the request id on the api and hook lanes,
    /// and **absent** where the dialog prints no index at all, as cursor's does not. A position the
    /// host invented must never be rendered as a keycap the user could type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

/// The call a permission prompt is about, joined from the hook payload or the transcript. It is
/// never used to infer that a pane is blocked, only to say what it is blocked on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub tool: String,
    /// The agent's own tool input, as an object rather than a rendered line: the hook lane exists
    /// because a rendered line wraps and the wrap cannot be undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// A permission prompt, with everything a reply needs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionCard {
    pub pane: String,
    pub agent: String,
    pub lane: Lane,
    pub options: Vec<PermissionOption>,
    pub extraction: Extraction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_call: Option<PendingCall>,
    pub binder: Binder,
}

/// A question asked mid-turn, answered by picking options rather than by granting anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionCard {
    pub pane: String,
    pub agent: String,
    pub request_id: String,
    pub questions: Vec<Question>,
    pub binder: Binder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    /// Whether more than one option may be picked.
    #[serde(default)]
    pub multiple: bool,
    /// Whether the agent accepts an answer that is not one of the options.
    #[serde(default)]
    pub custom: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// What the card was built against, quoted back on the dispatch and re-checked under the pane's
/// single-flight lock.
///
/// A match is a necessary condition, never a sufficient one: the stamp outlives a successful reply,
/// so a request the device still names may already be answered. The server's `request-gone` is the
/// authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binder {
    pub expect_episode_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_permission_request: Option<String>,
}
