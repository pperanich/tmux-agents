//! The state model: a closed `AgentState` core plus an open `Detail` dimension.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The closed, frozen published state vocabulary — "whose move is it". Normative: manifests map
/// into it, never redefine it. Machine tokens are the lowercase variant names; glyphs render only at surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    /// Prompt shown, nothing running.
    Idle,
    /// Agent is processing — the ball is with the agent.
    Working,
    /// Waiting on human input — the ball is with the human.
    Blocked,
    /// Recognized agent, unreadable evidence.
    Unknown,
}

impl AgentState {
    /// The machine token written to `@agent_state`.
    pub const fn token(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        }
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl FromStr for AgentState {
    type Err = GrammarError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(AgentState::Idle),
            "working" => Ok(AgentState::Working),
            "blocked" => Ok(AgentState::Blocked),
            "unknown" => Ok(AgentState::Unknown),
            other => Err(GrammarError::UnknownState(other.to_string())),
        }
    }
}

/// The open, additive detail dimension: *why* / qualification for a state. A newtype over the
/// token string; unknown tokens round-trip intact, so a newer manifest can emit one this engine
/// never heard of. The `@agent_detail` vocabulary is unstable until 1.0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Detail(String);

impl Detail {
    pub const PERMISSION: &'static str = "permission";
    /// A plan-approval dialog. Distinct from `permission` because its affirmative option grants
    /// every following action, not the one in front of the user.
    pub const PLAN: &'static str = "plan";
    /// A workspace-trust gate. Its affirmative option grants the whole folder, not one action.
    pub const TRUST: &'static str = "trust";
    pub const QUESTION: &'static str = "question";
    pub const ERROR: &'static str = "error";
    pub const RATE_LIMIT: &'static str = "rate_limit";
    pub const BACKGROUND: &'static str = "background";
    pub const COMPACTING: &'static str = "compacting";

    pub fn new(token: impl Into<String>) -> Self {
        Detail(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Public because it is the `Err` of the public `FromStr` impls and of
// [`StampedState::from_options`](crate::stamp::StampedState::from_options): a corrupt `@agent_*`
// option reads as never-stamped everywhere on the hot path, and `tma doctor` / `tma debug explain`
// need to name the option and the value that made it so.
pub use grammar::GrammarError;

mod grammar {
    /// Errors decoding the machine-token option grammar.
    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum GrammarError {
        #[error("unknown @agent_state token: {0:?}")]
        UnknownState(String),
        #[error("unknown @agent_source token: {0:?}")]
        UnknownProvenance(String),
        #[error("option {option} expected an integer, got {value:?}")]
        BadInteger { option: &'static str, value: String },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_token_display_fromstr_agree() {
        for s in [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Unknown,
        ] {
            let token = s.to_string();
            assert_eq!(token, s.token());
            assert_eq!(AgentState::from_str(&token).unwrap(), s);
        }
    }

    #[test]
    fn state_serde_token_matches_grammar() {
        // The serde token (used by manifests) must equal the option-grammar token,
        // so a `claim = { state = "blocked" }` and a `@agent_state blocked` never drift.
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            state: AgentState,
        }
        let decoded: Wrap = toml::from_str("state = \"blocked\"").unwrap();
        assert_eq!(decoded.state, AgentState::Blocked);
        let encoded = toml::to_string(&Wrap {
            state: AgentState::Working,
        })
        .unwrap();
        assert_eq!(encoded.trim(), "state = \"working\"");
    }

    #[test]
    fn unknown_state_token_errors() {
        assert_eq!(
            AgentState::from_str("running"),
            Err(GrammarError::UnknownState("running".to_string()))
        );
    }
}
