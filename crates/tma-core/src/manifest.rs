//! The agent manifest: one TOML file is the complete description of an agent — identity,
//! hook-event-to-claim mapping, capture coverage, screen rules, detail spellings. Schema
//! plus validating loader only.
//!
//! State routing is normative and NOT manifest-overridable: manifests map their agent's
//! events and screens *into* the closed [`AgentState`] vocabulary; `[details]` carries
//! token spellings only. Attempts to remap a state are rejected.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::Deserialize;

use crate::evidence::Claim;
use crate::state::{AgentState, Detail};

/// A parsed, validated agent manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The minimum engine version this manifest requires.
    pub min_engine_version: Version,
    pub identity: Identity,
    /// Absent for hookless agents (no `[hooks]` block — the screen-only floor a user manifest may
    /// sit at); detection then rides screen rules and title entirely.
    pub hooks: Option<Hooks>,
    pub capture: Capture,
    pub rules: Vec<Rule>,
    /// Canonical detail token ⇒ its alternate spellings.
    pub details: BTreeMap<String, DetailSpec>,
    /// Metric telemetry channels this agent exposes; absent when it has none. Its presence for
    /// the context metric is what distinguishes a `gated` refusal (channel present, metric absent) from
    /// a permanent `no-coverage` one.
    pub telemetry: Option<Telemetry>,
}

/// The `[telemetry]` block: one optional subtable per metric. The metric-named subtable exists
/// so a second metric (cost, rate-limit headroom) is an additive sibling, not a breaking migration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    /// `[telemetry.context]`: the context-utilization channel, absent when the agent exposes none.
    #[serde(default)]
    pub context: Option<ContextChannel>,
}

/// `[telemetry.context]`: how tma obtains this agent's context-utilization percent. `channel`
/// names the transport shape; `format` names a compiled-in pure parser (`claude-statusline-json`, …).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextChannel {
    pub channel: Channel,
    /// The compiled-in parser id (bytes-in, metric-out). Not user-authorable: a new format needs core
    /// code, so this is a free string here and the intake refuses an unknown one rather than the loader.
    pub format: String,
}

/// The transport shape of a telemetry channel. A closed vocabulary: `event` (a push shim, e.g.
/// Claude's statusline), `file-tail` (a bounded end-anchored read, e.g. Codex rollout), `screen` (last-resort extraction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Event,
    FileTail,
    Screen,
}

impl FromStr for Channel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "event" => Ok(Channel::Event),
            "file-tail" => Ok(Channel::FileTail),
            "screen" => Ok(Channel::Screen),
            other => Err(format!(
                "unknown telemetry channel {other:?} (expected event, file-tail, or screen)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// Observation identity.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// `#{pane_current_command}` values that cheaply flag a candidate agent pane.
    pub process_names: Vec<String>,
    /// Secondary signal: regexes over `#{pane_title}` that NARROW a generic `process_names` match.
    /// When non-empty, a pane is this agent only when a `process_names` entry AND a title pattern
    /// match (or the flicker-stickiness hold is active). Empty leaves identity as process match
    /// alone. Compiled at `RuleEngine::build`; an invalid pattern is a build-time error.
    #[serde(default)]
    pub title_patterns: Vec<String>,
}

/// The `[hooks]` block. Presence marks the agent hook-capable.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Which states/lifecycle the agent's hooks report — the first coverage gate.
    #[serde(default)]
    pub covers: Vec<CoverToken>,
    /// Event-to-claim mappings (`[[hooks.map]]`).
    #[serde(default)]
    pub map: Vec<HookMap>,
}

/// One `[[hooks.map]]` entry: an agent hook event and the claim it produces.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookMap {
    /// Agent hook event name (e.g. `Notification`, `SessionStart`).
    pub event: String,
    /// Optional payload matcher (e.g. `permission_prompt|elicitation_dialog`).
    #[serde(default)]
    pub matcher: Option<String>,
    /// The claim this event raises — a state claim or a lifecycle claim.
    pub claim: Claim,
    /// Whether this event MEANS "a turn ended" (Claude's `Stop`, Codex's `notify`, pi's
    /// `agent_settled`). A property of the event, not of its claim: the same `state = "idle"` claim
    /// is also raised by screen rules, where nothing ended. The intake raises the done marker on a
    /// turn end even when the pane was already idle, which is the only way a SECOND completion is
    /// signalled after the first marker was cleared. Read only for an `idle` state claim; an event
    /// that merely observes idleness (an idle-reminder notification) must leave it false, or a
    /// cleared marker would come straight back on the next reminder.
    #[serde(default)]
    pub turn_end: bool,
}

