//! The validating loader: the raw TOML shape (`RawAction` and friends), `ActionManifest::parse`,
//! the per-kind structural rules (`StructuralRule`), and `when` validation. Turns a manifest source
//! into a validated [`ActionManifest`] or an [`ActionError`]; the schema types and error boundary
//! live in the parent, the gate vocabulary in `gate`.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::manifest::{is_safe_token, Version};
use crate::state::{AgentState, Detail};

use super::gate::{Requirement, When};
use super::{ActionError, ActionKind, ActionManifest, ApiOp, ApiReply, ApiTransport};

/// Default synchronous execution / lock-expiry bound for an exec action, in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default detached execution / lock-expiry bound for a `detach = true` exec action.
const DEFAULT_DETACH_TIMEOUT_MS: u64 = 900_000;

impl ActionManifest {
    /// Parse and validate an action manifest. `stem` is the filename stem the loader resolved (the
    /// `name` must equal it); `file` names the source for error messages.
    pub fn parse(toml_src: &str, stem: &str, file: &str) -> Result<ActionManifest, ActionError> {
        // Gate the engine version before the strict parse, so a newer-schema manifest is told to
        // upgrade rather than seeing a confusing "unknown field" (mirrors the agent manifest loader).
        if let Ok(probe) = toml::from_str::<VersionProbe>(toml_src) {
            if let Ok(min) = probe.min_engine_version.parse::<Version>() {
                let engine = Version::engine();
                if min > engine {
                    return Err(ActionError::EngineTooOld {
                        file: file.to_string(),
                        required: min,
                        engine,
                    });
                }
            }
        }

        let raw: RawAction = toml::from_str(toml_src).map_err(|source| ActionError::Parse {
            file: file.to_string(),
            source,
        })?;

        let min_engine_version = raw
            .min_engine_version
            .parse::<Version>()
            .map_err(|reason| ActionError::BadVersion {
                file: file.to_string(),
                reason,
            })?;
        let engine = Version::engine();
        if min_engine_version > engine {
            return Err(ActionError::EngineTooOld {
                file: file.to_string(),
                required: min_engine_version,
                engine,
            });
        }

        // Name is a safe token (rides the lock value) and must equal the filename stem, so
        // shadowing and invocation share one key.
        if !is_safe_token(&raw.name) {
            return Err(ActionError::BadToken {
                file: file.to_string(),
                field: "name",
                token: raw.name,
            });
        }
        if raw.name != stem {
            return Err(ActionError::NameMismatch {
                file: file.to_string(),
                name: raw.name,
                stem: stem.to_string(),
            });
        }

        let kind = match raw.kind {
            RawKind::Keys => ActionKind::Keys,
            RawKind::Exec => ActionKind::Exec,
        };

        // Per-kind structural rules.
        let structural = |rule| ActionError::Structural {
            file: file.to_string(),
            rule,
        };
        match kind {
            ActionKind::Keys => {
                // A `keys` action needs at least one transport across `[keys]` and `[api]`
                // (an api-only action is legal; both empty stays a parse error).
                if raw.keys.is_empty() && raw.api.is_empty() {
                    return Err(structural(StructuralRule::KeysEmpty));
                }
                if raw.command.is_some() {
                    return Err(structural(StructuralRule::KeysForbidsCommand));
                }
                if raw.detach.is_some() {
                    return Err(structural(StructuralRule::KeysForbidsDetach));
                }
                if !raw.agents.is_empty() {
                    return Err(structural(StructuralRule::KeysForbidsAgents));
                }
                // Exclusivity: one agent in both tables would leave the broker no way to pick a
                // transport at act time, so it is a parse error, not a silent fallback.
                if let Some(agent) = raw.api.keys().find(|a| raw.keys.contains_key(*a)) {
                    return Err(ActionError::AgentInBothTransports {
                        file: file.to_string(),
                        agent: agent.clone(),
                    });
                }
            }
            ActionKind::Exec => {
                if raw.command.is_none() {
                    return Err(structural(StructuralRule::ExecNeedsCommand));
                }
                if !raw.keys.is_empty() {
                    return Err(structural(StructuralRule::ExecForbidsKeys));
                }
                if !raw.api.is_empty() {
                    return Err(structural(StructuralRule::ExecForbidsApi));
                }
            }
        }

        // Agent-name tokens (keys keys, api keys, and `agents` entries) obey the safe-token rules.
        for agent in raw.keys.keys() {
            if !is_safe_token(agent) {
                return Err(ActionError::BadToken {
                    file: file.to_string(),
                    field: "[keys] agent",
                    token: agent.clone(),
                });
            }
        }
        for agent in raw.api.keys() {
            if !is_safe_token(agent) {
                return Err(ActionError::BadToken {
                    file: file.to_string(),
                    field: "[api] agent",
                    token: agent.clone(),
                });
            }
        }
        for agent in &raw.agents {
            if !is_safe_token(agent) {
                return Err(ActionError::BadToken {
                    file: file.to_string(),
                    field: "agents",
                    token: agent.clone(),
                });
            }
        }

        let when = validate_when(raw.when, file)?;

        Ok(ActionManifest {
            min_engine_version,
            name: raw.name,
            label: raw.label,
            kind,
            when,
            agents: raw.agents,
            requires: raw.requires,
            confirm: raw.confirm.unwrap_or(false),
            detach: raw.detach.unwrap_or(false),
            timeout_ms: raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            detach_timeout_ms: raw.detach_timeout_ms.unwrap_or(DEFAULT_DETACH_TIMEOUT_MS),
            command: raw.command,
            keys: raw.keys,
            api: raw
                .api
                .into_iter()
                .map(|(agent, t)| {
                    (
                        agent,
                        ApiTransport {
                            op: t.op,
                            reply: t.reply,
                        },
                    )
                })
                .collect(),
        })
    }
}

