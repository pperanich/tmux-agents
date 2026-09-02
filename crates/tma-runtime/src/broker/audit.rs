//! The `[act] log` record: one JSON line per [`fire`](super::fire), whatever the outcome. The
//! notify log answers "what did tma tell me about"; this one answers the question no surface could
//! answer before, "which surface fired this action into this pane, and when". That is why `source`
//! is a first-class field rather than a nicety: an audit line that records only "approved" cannot
//! tell a human at a menu from a script from an agent shelling out to `tma act`, which is exactly
//! the question a tool with four fire surfaces raises. Claude Code's `claude_code.tool_decision` and
//! Codex's `codex.tool_decision` both carry a decision source for the same reason.
//!
//! The redaction rule is the notify log's, unchanged: no key, no token, no pane title, and no
//! agent-supplied prose. `@agent_pending_summary` is agent-supplied text and stays out; the tool
//! name and call id are tma's own tokens and go in, so a line says *what* was pending without
//! quoting it.

use std::path::Path;

use tma_core::ActionKind;

use crate::json::JsonWriter;

/// The act-log schema version. Independent of the `--json` result schema: this is a file format a
/// reader parses over months, and it versions on its own cadence.
pub const ACT_LOG_SCHEMA: i64 = 1;

/// Consecutive same-action fires in one episode that raise the mis-tap warning. Claude Code's auto
/// mode pauses when its classifier "blocks an action 3 times in a row"
/// (<https://code.claude.com/docs/en/permission-modes>) and Codex's auto-review circuit breaker
/// aborts the turn at three consecutive denials; the number is theirs. tma only warns: a repeat is
/// a signal that the agent may be re-asking, not a policy the broker is entitled to enforce.
pub const REPEAT_WARN: u32 = 3;

/// Which surface asked for the fire. Closed vocabulary, written verbatim into the line's `source`.
/// `keybinding` is deliberately absent: every bundled binding opens the action menu rather than
/// firing directly, so a bound key reaches the broker as `menu` and a value for it would always be
/// a lie.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActSource {
    /// An interactive `tma act` on a TTY: a person typed it.
    #[default]
    Cli,
    /// `tma act --yes`, or a `tma act` with no TTY to prompt on: a script, a hook, or an agent.
    CliYes,
    /// The tmux `display-menu` entry (`tma act --menu`, and the `a` key in the dashboards).
    Menu,
}

impl ActSource {
    pub const fn token(self) -> &'static str {
        match self {
            ActSource::Cli => "cli",
            ActSource::CliYes => "cli-yes",
            ActSource::Menu => "menu",
        }
    }

    /// Decode the `TMA_ACT_SOURCE` seam a menu entry sets on its `run-shell` command line. An
    /// unknown value decodes to `None` so the caller falls back to what it can prove about itself,
    /// rather than trusting an arbitrary string into the audit vocabulary.
    pub fn parse(token: &str) -> Option<ActSource> {
        match token {
            "cli" => Some(ActSource::Cli),
            "cli-yes" => Some(ActSource::CliYes),
            "menu" => Some(ActSource::Menu),
            _ => None,
        }
    }
}

/// Where the line goes and what the invocation knows about itself. [`AuditCtx::default`] writes
/// nothing, which is what every caller that is not the `tma act` CLI wants.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuditCtx<'a> {
    /// `[act] log`, `None` when unconfigured (no line is written).
    pub log: Option<&'a Path>,
    pub source: ActSource,
    /// `--all`: this fire is one of a fan-out.
    pub all: bool,
    /// The id shared by every fire of one `--all` invocation, `None` for a single fire.
    pub batch: Option<&'a str>,
}

/// What the broker observed about the pane while it held the lock. Separate from [`super::ActResult`]
/// because it is audit material, not a return value: the CLI never renders it, and only the fire
/// path that actually took the lock can fill it in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActObserved {
    /// `@agent_name`, `None` when the pane carried no agent stamp (an identity refusal).
    pub agent: Option<String>,
    /// The pane's episode instant, `None` when the pane was never read (it vanished first).
    pub episode_ms: Option<u64>,
    /// `@agent_pending_tool` / `@agent_pending_call`: the pending call's identity, never its text.
    pub pending_tool: Option<String>,
    pub pending_call: Option<String>,
    /// Consecutive fires of this action on this pane in this episode, counting this one. `0` when
    /// the fire never reached the effect, so a refusal contributes nothing to the run.
    pub repeat: u32,
}

/// The `kind` token: the transport the fire actually used. An `api` line is a `keys` action whose
/// agent answers over HTTP, which is a different effect on the world and so a different token.
pub fn kind_token(kind: ActionKind, api: bool) -> &'static str {
    match (kind, api) {
        (ActionKind::Keys, true) => "api",
        (ActionKind::Keys, false) => "keys",
        (ActionKind::Exec, _) => "exec",
    }
}

/// One `[act] log` line. `at` is the fire's completion instant; every other field is either the
/// invocation's own (`action`, `source`, `all`, `batch`) or read from the pane under the lock.
#[allow(clippy::too_many_arguments)]
pub fn act_log_line(
    at: u64,
    pane: &str,
    action: &str,
    kind: &str,
    outcome: &str,
    reason: Option<&str>,
    obs: &ActObserved,
    ctx: &AuditCtx,
) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", ACT_LOG_SCHEMA);
    j.number("at", at as i64);
    j.string("pane", pane);
    opt_string(&mut j, "agent", obs.agent.as_deref());
    j.string("action", action);
    j.string("kind", kind);
    j.string("outcome", outcome);
    opt_string(&mut j, "reason", reason);
    j.string("source", ctx.source.token());
    match obs.episode_ms {
        Some(ms) => j.number("episode_ms", ms as i64),
        None => j.null("episode_ms"),
    }
    j.number("repeat", obs.repeat as i64);
    opt_string(&mut j, "pending_tool", obs.pending_tool.as_deref());
    opt_string(&mut j, "pending_call", obs.pending_call.as_deref());
    j.bool("all", ctx.all);
    opt_string(&mut j, "batch", ctx.batch);
    j.end_object();
    j.finish()
}