/// The `[capture]` block — the second coverage gate that the fold's coverage-aware
/// decay reads.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// States the agent's screen rules reliably detect (evidence-backed).
    #[serde(default)]
    pub visible: Vec<AgentState>,
}

/// One `[[rules]]` screen rule.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The state this rule asserts on match. State routing is normative.
    pub state: AgentState,
    /// Optional detail token to attach (e.g. `permission`), needed for permission-prompt coverage
    /// (permission prompt ⇒ `blocked/permission`). State stays normative; detail is per-manifest.
    #[serde(default)]
    pub detail: Option<Detail>,
    /// Higher wins when multiple rules match.
    #[serde(default)]
    pub priority: i64,
    /// Where to look (v1: `tail_lines(N)`, `visible`, or `title`).
    pub region: Region,
    #[serde(rename = "match")]
    pub match_: Matcher,
    /// This screen shows history, not live state — freeze, do not restate.
    #[serde(default)]
    pub skip_state_update: bool,
}

/// The `[details]` per-token spec: alternate spellings that map to the canonical token.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailSpec {
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// A rule region. v1 supports tail-window scoping only; richer regions grow from evidence
/// later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Match against the last `N` lines of the captured tail. Bottom-anchored agents (claude/codex)
    /// use a small window whose chrome fits the visible screen, so this never reads scrollback for them.
    TailLines(usize),
    /// Match against the VISIBLE SCREEN ONLY — the last
    /// [`visible_height`](crate::snapshot::PaneSnapshot::visible_height) lines of the tail.
    /// `capture-pane -S -50` can reach 50 lines into scrollback, so whole-screen rules on floating
    /// chrome (cursor, pi) would match a PRIOR turn on a short pane; clamping to pane height removes
    /// exactly those lines. `None` degrades to the whole tail.
    Visible,
    /// Match against the pane title.
    Title,
}

impl FromStr for Region {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "title" {
            return Ok(Region::Title);
        }
        if s == "visible" {
            return Ok(Region::Visible);
        }
        if let Some(inner) = s
            .strip_prefix("tail_lines(")
            .and_then(|r| r.strip_suffix(')'))
        {
            let n: usize = inner.trim().parse().map_err(|_| {
                format!("region tail_lines(N) needs a non-negative integer, got {inner:?}")
            })?;
            return Ok(Region::TailLines(n));
        }
        Err(format!(
            "unknown region {s:?} (v1 supports tail_lines(N), visible, and title)"
        ))
    }
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// A screen matcher — leaf text predicates composed via `any`/`all`/`not`. Externally tagged in
/// TOML (`{ contains = "x" }`, `{ not = { ... } }`); regexes compile at match time, not in the loader.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matcher {
    Contains(String),
    Regex(String),
    LineRegex(String),
    Any(Vec<Matcher>),
    All(Vec<Matcher>),
    Not(Box<Matcher>),
}

/// A token in `[hooks].covers`: a published state or the `lifecycle` marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverToken {
    State(AgentState),
    Lifecycle,
}

impl<'de> Deserialize<'de> for CoverToken {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "lifecycle" {
            return Ok(CoverToken::Lifecycle);
        }
        s.parse::<AgentState>().map(CoverToken::State).map_err(|_| {
            de::Error::custom(format!(
                "unknown covers token {s:?} (expected a state or \"lifecycle\")"
            ))
        })
    }
}

