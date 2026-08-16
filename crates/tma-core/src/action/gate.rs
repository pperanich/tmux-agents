//! Applicability and the pure `when`/`requires` gate evaluation over a pane snapshot row: the gate
//! vocabulary (`When`, `Requirement`, `GateInput`, `GateOutcome`, `RefusalReason`) and the
//! `ActionManifest` methods that read a parsed manifest against a row. No I/O, no parsing.

use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::Deserialize;

use crate::state::{AgentState, Detail};

use super::{ActionKind, ActionManifest, ApiTransport};

/// The optional `when` gate. All present keys are ANDed; context bounds fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct When {
    /// Fire only when the pane's state is one of these. Empty = state not gated.
    pub state: Vec<AgentState>,
    /// Fire only when the pane's detail token is one of these. Empty = detail not gated.
    pub detail: Vec<Detail>,
    /// Lower bound (inclusive) on context utilization percent.
    pub context_pct_min: Option<u8>,
    /// Upper bound (inclusive) on context utilization percent.
    pub context_pct_max: Option<u8>,
}

impl When {
    /// Whether the gate reads the context metric at all.
    fn has_context_bound(&self) -> bool {
        self.context_pct_min.is_some() || self.context_pct_max.is_some()
    }
}

/// The closed `requires` vocabulary: each token names a context key that must be
/// non-empty for the gate to pass, so a script never half-runs on a missing value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// `@agent_session` ⇒ `TMA_SESSION_ID`.
    Session,
    /// `#{pane_current_path}` ⇒ `TMA_CWD`.
    Cwd,
    /// `@agent_pid` ⇒ `TMA_PID`.
    Pid,
    /// `#{pane_title}` ⇒ `TMA_TITLE`.
    Title,
}

impl Requirement {
    /// The `requires` token spelling.
    pub const fn token(self) -> &'static str {
        match self {
            Requirement::Session => "session",
            Requirement::Cwd => "cwd",
            Requirement::Pid => "pid",
            Requirement::Title => "title",
        }
    }
}

impl FromStr for Requirement {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session" => Ok(Requirement::Session),
            "cwd" => Ok(Requirement::Cwd),
            "pid" => Ok(Requirement::Pid),
            "title" => Ok(Requirement::Title),
            other => Err(format!(
                "unknown requires token {other:?} (expected session, cwd, pid, or title)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Requirement {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// Which `requires` context keys are currently non-empty for a pane. The broker fills this from the
/// pane's stamped options and tmux formats; the gate reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextKeys {
    pub session: bool,
    pub cwd: bool,
    pub pid: bool,
    pub title: bool,
}

impl ContextKeys {
    fn has(self, req: Requirement) -> bool {
        match req {
            Requirement::Session => self.session,
            Requirement::Cwd => self.cwd,
            Requirement::Pid => self.pid,
            Requirement::Title => self.title,
        }
    }
}

/// One pane's snapshot row, the pure input to gate evaluation. The broker builds it from stamped
/// pane options; `--list` builds it the same way, so both see one verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateInput<'a> {
    /// `@agent_name`.
    pub agent: &'a str,
    /// `@agent_state` at evaluation time.
    pub state: AgentState,
    /// `@agent_detail`, `None` when absent.
    pub detail: Option<&'a str>,
    /// `@agent_context_pct`, `None` when the metric is absent right now.
    pub context_pct: Option<u8>,
    /// Whether the pane's agent manifest declares a context telemetry channel. Distinguishes
    /// `no-coverage` (false, permanent) from `gated` (true, metric merely absent).
    pub context_covered: bool,
    /// Which `requires` keys are currently non-empty for the pane.
    pub context_keys: ContextKeys,
}

/// The outcome of evaluating an action against a snapshot row: fireable, or refused with a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    Fireable,
    Refused(RefusalReason),
}

/// Why a gate refused. A closed vocabulary shared with `tma act --list` and the broker;
/// `locked` is a broker-time verdict, not a gate outcome, so it is not represented here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// The action does not apply to this pane's agent.
    WrongAgent,
    /// A context bound reads a metric this agent declares no telemetry channel for — permanent.
    NoCoverage,
    /// A required context key (`requires`) is empty.
    RequiresUnmet,
    /// `when` is unsatisfied right now: wrong state/detail, or the metric is absent or out of range.
    Gated,
}

impl RefusalReason {
    /// The reason token, matching the `tma act --list` / exit-code vocabulary.
    pub const fn token(self) -> &'static str {
        match self {
            RefusalReason::WrongAgent => "wrong-agent",
            RefusalReason::NoCoverage => "no-coverage",
            RefusalReason::RequiresUnmet => "requires-unmet",
            RefusalReason::Gated => "gated",
        }
    }
}

