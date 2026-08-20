//! The typed-evidence model: heterogeneous signals reduced to ranked records.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::state::{AgentState, Detail};

/// Where a piece of evidence came from; the fold ranks by this. Coarser [`Provenance`] is what
/// persists to `@agent_source` — several sources fold to one bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A fresh agent hook event — highest authority, decays over time.
    HookEvent,
    /// A `capture-pane` screen rule matched.
    ScreenRule,
    /// The pane title carried recognizable chrome (spinner / idle marker).
    Title,
    /// A process-tree fact (pid present/gone, foreground command).
    ProcessFact,
}

impl Source {
    /// Fold this source down to the persisted [`Provenance`] bucket for `@agent_source`. Only
    /// `hook` vs non-hook is load-bearing for the write guards; title and screen both record `capture`.
    pub fn provenance(self) -> Provenance {
        match self {
            Source::HookEvent => Provenance::Hook,
            Source::ScreenRule | Source::Title => Provenance::Capture,
            Source::ProcessFact => Provenance::Process,
        }
    }
}

/// The provenance value persisted in `@agent_source`. Coarser than [`Source`]:
/// the write-ownership guards only need to tell `hook` from everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Hook,
    Capture,
    /// Legacy, no longer produced: a viewport hash change used to count as working evidence.
    /// Kept so a running tmux server carrying `@agent_source=activity` still decodes.
    Activity,
    Process,
}

impl Provenance {
    pub const fn token(self) -> &'static str {
        match self {
            Provenance::Hook => "hook",
            Provenance::Capture => "capture",
            Provenance::Activity => "activity",
            Provenance::Process => "process",
        }
    }
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.token())
    }
}

impl std::str::FromStr for Provenance {
    type Err = crate::state::GrammarError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hook" => Ok(Provenance::Hook),
            "capture" => Ok(Provenance::Capture),
            "activity" => Ok(Provenance::Activity),
            "process" => Ok(Provenance::Process),
            other => Err(crate::state::GrammarError::UnknownProvenance(
                other.to_string(),
            )),
        }
    }
}

/// A state claim: what state (and optional detail) a signal asserts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateClaim {
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Detail>,
}

/// A lifecycle claim: an agent registered (`start`) or deregistered (`end`) on a pane. Carried by
/// SessionStart/SessionEnd hooks; handled by identity/episode logic, not the state fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Start,
    End,
}

/// What a piece of evidence claims: a state or a lifecycle transition. Deserializes from the
/// manifest `claim` table as exactly one of `{ state, detail }` or `{ lifecycle }`; strict —
/// unknown keys, a mixture, or an empty table each error by field (where `#[serde(untagged)]`
/// would silently drop typos). Serialization keeps the untagged shape so a claim round-trips.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Claim {
    State(StateClaim),
    Lifecycle { lifecycle: Lifecycle },
}

impl<'de> Deserialize<'de> for Claim {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// The union of both claim shapes; every field optional so we can diagnose which
        /// variant the author meant (and reject mixtures) instead of silently choosing one.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawClaim {
            state: Option<AgentState>,
            detail: Option<Detail>,
            lifecycle: Option<Lifecycle>,
        }

        let raw = RawClaim::deserialize(d)?;
        match (raw.state, raw.lifecycle) {
            (Some(state), None) => Ok(Claim::State(StateClaim {
                state,
                detail: raw.detail,
            })),
            (None, Some(lifecycle)) => {
                if raw.detail.is_some() {
                    return Err(de::Error::custom(
                        "claim `detail` belongs to the { state, detail } variant, \
                         not { lifecycle }",
                    ));
                }
                Ok(Claim::Lifecycle { lifecycle })
            }
            (Some(_), Some(_)) => Err(de::Error::custom(
                "claim must be either { state, detail } or { lifecycle }, \
                 not both `state` and `lifecycle`",
            )),
            (None, None) => Err(de::Error::custom(
                "claim needs a `state` (with optional `detail`) or a `lifecycle` field",
            )),
        }
    }
}

/// One typed evidence record. `at` is injected; the core never reads a clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub source: Source,
    pub claim: Claim,
    /// When the evidence was produced (epoch milliseconds, injected).
    pub at: u64,
    /// Rule id / hook name / matcher — carried for `tma debug explain`.
    pub meta: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_folds_to_provenance() {
        assert_eq!(Source::HookEvent.provenance(), Provenance::Hook);
        assert_eq!(Source::ScreenRule.provenance(), Provenance::Capture);
        assert_eq!(Source::Title.provenance(), Provenance::Capture);
        assert_eq!(Source::ProcessFact.provenance(), Provenance::Process);
    }

    #[test]
    fn provenance_token_roundtrip() {
        for p in [
            Provenance::Hook,
            Provenance::Capture,
            Provenance::Activity,
            Provenance::Process,
        ] {
            assert_eq!(p.to_string().parse::<Provenance>().unwrap(), p);
        }
    }

    #[test]
    fn claim_deserializes_state_variant() {
        #[derive(serde::Deserialize)]
        struct Row {
            claim: Claim,
        }
        let row: Row =
            toml::from_str(r#"claim = { state = "blocked", detail = "permission" }"#).unwrap();
        assert_eq!(
            row.claim,
            Claim::State(StateClaim {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
            })
        );
    }

    #[test]
    fn claim_deserializes_lifecycle_variant() {
        #[derive(serde::Deserialize)]
        struct Row {
            claim: Claim,
        }
        let start: Row = toml::from_str(r#"claim = { lifecycle = "start" }"#).unwrap();
        assert_eq!(
            start.claim,
            Claim::Lifecycle {
                lifecycle: Lifecycle::Start
            }
        );
        let end: Row = toml::from_str(r#"claim = { lifecycle = "end" }"#).unwrap();
        assert_eq!(
            end.claim,
            Claim::Lifecycle {
                lifecycle: Lifecycle::End
            }
        );
    }

    #[derive(serde::Deserialize)]
    struct ClaimRow {
        #[allow(dead_code)]
        claim: Claim,
    }

    /// Parse a `claim = ...` row and return the serde error message, asserting it failed.
    fn claim_err(src: &str) -> String {
        match toml::from_str::<ClaimRow>(src) {
            Ok(_) => panic!("expected claim parse to fail: {src}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn claim_rejects_typoed_field() {
        // A mistyped `detial` must error naming the field, not be silently dropped.
        let err = claim_err(r#"claim = { state = "blocked", detial = "permission" }"#);
        assert!(
            err.contains("detial"),
            "error should name the offending field: {err}"
        );
    }

    #[test]
    fn claim_rejects_variant_mixture() {
        let err = claim_err(r#"claim = { state = "blocked", lifecycle = "start" }"#);
        assert!(
            err.contains("both"),
            "mixing state and lifecycle must error: {err}"
        );
    }

    #[test]
    fn claim_rejects_detail_on_lifecycle() {
        let err = claim_err(r#"claim = { lifecycle = "start", detail = "permission" }"#);
        assert!(
            err.contains("detail"),
            "detail on a lifecycle claim must error: {err}"
        );
    }

    #[test]
    fn claim_rejects_empty_table() {
        let err = claim_err(r#"claim = {}"#);
        assert!(
            err.contains("state") && err.contains("lifecycle"),
            "an empty claim must ask for state or lifecycle: {err}"
        );
    }
}