/// A minimal semver-ish `major.minor.patch` (missing parts default to zero). Sufficient for the
/// engine-version gate; not full semver (no pre-release / build metadata).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// The running engine version (the `tma-core` crate version).
    pub fn engine() -> Version {
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("CARGO_PKG_VERSION is a valid version")
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let mut next = |label: &str| -> Result<u64, String> {
            match parts.next() {
                None => Ok(0),
                Some(p) => p
                    .parse()
                    .map_err(|_| format!("version component {label} is not a number: {p:?}")),
            }
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if parts.next().is_some() {
            return Err(format!("version {s:?} has too many components"));
        }
        Ok(Version {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Manifest load/validation errors. Every variant names the offending file (and field).
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{file}: {source}")]
    // Exposing `toml::de::Error` publicly is deliberate: this crate feeds the tma binary, the toml
    // version is workspace-pinned, and an opaque wrapper would cost more than it protects.
    Parse {
        file: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{file}: invalid min_engine_version: {reason}")]
    BadVersion { file: String, reason: String },
    #[error(
        "{file}: manifest requires engine {required} but this engine is {engine} \
         (upgrade tma to load it)"
    )]
    EngineTooOld {
        file: String,
        required: Version,
        engine: Version,
    },
    #[error(
        "{file}: [details] key {key:?} collides with a state token; state routing is \
         normative and not manifest-overridable"
    )]
    StateRemap { file: String, key: String },
    #[error(
        "{file}: {field} detail token {token:?} is not a valid machine token \
         (lowercase a-z, digits, and _ only — no glyphs or format metacharacters)"
    )]
    BadDetailToken {
        file: String,
        field: String,
        token: String,
    },
}

/// A lenient probe capturing *only* `min_engine_version`. Parsed before the strict [`RawManifest`]
/// so a newer-schema manifest (carrying unknown fields) is rejected with the upgrade error rather
/// than a confusing `deny_unknown_fields` "unknown field".
#[derive(Deserialize)]
struct VersionProbe {
    min_engine_version: String,
}

/// The raw TOML shape, deserialized before semantic validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    min_engine_version: String,
    identity: Identity,
    #[serde(default)]
    hooks: Option<Hooks>,
    capture: Capture,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    details: BTreeMap<String, DetailSpec>,
    #[serde(default)]
    telemetry: Option<Telemetry>,
}

/// A safe machine token: non-empty, `[a-z0-9_-]` only. Rejects the `set -pF` format
/// metacharacters (`#{},`), whitespace, control bytes, and non-ASCII glyphs, so a token embedded
/// into a stamp chain can never break it. Shared by the agent manifest and the action manifest.
pub(crate) fn is_safe_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A manifest-declared detail token must be a safe machine token: non-empty, `[a-z0-9_-]` only.
/// This rejects the `set -pF` format metacharacters (`#{},`), whitespace, control bytes, and
/// non-ASCII glyphs — none may reach the render chain — with a field+file error.
fn validate_detail_token(token: &str, field: &str, file: &str) -> Result<(), ManifestError> {
    if is_safe_token(token) {
        Ok(())
    } else {
        Err(ManifestError::BadDetailToken {
            file: file.to_string(),
            field: field.to_string(),
            token: token.to_string(),
        })
    }
}