/// A lenient probe capturing only `min_engine_version`, parsed before the strict shape so a
/// newer-schema manifest surfaces as an upgrade error, not "unknown field".
#[derive(Deserialize)]
struct VersionProbe {
    min_engine_version: String,
}

/// The raw TOML shape, deserialized before semantic validation. `command`, `detach`, `confirm`,
/// and the timeout fields are `Option` so their mere presence can be rejected per kind and their
/// defaults applied only when absent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    min_engine_version: String,
    name: String,
    label: String,
    kind: RawKind,
    #[serde(default)]
    when: Option<RawWhen>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    requires: Vec<Requirement>,
    #[serde(default)]
    confirm: Option<bool>,
    #[serde(default)]
    detach: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    detach_timeout_ms: Option<u64>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    keys: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    api: BTreeMap<String, RawApiTransport>,
}

/// The raw `[api]` per-agent transport. `op` and `reply` are closed serde enums, so an unknown
/// operation or reply value (or a missing `reply`) surfaces as a parse error.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApiTransport {
    op: ApiOp,
    reply: ApiReply,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawKind {
    Keys,
    Exec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWhen {
    #[serde(default)]
    state: Vec<AgentState>,
    #[serde(default)]
    detail: Vec<Detail>,
    #[serde(default)]
    context_pct_min: Option<u8>,
    #[serde(default)]
    context_pct_max: Option<u8>,
}

/// Validate the optional `when` gate: detail tokens are safe tokens, context bounds sit in
/// `0..=100`, and `min <= max`.
fn validate_when(raw: Option<RawWhen>, file: &str) -> Result<Option<When>, ActionError> {
    let Some(raw) = raw else { return Ok(None) };
    for detail in &raw.detail {
        if !is_safe_token(detail.as_str()) {
            return Err(ActionError::BadToken {
                file: file.to_string(),
                field: "[when] detail",
                token: detail.as_str().to_string(),
            });
        }
    }
    let bound = |field: &str, v: Option<u8>| -> Result<(), ActionError> {
        if let Some(v) = v {
            if v > 100 {
                return Err(ActionError::BadContextBound {
                    file: file.to_string(),
                    reason: format!("{field} {v} is out of 0..=100"),
                });
            }
        }
        Ok(())
    };
    bound("context_pct_min", raw.context_pct_min)?;
    bound("context_pct_max", raw.context_pct_max)?;
    if let (Some(min), Some(max)) = (raw.context_pct_min, raw.context_pct_max) {
        if min > max {
            return Err(ActionError::BadContextBound {
                file: file.to_string(),
                reason: format!("context_pct_min {min} exceeds context_pct_max {max}"),
            });
        }
    }
    Ok(Some(When {
        state: raw.state,
        detail: raw.detail,
        context_pct_min: raw.context_pct_min,
        context_pct_max: raw.context_pct_max,
    }))
}

// `pub` because it rides the public `ActionError::Structural` error variant; a `pub(crate)` type
// there would be an E0446 leak. The private `schema` module keeps it externally un-nameable (the
// same shape as `GrammarError` in state.rs).
/// A per-kind structural rule violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuralRule {
    /// `kind = "keys"` with no transport entry across `[keys]` and `[api]`.
    KeysEmpty,
    /// `kind = "keys"` carrying `command` (an exec-only field).
    KeysForbidsCommand,
    /// `kind = "keys"` carrying `detach` (an exec-only field).
    KeysForbidsDetach,
    /// `kind = "keys"` carrying `agents` (applicability comes from `[keys]`/`[api]`, not `agents`).
    KeysForbidsAgents,
    /// `kind = "exec"` missing `command`.
    ExecNeedsCommand,
    /// `kind = "exec"` carrying a `[keys]` table (a keys-only field).
    ExecForbidsKeys,
    /// `kind = "exec"` carrying an `[api]` table (a keys-kind-only transport).
    ExecForbidsApi,
}

