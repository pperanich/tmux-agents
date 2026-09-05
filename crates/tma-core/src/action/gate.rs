//! Applicability and the pure `when`/`requires` gate evaluation over a pane snapshot row: the gate
//! vocabulary (`When`, `Requirement`, `GateInput`, `GateOutcome`, `RefusalReason`) and the
//! `ActionManifest` methods that read a parsed manifest against a row. No I/O, no parsing.

use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::Deserialize;

use crate::state::{AgentState, Detail};

use super::{ActionKind, ActionManifest, ApiTransport, TextRefusal, TextTransport, TEXT_MAX_BYTES};

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
    /// its `[keys]` table, a `text` action from `[text]`, an `exec` action from `agents`
    /// (empty = all).
    pub fn applies_to(&self, agent: &str) -> bool {
        match self.kind {
            // Applicability is the union of the two transport tables; exclusivity is a
            // parse rule, so at most one of them ever covers a given agent.
            ActionKind::Keys => self.keys.contains_key(agent) || self.api.contains_key(agent),
            ActionKind::Exec => self.agents.is_empty() || self.agents.iter().any(|a| a == agent),
            ActionKind::Text => self.text.contains_key(agent),
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

    /// The `text` transport for `agent`, or `None` when the action does not cover it (or is not
    /// `kind = "text"`).
    pub fn text_for(&self, agent: &str) -> Option<&TextTransport> {
        self.text.get(agent)
    }

    /// Check a caller's `text` payload against the host-side rules, before anything is sent. In
    /// reporting order: nothing to send, over [`TEXT_MAX_BYTES`], any control character (C0, DEL or
    /// C1 alike, so a `\r` cannot submit early and an `\x1b` cannot escape), then a leading declared
    /// sigil after optional whitespace, which the agent would read as its own command rather than as
    /// a message. `Ok(())` means the string may be delivered literally.
    pub fn check_text(&self, text: &str) -> Result<(), TextRefusal> {
        let trimmed = text.trim_start();
        if trimmed.trim_end().is_empty() {
            return Err(TextRefusal::Empty);
        }
        if text.len() > TEXT_MAX_BYTES {
            return Err(TextRefusal::TooLong);
        }
        if text.chars().any(char::is_control) {
            return Err(TextRefusal::ControlBytes);
        }
        if trimmed
            .chars()
            .next()
            .is_some_and(|c| self.sigils.contains(&c))
        {
            return Err(TextRefusal::Sigil);
        }
        Ok(())
    }

    /// Evaluate applicability, the `when` gate, and `requires` against a pane snapshot row.
    ///
    /// Reasons are reported most-permanent first, so a surface grays a permanently-unfireable
    /// action differently from a transiently-gated one: `wrong-agent` (never applies) then
    /// `no-coverage` (bound on an agent with no telemetry) then `requires-unmet` (a context key is
    /// empty; not skippable even by `--force`) then `gated` (state/detail/metric, the only
    /// `--force`-skippable refusal).
    ///
    /// A `text` action carries one more `gated` condition on top of `when`: an agent that has not
    /// declared `steer_now` refuses while the pane is `working`, because tma cannot tell a queue
    /// from a swallowed message.
    pub fn evaluate_gate(&self, input: &GateInput) -> GateOutcome {
        match self.when_gate(input) {
            GateOutcome::Fireable if self.steer_now_refused(input) => {
                GateOutcome::Refused(RefusalReason::Gated)
            }
            outcome => outcome,
        }
    }

    /// Text into a `working` pane, where the agent's transport does not declare `steer_now`. The
    /// message would land somewhere tma cannot see, so the action is not offered there at all.
    fn steer_now_refused(&self, input: &GateInput) -> bool {
        input.state == AgentState::Working
            && self.text_for(input.agent).is_some_and(|t| !t.steer_now)
    }

    /// The `when` + `requires` half of [`ActionManifest::evaluate_gate`].
    fn when_gate(&self, input: &GateInput) -> GateOutcome {
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

    // ---- text payload + steer_now ------------------------------------------------

    fn text_action(extra: &str, table: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"steer\"\nlabel = \"Steer\"\nkind = \"text\"\n{extra}\n[text]\n{table}\n"
        );
        ActionManifest::parse(&src, "steer", "steer.toml").unwrap()
    }

    #[test]
    fn payload_rules_refuse_in_reporting_order() {
        let a = text_action("", "claude = { suffix = [\"Enter\"] }");
        assert_eq!(a.check_text("write the tests first"), Ok(()));
        // A leading `-` is data, not a flag, and reaches the pane.
        assert_eq!(a.check_text("-not-a-flag"), Ok(()));
        // The named-key spellings are prose here, which is the whole point of the literal send.
        assert_eq!(a.check_text("Enter"), Ok(()));
        assert_eq!(a.check_text("C-c"), Ok(()));

        assert_eq!(a.check_text(""), Err(TextRefusal::Empty));
        assert_eq!(a.check_text("   "), Err(TextRefusal::Empty));
        assert_eq!(
            a.check_text(&"x".repeat(TEXT_MAX_BYTES + 1)),
            Err(TextRefusal::TooLong)
        );
        assert_eq!(a.check_text(&"x".repeat(TEXT_MAX_BYTES)), Ok(()));
        for bad in ["one\rtwo", "one\ntwo", "one\ttwo", "esc\x1bhere", "del\x7f"] {
            assert_eq!(a.check_text(bad), Err(TextRefusal::ControlBytes), "{bad:?}");
        }
        // C1 too, which arrives as two UTF-8 bytes rather than one.
        assert_eq!(
            a.check_text("c1\u{0085}here"),
            Err(TextRefusal::ControlBytes)
        );
        for bad in ["/clear", "  /compact", "!ls"] {
            assert_eq!(a.check_text(bad), Err(TextRefusal::Sigil), "{bad:?}");
        }
        // Not a declared sigil by default, and not a leading one either.
        assert_eq!(a.check_text("@notes"), Ok(()));
        assert_eq!(a.check_text("run ./x !now"), Ok(()));
    }

    #[test]
    fn declared_sigils_replace_the_default_list() {
        let a = text_action("sigils = [\"@\"]", "claude = {}");
        assert_eq!(a.check_text("@notes"), Err(TextRefusal::Sigil));
        assert_eq!(a.check_text("/clear"), Ok(()), "no longer declared");
    }

    /// An agent that has not declared `steer_now` refuses while the pane is working even when
    /// `when` allows it: gemini queues the text and then hands it back to the composer, unsent.
    #[test]
    fn steer_now_is_required_to_send_text_into_a_working_pane() {
        let quiet = text_action("", "claude = { suffix = [\"Enter\"] }");
        assert_eq!(
            quiet.evaluate_gate(&input("claude", AgentState::Working)),
            GateOutcome::Refused(RefusalReason::Gated)
        );
        assert_eq!(
            quiet.evaluate_gate(&input("claude", AgentState::Idle)),
            GateOutcome::Fireable
        );

        let now = text_action(
            "when = { state = [\"working\"] }",
            "claude = { suffix = [\"Enter\"], steer_now = true }",
        );
        assert_eq!(
            now.evaluate_gate(&input("claude", AgentState::Working)),
            GateOutcome::Fireable
        );
        // The `when` gate still runs first: an idle pane is refused by it, not by steer_now.
        assert_eq!(
            now.evaluate_gate(&input("claude", AgentState::Idle)),
            GateOutcome::Refused(RefusalReason::Gated)
        );
    }

    /// A blocked pane refuses on the ordinary state gate, whatever its detail says.
    #[test]
    fn text_at_a_blocked_pane_is_gated() {
        let a = text_action("when = { state = [\"idle\"] }", "claude = {}");
        for detail in ["permission", "awaiting-text"] {
            let refused = GateInput {
                detail: Some(detail),
                ..input("claude", AgentState::Blocked)
            };
            assert_eq!(
                a.evaluate_gate(&refused),
                GateOutcome::Refused(RefusalReason::Gated),
                "blocked/{detail}"
            );
        }
    }

    #[test]
    fn reason_tokens_match_vocabulary() {
        assert_eq!(RefusalReason::WrongAgent.token(), "wrong-agent");
        assert_eq!(RefusalReason::NoCoverage.token(), "no-coverage");
        assert_eq!(RefusalReason::RequiresUnmet.token(), "requires-unmet");
        assert_eq!(RefusalReason::Gated.token(), "gated");
    }
}