impl Manifest {
    /// Parse and validate a manifest. `file` names the source for error messages (the
    /// on-disk path, or a synthetic name for embedded/test manifests).
    pub fn parse(toml_src: &str, file: &str) -> Result<Manifest, ManifestError> {
        // Gate on the engine version BEFORE the strict parse: a newer-schema manifest carries
        // unknown fields that `deny_unknown_fields` would reject with "unknown field `foo`", burying
        // the real cause. Probe `min_engine_version` leniently; a version that won't parse falls
        // through to the strict path, which surfaces it as the precise `BadVersion`.
        if let Ok(probe) = toml::from_str::<VersionProbe>(toml_src) {
            if let Ok(min) = probe.min_engine_version.parse::<Version>() {
                let engine = Version::engine();
                if min > engine {
                    return Err(ManifestError::EngineTooOld {
                        file: file.to_string(),
                        required: min,
                        engine,
                    });
                }
            }
        }

        let raw: RawManifest = toml::from_str(toml_src).map_err(|source| ManifestError::Parse {
            file: file.to_string(),
            source,
        })?;

        let min_engine_version = raw
            .min_engine_version
            .parse::<Version>()
            .map_err(|reason| ManifestError::BadVersion {
                file: file.to_string(),
                reason,
            })?;

        // The lenient probe already gated same/older versions through; re-check defends
        // against a probe that could not parse the field (it falls through to here).
        let engine = Version::engine();
        if min_engine_version > engine {
            return Err(ManifestError::EngineTooOld {
                file: file.to_string(),
                required: min_engine_version,
                engine,
            });
        }

        // [details] keys are canonical detail tokens, never states.
        for key in raw.details.keys() {
            if key.parse::<AgentState>().is_ok() {
                return Err(ManifestError::StateRemap {
                    file: file.to_string(),
                    key: key.clone(),
                });
            }
        }

        // Detail tokens are embedded into `set -pF` chains at render time. Reject any declared
        // spelling that could break the chain (`#{},`, whitespace) here at the load boundary. This
        // governs what a manifest may DECLARE; reading a stored `@agent_detail` stays display-tolerant.
        for (key, spec) in &raw.details {
            validate_detail_token(key, "[details] key", file)?;
            for alias in &spec.aliases {
                validate_detail_token(alias, "[details] alias", file)?;
            }
        }
        for rule in &raw.rules {
            if let Some(detail) = &rule.detail {
                validate_detail_token(detail.as_str(), "[[rules]] detail", file)?;
            }
        }
        if let Some(hooks) = &raw.hooks {
            for m in &hooks.map {
                if let Claim::State(sc) = &m.claim {
                    if let Some(detail) = &sc.detail {
                        validate_detail_token(detail.as_str(), "[[hooks.map]] claim detail", file)?;
                    }
                }
            }
        }

        Ok(Manifest {
            min_engine_version,
            identity: raw.identity,
            hooks: raw.hooks,
            capture: raw.capture,
            rules: raw.rules,
            details: raw.details,
            telemetry: raw.telemetry,
        })
    }

    /// Whether this agent declares a context-utilization telemetry channel. Drives the gate's
    /// `no-coverage` (false, permanent) vs `gated` (true, metric merely absent) distinction.
    pub fn covers_context(&self) -> bool {
        self.telemetry
            .as_ref()
            .and_then(|t| t.context.as_ref())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Lifecycle, StateClaim};

    const FULL: &str = r#"
min_engine_version = "0.1"

[identity]
process_names = ["claude"]

[hooks]
covers = ["working", "blocked", "idle", "lifecycle"]

[[hooks.map]]
event = "Notification"
matcher = "permission_prompt|elicitation_dialog"
claim = { state = "blocked", detail = "permission" }

[[hooks.map]]
event = "SessionStart"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "SessionEnd"
claim = { lifecycle = "end" }

[capture]
visible = ["working", "idle", "blocked"]

[[rules]]
state = "blocked"
detail = "permission"
priority = 100
region = "tail_lines(5)"
match = { any = [ { contains = "Do you want to proceed?" }, { regex = "❯\\s" } ] }

[[rules]]
state = "idle"
priority = 50
region = "title"
match = { contains = "✳" }

[[rules]]
state = "idle"
priority = 10
region = "tail_lines(50)"
skip_state_update = true
match = { all = [ { contains = "transcript" }, { not = { contains = "❯" } } ] }

[details]
rate_limit = { aliases = ["ratelimited", "rate-limited"] }
"#;