impl fmt::Display for StructuralRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            StructuralRule::KeysEmpty => {
                "kind = \"keys\" requires at least one [keys] or [api] transport entry"
            }
            StructuralRule::KeysForbidsCommand => {
                "kind = \"keys\" must not set command (exec only)"
            }
            StructuralRule::KeysForbidsDetach => "kind = \"keys\" must not set detach (exec only)",
            StructuralRule::KeysForbidsAgents => {
                "kind = \"keys\" must not set agents; applicability comes from the [keys] table"
            }
            StructuralRule::ExecNeedsCommand => "kind = \"exec\" requires command",
            StructuralRule::ExecForbidsKeys => "kind = \"exec\" must not set a [keys] table",
            StructuralRule::ExecForbidsApi => "kind = \"exec\" must not set an [api] table",
        };
        f.write_str(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse round-trips -------------------------------------------------------

    #[test]
    fn parses_keys_action_with_defaults() {
        let src = r#"
min_engine_version = "0.1"
name = "approve"
label = "Approve"
kind = "keys"
when = { state = ["blocked"], detail = ["permission"] }

[keys]
claude = ["1"]
codex = ["Enter"]
"#;
        let a = ActionManifest::parse(src, "approve", "approve.toml").unwrap();
        assert_eq!(a.name, "approve");
        assert_eq!(a.kind, ActionKind::Keys);
        assert_eq!(a.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(a.detach_timeout_ms, DEFAULT_DETACH_TIMEOUT_MS);
        assert!(!a.detach);
        assert!(!a.confirm);
        assert_eq!(a.keys_for("claude"), Some(["1".to_string()].as_slice()));
        assert_eq!(a.keys_for("gemini"), None);
        let when = a.when.as_ref().unwrap();
        assert_eq!(when.state, [AgentState::Blocked]);
        assert_eq!(when.detail, [Detail::new("permission")]);
    }

    #[test]
    fn parses_exec_action_with_requires_and_overrides() {
        let src = r#"
min_engine_version = "0.1"
name = "summarize"
label = "Summarize progress"
kind = "exec"
agents = ["claude"]
when = { state = ["working", "idle"] }
requires = ["session", "cwd"]
confirm = true
detach = true
timeout_ms = 60000
detach_timeout_ms = 120000
command = "~/.config/tma/actions/summarize.sh"
"#;
        let a = ActionManifest::parse(src, "summarize", "summarize.toml").unwrap();
        assert_eq!(a.kind, ActionKind::Exec);
        assert_eq!(a.agents, ["claude"]);
        assert_eq!(a.requires, [Requirement::Session, Requirement::Cwd]);
        assert!(a.confirm);
        assert!(a.detach);
        assert_eq!(a.timeout_ms, 60_000);
        assert_eq!(a.detach_timeout_ms, 120_000);
        assert_eq!(
            a.command.as_deref(),
            Some("~/.config/tma/actions/summarize.sh")
        );
    }

    // ---- structural rejection ----------------------------------------------------

    fn structural_rule(src: &str, stem: &str) -> StructuralRule {
        match ActionManifest::parse(src, stem, "t.toml").unwrap_err() {
            ActionError::Structural { rule, .. } => rule,
            other => panic!("expected Structural error, got {other:?}"),
        }
    }

    #[test]
    fn keys_action_needs_non_empty_keys_table() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
"#;
        assert_eq!(structural_rule(src, "x"), StructuralRule::KeysEmpty);
    }

    #[test]
    fn keys_action_forbids_command_detach_agents() {
        let base = |extra: &str| {
            format!(
                "min_engine_version = \"0.1\"\nname = \"x\"\nlabel = \"X\"\nkind = \"keys\"\n{extra}\n[keys]\nclaude = [\"1\"]\n"
            )
        };
        assert_eq!(
            structural_rule(&base("command = \"echo hi\""), "x"),
            StructuralRule::KeysForbidsCommand
        );
        assert_eq!(
            structural_rule(&base("detach = true"), "x"),
            StructuralRule::KeysForbidsDetach
        );
        assert_eq!(
            structural_rule(&base("agents = [\"claude\"]"), "x"),
            StructuralRule::KeysForbidsAgents
        );
    }

    #[test]
    fn exec_action_needs_command_and_forbids_keys() {
        let missing = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "exec"
"#;
        assert_eq!(
            structural_rule(missing, "x"),
            StructuralRule::ExecNeedsCommand
        );
        let has_keys = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "exec"
command = "echo hi"
[keys]
claude = ["1"]
"#;
        assert_eq!(
            structural_rule(has_keys, "x"),
            StructuralRule::ExecForbidsKeys
        );
    }

    #[test]
    fn name_must_equal_stem() {
        let src = r#"
min_engine_version = "0.1"
name = "approve"
label = "Approve"
kind = "keys"
[keys]
claude = ["1"]
"#;
        match ActionManifest::parse(src, "approv", "approv.toml").unwrap_err() {
            ActionError::NameMismatch { name, stem, .. } => {
                assert_eq!(name, "approve");
                assert_eq!(stem, "approv");
            }
            other => panic!("expected NameMismatch, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_parse_error() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
surprise = true
[keys]
claude = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(src, "x", "t.toml").unwrap_err(),
            ActionError::Parse { .. }
        ));
    }

    #[test]
    fn unknown_requires_token_is_parse_error() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "exec"
command = "echo hi"
requires = ["session", "screen"]
"#;
        match ActionManifest::parse(src, "x", "t.toml").unwrap_err() {
            ActionError::Parse { source, .. } => {
                assert!(source.to_string().contains("screen"), "msg: {source}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn bad_name_token_rejected() {
        let src = r#"
min_engine_version = "0.1"
name = "ap,prove"
label = "X"
kind = "keys"
[keys]
claude = ["1"]
"#;
        match ActionManifest::parse(src, "ap,prove", "t.toml").unwrap_err() {
            ActionError::BadToken { field, token, .. } => {
                assert_eq!(field, "name");
                assert_eq!(token, "ap,prove");
            }
            other => panic!("expected BadToken, got {other:?}"),
        }
    }

    #[test]
    fn bad_agent_and_detail_tokens_rejected() {
        let bad_agent = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[keys]
"cla ude" = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(bad_agent, "x", "t.toml").unwrap_err(),
            ActionError::BadToken { field, .. } if field == "[keys] agent"
        ));

        let bad_detail = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
when = { detail = ["per,mission"] }
[keys]
claude = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(bad_detail, "x", "t.toml").unwrap_err(),
            ActionError::BadToken { field, .. } if field == "[when] detail"
        ));
    }

    #[test]
    fn context_bounds_validated() {
        let over = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
when = { context_pct_min = 150 }
[keys]
claude = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(over, "x", "t.toml").unwrap_err(),
            ActionError::BadContextBound { .. }
        ));

        let inverted = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