impl ActionManifest {
    /// Whether this action applies to `agent` at all (applicability): a `keys` action from
    /// its `[keys]` table, an `exec` action from `agents` (empty = all).
    pub fn applies_to(&self, agent: &str) -> bool {
        match self.kind {
            // Applicability is the union of the two transport tables; exclusivity is a
            // parse rule, so at most one of them ever covers a given agent.
            ActionKind::Keys => self.keys.contains_key(agent) || self.api.contains_key(agent),
            ActionKind::Exec => self.agents.is_empty() || self.agents.iter().any(|a| a == agent),
        }
    }

    /// The `keys` sequence for `agent`, or `None` when the action does not cover it (or is `exec`).
    pub fn keys_for(&self, agent: &str) -> Option<&[String]> {
        self.keys.get(agent).map(Vec::as_slice)
    }

    /// The API-channel transport for `agent`, or `None` when the agent is keys-covered or
    /// uncovered. Exclusivity is enforced at parse, so this never overlaps [`ActionManifest::keys_for`].
    pub fn api_for(&self, agent: &str) -> Option<&ApiTransport> {
        self.api.get(agent)
    }

    /// Evaluate applicability, the `when` gate, and `requires` against a pane snapshot row.
    ///
    /// Reasons are reported most-permanent first, so a surface grays a permanently-unfireable
    /// action differently from a transiently-gated one: `wrong-agent` (never applies) then
    /// `no-coverage` (bound on an agent with no telemetry) then `requires-unmet` (a context key is
    /// empty; not skippable even by `--force`) then `gated` (state/detail/metric, the only
    /// `--force`-skippable refusal).
    pub fn evaluate_gate(&self, input: &GateInput) -> GateOutcome {
        if !self.applies_to(input.agent) {
            return GateOutcome::Refused(RefusalReason::WrongAgent);
        }
        let Some(when) = &self.when else {
            return self.check_requires(input);
        };

        // Fail closed on the context metric before the transient state/detail checks: an absent
        // channel is permanent, so it must win over a state that could later satisfy the gate.
        if when.has_context_bound() && !input.context_covered {
            return GateOutcome::Refused(RefusalReason::NoCoverage);
        }
        if let GateOutcome::Refused(r) = self.check_requires(input) {
            return GateOutcome::Refused(r);
        }
        if !when.state.is_empty() && !when.state.contains(&input.state) {
            return GateOutcome::Refused(RefusalReason::Gated);
        }
        if !when.detail.is_empty() {
            let matched = input
                .detail
                .is_some_and(|d| when.detail.iter().any(|w| w.as_str() == d));
            if !matched {
                return GateOutcome::Refused(RefusalReason::Gated);
            }
        }
        if when.has_context_bound() {
            // Covered here (checked above), so an absent value is the metric merely not observed
            // yet: `gated`, not `no-coverage`.
            let Some(pct) = input.context_pct else {
                return GateOutcome::Refused(RefusalReason::Gated);
            };
            if when.context_pct_min.is_some_and(|min| pct < min)
                || when.context_pct_max.is_some_and(|max| pct > max)
            {
                return GateOutcome::Refused(RefusalReason::Gated);
            }
        }
        GateOutcome::Fireable
    }