    #[test]
    fn parses_full_manifest() {
        let m = Manifest::parse(FULL, "claude.toml").unwrap();
        assert_eq!(
            m.min_engine_version,
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
        assert_eq!(m.identity.process_names, ["claude"]);

        let hooks = m.hooks.as_ref().unwrap();
        assert_eq!(
            hooks.covers,
            [
                CoverToken::State(AgentState::Working),
                CoverToken::State(AgentState::Blocked),
                CoverToken::State(AgentState::Idle),
                CoverToken::Lifecycle,
            ]
        );
        assert_eq!(hooks.map[0].event, "Notification");
        assert_eq!(
            hooks.map[0].matcher.as_deref(),
            Some("permission_prompt|elicitation_dialog")
        );
        assert_eq!(
            hooks.map[0].claim,
            Claim::State(StateClaim {
                state: AgentState::Blocked,
                detail: Some(Detail::new("permission")),
            })
        );
        assert_eq!(
            hooks.map[1].claim,
            Claim::Lifecycle {
                lifecycle: Lifecycle::Start
            }
        );

        assert_eq!(
            m.capture.visible,
            [AgentState::Working, AgentState::Idle, AgentState::Blocked]
        );

        assert_eq!(m.rules.len(), 3);
        assert_eq!(m.rules[0].region, Region::TailLines(5));
        assert_eq!(m.rules[0].detail, Some(Detail::new("permission")));
        assert_eq!(m.rules[1].region, Region::Title);
        assert!(m.rules[2].skip_state_update);

        assert_eq!(
            m.details["rate_limit"].aliases,
            ["ratelimited", "rate-limited"]
        );
    }

    #[test]
    fn hookless_manifest_omits_hooks() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["gemini"]
[capture]
visible = ["working", "idle"]
[[rules]]
state = "working"
region = "title"
match = { contains = "spinner" }
"#;
        let m = Manifest::parse(src, "gemini.toml").unwrap();
        assert!(m.hooks.is_none());
    }