when = { context_pct_min = 80, context_pct_max = 20 }
[keys]
claude = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(inverted, "x", "t.toml").unwrap_err(),
            ActionError::BadContextBound { .. }
        ));
    }

    #[test]
    fn future_schema_reports_upgrade_not_unknown_field() {
        let src = r#"
min_engine_version = "9.9"
name = "x"
label = "X"
kind = "keys"
some_future_field = true
[keys]
claude = ["1"]
"#;
        assert!(matches!(
            ActionManifest::parse(src, "x", "t.toml").unwrap_err(),
            ActionError::EngineTooOld { .. }
        ));
    }

    // ---- [api] transport ---------------------------------------------------------

    #[test]
    fn parses_keys_action_with_api_transport_and_union_applicability() {
        let src = r#"
min_engine_version = "0.1"
name = "approve"
label = "Approve"
kind = "keys"
when = { state = ["blocked"], detail = ["permission"] }

[keys]
claude = ["1"]

[api]
opencode = { op = "permission-reply", reply = "once" }
"#;
        let a = ActionManifest::parse(src, "approve", "approve.toml").unwrap();
        assert_eq!(a.keys_for("claude"), Some(["1".to_string()].as_slice()));
        assert_eq!(
            a.api_for("opencode"),
            Some(&ApiTransport {
                op: ApiOp::PermissionReply,
                reply: ApiReply::Once,
            })
        );
        // Applicability is the union of the two tables.
        assert!(a.applies_to("claude"));
        assert!(a.applies_to("opencode"));
        assert!(!a.applies_to("codex"));
        assert!(a.keys_for("opencode").is_none(), "api agent has no keys");
    }

    #[test]
    fn api_only_action_is_legal() {
        let src = r#"
min_engine_version = "0.1"
name = "approve"
label = "Approve"
kind = "keys"

[api]
opencode = { op = "permission-reply", reply = "reject" }
"#;
        let a = ActionManifest::parse(src, "approve", "approve.toml").unwrap();
        assert!(a.keys.is_empty());
        assert_eq!(
            a.api_for("opencode").map(|t| t.reply),
            Some(ApiReply::Reject)
        );
        assert!(a.applies_to("opencode"));
    }

    #[test]
    fn keys_and_api_both_empty_is_keys_empty() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