fn opt_string(j: &mut JsonWriter, key: &str, value: Option<&str>) {
    match value {
        Some(v) => j.string(key, v),
        None => j.null(key),
    }
}

/// The next `@agent_act_repeat` value and the count it carries. The stored form is
/// `<episode_ms>:<action>:<count>`: a different episode or a different action starts the run over,
/// which is what makes the counter a *consecutive* one. An unparseable value is treated as absent
/// rather than repaired, so a hand-edited option costs one undercount and nothing else.
pub fn next_repeat(stored: Option<&str>, episode_ms: u64, action: &str) -> (String, u32) {
    let prior = stored
        .and_then(|s| parse_repeat(s, episode_ms, action))
        .unwrap_or(0);
    let count = prior.saturating_add(1);
    (format!("{episode_ms}:{action}:{count}"), count)
}

/// The stored count when it belongs to this episode and this action, else `None`. An action name
/// holds no `:` (the manifest stem charset), so a plain two-way split is unambiguous.
fn parse_repeat(stored: &str, episode_ms: u64, action: &str) -> Option<u32> {
    let (episode, rest) = stored.split_once(':')?;
    let (name, count) = rest.split_once(':')?;
    if episode.parse::<u64>().ok()? != episode_ms || name != action {
        return None;
    }
    count.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs() -> ActObserved {
        ActObserved {
            agent: Some("claude".to_string()),
            episode_ms: Some(1_700_000_000_000),
            pending_tool: Some("Bash".to_string()),
            pending_call: Some("toolu_01".to_string()),
            repeat: 1,
        }
    }

    /// The pinned key set. A dropped or renamed key breaks every `jq` filter written against the
    /// file, and a NEW key is the failure that matters: this is where a pending summary or a title
    /// would leak in.
    #[test]
    fn the_line_carries_exactly_the_pinned_keys() {
        let line = act_log_line(
            1_700_000_000_500,
            "%5",
            "approve",
            "keys",
            "sent",
            None,
            &obs(),
            &AuditCtx {
                source: ActSource::Menu,
                all: true,
                batch: Some("b1"),
                ..AuditCtx::default()
            },
        );
        assert_eq!(
            line,
            r#"{"schema":1,"at":1700000000500,"pane":"%5","agent":"claude","action":"approve","kind":"keys","outcome":"sent","reason":null,"source":"menu","episode_ms":1700000000000,"repeat":1,"pending_tool":"Bash","pending_call":"toolu_01","all":true,"batch":"b1"}"#
        );
    }

    /// A refusal records the reason token and a pane that vanished before any read records nulls,
    /// so a reader never has to guess whether a missing field means "absent" or "not applicable".
    #[test]
    fn a_refusal_and_a_vanish_still_write_a_line() {
        let refused = act_log_line(
            1,
            "%5",
            "approve",
            "keys",
            "refused",
            Some("gated"),
            &ActObserved { repeat: 0, ..obs() },
            &AuditCtx::default(),
        );
        assert!(refused.contains(r#""outcome":"refused","reason":"gated""#));
        assert!(refused.contains(r#""source":"cli""#));
        assert!(refused.contains(r#""repeat":0"#));

        let vanished = act_log_line(
            1,
            "%5",
            "approve",
            "keys",
            "vanished",
            Some("pane-gone"),
            &ActObserved::default(),
            &AuditCtx::default(),
        );
        assert!(vanished.contains(r#""agent":null"#));
        assert!(vanished.contains(r#""episode_ms":null"#));
        assert!(vanished.contains(r#""batch":null"#));
    }

    #[test]
    fn the_repeat_run_counts_within_an_episode_and_resets_outside_it() {
        let (v1, n1) = next_repeat(None, 100, "approve");
        assert_eq!((v1.as_str(), n1), ("100:approve:1", 1));
        let (v2, n2) = next_repeat(Some(&v1), 100, "approve");
        assert_eq!((v2.as_str(), n2), ("100:approve:2", 2));
        let (v3, n3) = next_repeat(Some(&v2), 100, "approve");
        assert_eq!(n3, REPEAT_WARN, "the third consecutive fire is the warning");

        // A different action in the same episode is a different run, not a continuation.
        assert_eq!(next_repeat(Some(&v3), 100, "deny").1, 1);
        // A new episode restarts the run even for the same action.
        assert_eq!(next_repeat(Some(&v3), 200, "approve").1, 1);
        // Garbage in the option undercounts once rather than propagating.
        assert_eq!(next_repeat(Some("nonsense"), 100, "approve").1, 1);
        assert_eq!(next_repeat(Some("100:approve:x"), 100, "approve").1, 1);
    }

    #[test]
    fn source_tokens_round_trip_and_reject_anything_else() {
        for s in [ActSource::Cli, ActSource::CliYes, ActSource::Menu] {
            assert_eq!(ActSource::parse(s.token()), Some(s));
        }
        assert_eq!(ActSource::parse("device"), None);
        assert_eq!(ActSource::parse(""), None);
    }

    #[test]
    fn kind_names_the_transport_not_just_the_manifest_kind() {
        assert_eq!(kind_token(ActionKind::Keys, false), "keys");
        assert_eq!(kind_token(ActionKind::Keys, true), "api");
        assert_eq!(kind_token(ActionKind::Exec, false), "exec");
    }
}