    fn check_requires(&self, input: &GateInput) -> GateOutcome {
        if self.requires.iter().all(|&r| input.context_keys.has(r)) {
            GateOutcome::Fireable
        } else {
            GateOutcome::Refused(RefusalReason::RequiresUnmet)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(agent: &'a str, state: AgentState) -> GateInput<'a> {
        GateInput {
            agent,
            state,
            detail: None,
            context_pct: None,
            context_covered: false,
            context_keys: ContextKeys::default(),
        }
    }

    // ---- gate evaluation ---------------------------------------------------------

    fn keys_action(when: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"a\"\nlabel = \"A\"\nkind = \"keys\"\n{when}\n[keys]\nclaude = [\"1\"]\n"
        );
        ActionManifest::parse(&src, "a", "a.toml").unwrap()
    }

    #[test]
    fn wrong_agent_when_not_applicable() {
        let a = keys_action("");
        let out = a.evaluate_gate(&input("codex", AgentState::Idle));
        assert_eq!(out, GateOutcome::Refused(RefusalReason::WrongAgent));
    }

    #[test]
    fn no_when_is_fireable_for_applicable_agent() {
        let a = keys_action("");
        assert_eq!(
            a.evaluate_gate(&input("claude", AgentState::Working)),
            GateOutcome::Fireable
        );
    }

    #[test]
    fn state_gate_ands_and_refuses_gated() {
        let a = keys_action("when = { state = [\"blocked\"], detail = [\"permission\"] }");
        // state satisfied, detail satisfied ⇒ fireable.
        let ok = GateInput {
            detail: Some("permission"),
            ..input("claude", AgentState::Blocked)
        };
        assert_eq!(a.evaluate_gate(&ok), GateOutcome::Fireable);
        // state wrong ⇒ gated.
        assert_eq!(
            a.evaluate_gate(&GateInput {
                detail: Some("permission"),
                ..input("claude", AgentState::Idle)
            }),
            GateOutcome::Refused(RefusalReason::Gated)
        );
        // detail wrong (AND semantics) ⇒ gated even though state matches.
        assert_eq!(
            a.evaluate_gate(&GateInput {
                detail: Some("question"),
                ..input("claude", AgentState::Blocked)
            }),
            GateOutcome::Refused(RefusalReason::Gated)
        );
        // detail absent against a detail gate ⇒ gated.
        assert_eq!(
            a.evaluate_gate(&input("claude", AgentState::Blocked)),
            GateOutcome::Refused(RefusalReason::Gated)
        );
    }

    #[test]
    fn context_bound_fails_closed_no_coverage_vs_gated() {
        let a = keys_action("when = { state = [\"idle\"], context_pct_min = 75 }");
        // No telemetry channel at all ⇒ no-coverage (permanent), wins over the idle state check.
        assert_eq!(
            a.evaluate_gate(&input("claude", AgentState::Working)),
            GateOutcome::Refused(RefusalReason::NoCoverage)
        );
        // Channel present but metric absent right now ⇒ gated.
        let covered_absent = GateInput {
            context_covered: true,
            ..input("claude", AgentState::Idle)
        };
        assert_eq!(
            a.evaluate_gate(&covered_absent),
            GateOutcome::Refused(RefusalReason::Gated)
        );
        // Metric present but below the bound ⇒ gated.
        let below = GateInput {
            context_covered: true,
            context_pct: Some(50),
            ..input("claude", AgentState::Idle)
        };
        assert_eq!(
            a.evaluate_gate(&below),
            GateOutcome::Refused(RefusalReason::Gated)
        );
        // At the inclusive bound and idle ⇒ fireable.
        let at_bound = GateInput {
            context_covered: true,
            context_pct: Some(75),
            ..input("claude", AgentState::Idle)
        };
        assert_eq!(a.evaluate_gate(&at_bound), GateOutcome::Fireable);
    }

    #[test]
    fn requires_unmet_on_empty_session() {
        let src = r#"
min_engine_version = "0.1"
name = "s"
label = "S"
kind = "exec"
requires = ["session"]
command = "echo hi"
"#;
        let a = ActionManifest::parse(src, "s", "s.toml").unwrap();
        // Session absent ⇒ requires-unmet.
        assert_eq!(
            a.evaluate_gate(&input("claude", AgentState::Working)),
            GateOutcome::Refused(RefusalReason::RequiresUnmet)
        );
        // Session present ⇒ fireable.
        let with_session = GateInput {
            context_keys: ContextKeys {
                session: true,
                ..ContextKeys::default()
            },
            ..input("claude", AgentState::Working)
        };
        assert_eq!(a.evaluate_gate(&with_session), GateOutcome::Fireable);
    }

    #[test]
    fn no_coverage_outranks_requires_unmet() {
        let src = r#"
min_engine_version = "0.1"
name = "c"
label = "C"
kind = "exec"
requires = ["session"]
when = { context_pct_min = 75 }
command = "echo hi"
"#;
        let a = ActionManifest::parse(src, "c", "c.toml").unwrap();
        // Both no telemetry and no session ⇒ the permanent no-coverage wins.
        assert_eq!(
            a.evaluate_gate(&input("claude", AgentState::Idle)),
            GateOutcome::Refused(RefusalReason::NoCoverage)
        );
    }

    #[test]
    fn reason_tokens_match_vocabulary() {
        assert_eq!(RefusalReason::WrongAgent.token(), "wrong-agent");
        assert_eq!(RefusalReason::NoCoverage.token(), "no-coverage");
        assert_eq!(RefusalReason::RequiresUnmet.token(), "requires-unmet");
        assert_eq!(RefusalReason::Gated.token(), "gated");
    }
}