"#;
        assert_eq!(structural_rule(src, "x"), StructuralRule::KeysEmpty);
    }

    #[test]
    fn api_on_exec_is_a_structural_error() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "exec"
command = "echo hi"
[api]
opencode = { op = "permission-reply", reply = "once" }
"#;
        assert_eq!(structural_rule(src, "x"), StructuralRule::ExecForbidsApi);
    }

    #[test]
    fn agent_in_both_keys_and_api_is_a_parse_error() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[keys]
opencode = ["1"]
[api]
opencode = { op = "permission-reply", reply = "once" }
"#;
        match ActionManifest::parse(src, "x", "t.toml").unwrap_err() {
            ActionError::AgentInBothTransports { agent, .. } => assert_eq!(agent, "opencode"),
            other => panic!("expected AgentInBothTransports, got {other:?}"),
        }
    }

    #[test]
    fn unknown_api_op_and_reply_are_parse_errors() {
        let bad_op = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[api]
opencode = { op = "delete-everything", reply = "once" }
"#;
        assert!(matches!(
            ActionManifest::parse(bad_op, "x", "t.toml").unwrap_err(),
            ActionError::Parse { .. }
        ));
        let bad_reply = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[api]
opencode = { op = "permission-reply", reply = "maybe" }
"#;
        assert!(matches!(
            ActionManifest::parse(bad_reply, "x", "t.toml").unwrap_err(),
            ActionError::Parse { .. }
        ));
        // A missing `reply` is a parse error too (the field is required for permission-reply).
        let no_reply = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[api]
opencode = { op = "permission-reply" }
"#;
        assert!(matches!(
            ActionManifest::parse(no_reply, "x", "t.toml").unwrap_err(),
            ActionError::Parse { .. }
        ));
    }

    #[test]
    fn bad_api_agent_token_rejected() {
        let src = r#"
min_engine_version = "0.1"
name = "x"
label = "X"
kind = "keys"
[api]
"open,code" = { op = "permission-reply", reply = "once" }
"#;
        assert!(matches!(
            ActionManifest::parse(src, "x", "t.toml").unwrap_err(),
            ActionError::BadToken { field, .. } if field == "[api] agent"
        ));
    }
}