    #[test]
    fn rejects_state_remap_in_details() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[details]
blocked = { aliases = ["halted"] }
"#;
        let err = Manifest::parse(src, "bad.toml").unwrap_err();
        assert!(
            matches!(&err, ManifestError::StateRemap { file, key } if file == "bad.toml" && key == "blocked"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_future_schema_version() {
        let src = r#"
min_engine_version = "9.9"
[identity]
process_names = ["x"]
[capture]
visible = []
"#;
        let err = Manifest::parse(src, "future.toml").unwrap_err();
        match err {
            ManifestError::EngineTooOld { required, .. } => {
                assert_eq!(
                    required,
                    Version {
                        major: 9,
                        minor: 9,
                        patch: 0
                    }
                );
            }
            other => panic!("expected EngineTooOld, got {other:?}"),
        }
    }

    #[test]
    fn future_schema_with_unknown_field_reports_upgrade_not_unknown_field() {
        // A newer-schema manifest carries fields this engine doesn't know. The version
        // gate must win, so the user is told to upgrade rather than seeing "unknown field".
        let src = r#"
min_engine_version = "9.9"
some_future_block = { whatever = true }
[identity]
process_names = ["x"]
[capture]
visible = []
"#;
        match Manifest::parse(src, "future.toml").unwrap_err() {
            ManifestError::EngineTooOld { file, required, .. } => {
                assert_eq!(file, "future.toml");
                assert_eq!(
                    required,
                    Version {
                        major: 9,
                        minor: 9,
                        patch: 0
                    }
                );
            }
            other => panic!("expected EngineTooOld, got {other:?}"),
        }
    }

    #[test]
    fn same_version_unknown_field_still_reports_unknown_field() {
        // The version gate must not swallow genuine schema errors on a loadable version.
        let src = r#"
min_engine_version = "0.1"
some_future_block = { whatever = true }
[identity]
process_names = ["x"]
[capture]
visible = []
"#;
        match Manifest::parse(src, "extra.toml").unwrap_err() {
            ManifestError::Parse { file, source } => {
                assert_eq!(file, "extra.toml");
                assert!(
                    source.to_string().contains("some_future_block")
                        || source.to_string().contains("unknown field"),
                    "message: {source}"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_detail_token_with_metacharacter() {
        for (bad, where_) in [
            ("per,mission", "[details] key"),
            ("perm}ission", "[details] key"),
            ("perm#ission", "[details] key"),
            ("rate limit", "[details] key"),
        ] {
            let src = format!(
                r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[details]
"{bad}" = {{ aliases = [] }}
"#
            );
            match Manifest::parse(&src, "detail.toml").unwrap_err() {
                ManifestError::BadDetailToken { file, field, token } => {
                    assert_eq!(file, "detail.toml");
                    assert_eq!(field, where_);
                    assert_eq!(token, bad);
                }
                other => panic!("expected BadDetailToken for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_bad_alias_and_rule_and_hook_detail() {
        // Alias metacharacter.
        let alias_src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[details]
rate_limit = { aliases = ["rate,limited"] }
"#;
        assert!(matches!(
            Manifest::parse(alias_src, "a.toml").unwrap_err(),
            ManifestError::BadDetailToken { field, .. } if field == "[details] alias"
        ));

        // Rule detail metacharacter.
        let rule_src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[[rules]]
state = "blocked"
detail = "per}mission"
region = "title"
match = { contains = "?" }
"#;
        assert!(matches!(
            Manifest::parse(rule_src, "r.toml").unwrap_err(),
            ManifestError::BadDetailToken { field, .. } if field == "[[rules]] detail"
        ));

        // Hook claim detail metacharacter.
        let hook_src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[[hooks.map]]
event = "Notification"
claim = { state = "blocked", detail = "perm ission" }
"#;
        assert!(matches!(
            Manifest::parse(hook_src, "h.toml").unwrap_err(),
            ManifestError::BadDetailToken { field, .. } if field == "[[hooks.map]] claim detail"
        ));
    }

    #[test]
    fn accepts_valid_machine_token_details() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = ["blocked"]
[[rules]]
state = "blocked"
detail = "permission"
region = "title"
match = { contains = "?" }
[details]
rate_limit = { aliases = ["ratelimited"] }
"#;
        let m = Manifest::parse(src, "ok.toml").unwrap();
        assert_eq!(m.rules[0].detail, Some(Detail::new("permission")));
        assert!(m.details.contains_key("rate_limit"));
    }

    #[test]
    fn rejects_unknown_region() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[[rules]]
state = "blocked"
region = "bottom_non_empty_lines(5)"
match = { contains = "?" }
"#;
        let err = Manifest::parse(src, "region.toml").unwrap_err();
        match err {
            ManifestError::Parse { file, source } => {
                assert_eq!(file, "region.toml");
                assert!(
                    source.to_string().contains("unknown region"),
                    "message: {source}"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_state_in_capture_visible() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = ["running"]
"#;
        assert!(matches!(
            Manifest::parse(src, "cap.toml").unwrap_err(),
            ManifestError::Parse { .. }
        ));
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let src = r#"
min_engine_version = "0.1"
surprise = true
[identity]
process_names = ["x"]
[capture]
visible = []
"#;
        assert!(matches!(
            Manifest::parse(src, "extra.toml").unwrap_err(),
            ManifestError::Parse { .. }
        ));
    }

    #[test]
    fn parses_telemetry_context_channel() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["claude"]
[capture]
visible = ["working"]
[telemetry.context]
channel = "event"
format = "claude-statusline-json"
"#;
        let m = Manifest::parse(src, "claude.toml").unwrap();
        let ctx = m.telemetry.as_ref().unwrap().context.as_ref().unwrap();
        assert_eq!(ctx.channel, Channel::Event);
        assert_eq!(ctx.format, "claude-statusline-json");
        assert!(m.covers_context());
    }

    #[test]
    fn no_telemetry_block_means_no_context_coverage() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
"#;
        let m = Manifest::parse(src, "x.toml").unwrap();
        assert!(m.telemetry.is_none());
        assert!(!m.covers_context());
    }

    #[test]
    fn rejects_unknown_telemetry_channel() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[telemetry.context]
channel = "carrier-pigeon"
format = "x"
"#;
        match Manifest::parse(src, "t.toml").unwrap_err() {
            ManifestError::Parse { source, .. } => {
                assert!(
                    source.to_string().contains("unknown telemetry channel"),
                    "msg: {source}"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_telemetry_field() {
        let src = r#"
min_engine_version = "0.1"
[identity]
process_names = ["x"]
[capture]
visible = []
[telemetry.context]
channel = "event"
format = "x"
surprise = true
"#;
        assert!(matches!(
            Manifest::parse(src, "t.toml").unwrap_err(),
            ManifestError::Parse { .. }
        ));
    }

    #[test]
    fn version_ordering() {
        let v = |s: &str| s.parse::<Version>().unwrap();
        assert!(v("0.1") == v("0.1.0"));
        assert!(v("0.2") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.9.9"));
        assert!(v("0.1.1") > v("0.1"));
    }
}
