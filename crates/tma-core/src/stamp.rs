//! The pane-option tuple: its deserialized form ([`StampedState`]) and the machine-token grammar
//! that round-trips it through tmux user options. Values are machine tokens and epoch
//! **milliseconds**, never glyphs; `@agent_attention` is `1`-or-absent. Persisted provenance
//! (`@agent_source`, `@agent_evidence_at`) lets a stateless producer rank a stamped hook claim
//! above its own capture — the mechanism that stops the `blocked`-clobber race.
//!
//! Millisecond migration: a pre-upgrade server may hold legacy epoch-seconds (10-digit) `*_at`
//! values, so [`from_options`](StampedState::from_options) scales any below [`MILLIS_FLOOR`] on read.

use std::collections::HashMap;

use crate::evidence::Provenance;
use crate::state::{AgentState, Detail, GrammarError};

/// tmux user-option key names for the pane schema.
pub mod opt {
    pub const NAME: &str = "@agent_name";
    pub const STATE: &str = "@agent_state";
    pub const DETAIL: &str = "@agent_detail";
    pub const SOURCE: &str = "@agent_source";
    pub const EVIDENCE_AT: &str = "@agent_evidence_at";
    pub const SINCE: &str = "@agent_since";
    pub const STAMPED_AT: &str = "@agent_stamped_at";
    pub const ATTENTION: &str = "@agent_attention";
    pub const NOTIFIED_AT: &str = "@agent_notified_at";
    /// Epoch **ms** of the last hook event that MEANT "a turn ended" and raised the done marker
    /// (`[[hooks.map]] turn_end = true`). Distinct from [`SINCE`], which is write-once per state
    /// run and so cannot move on the idle→idle edge a second completion draws; the uptime display
    /// reads `SINCE`, and moving it would make the state duration lie. Absent (0) on a pane that
    /// has never had a turn end recorded, which makes every comparison
    /// ([`super::StampedState::episode_at`]) degrade to `SINCE` alone. Pane scope.
    pub const TURN_AT: &str = "@agent_turn_at";
    pub const HASH: &str = "@agent_hash";
    pub const PID: &str = "@agent_pid";
    pub const SESSION: &str = "@agent_session";
    pub const SUBAGENTS: &str = "@agent_subagents";
    /// Context-utilization metric percent: integer `0..=100`, or absent when the agent has no
    /// telemetry coverage or the channel reported no window (a null-clear). Stamped under the
    /// evidence-time write guard beside its marker; never part of the [`super::StampedState`] tuple.
    pub const CONTEXT_PCT: &str = "@agent_context_pct";
    /// Epoch **ms** of the evidence behind [`CONTEXT_PCT`]: written last in the context
    /// mini-chain, advanced even by a null-clear, and the `not older` arbitration basis.
    pub const CONTEXT_AT: &str = "@agent_context_at";
    /// Tokens currently in the agent's context window: the absolute the gauge is a percent of, for
    /// the channels that report one tma can call a footprint (pi, Cursor). Absent for a percent-only
    /// channel (Claude) and for Codex, whose `total_token_usage` mixes footprint and session spend.
    /// Never a cost figure — tma stamps no pricing and no cumulative spend. Written in the same
    /// guarded chain as [`CONTEXT_PCT`], set and cleared with [`TOKENS_AT`].
    pub const TOKENS: &str = "@agent_tokens";
    /// Epoch **ms** of the evidence behind [`TOKENS`]. Written and cleared with it under the context
    /// chain's guard, so it equals [`CONTEXT_AT`] whenever a count is present; it exists so a reader
    /// that wants only the count can age it without also reading the gauge's marker.
    pub const TOKENS_AT: &str = "@agent_tokens_at";
    /// Account quota utilization percent: integer `0..=100`, the HIGHEST of the rate-limit windows
    /// the channel reported. Account-wide rather than per-pane, so every pane on one account carries
    /// the same figure; absent when the channel reports no `rate_limits` block. Stamped under its own
    /// evidence-time guard beside [`QUOTA_AT`]; never part of the [`super::StampedState`] tuple.
    pub const QUOTA_PCT: &str = "@agent_quota_pct";
    /// Which window [`QUOTA_PCT`] came from: `5h` / `7d` / `spend` (Claude) or `primary` /
    /// `secondary` (Codex). A machine token like every other option value; without it the percent is
    /// unreadable, since 80% of a five-hour window and 80% of a week mean different things.
    pub const QUOTA_WINDOW: &str = "@agent_quota_window";
    /// Epoch **ms** at which [`QUOTA_WINDOW`] resets, absent when the channel states none. Both
    /// vendors publish seconds (Claude and newer Codex absolute, older Codex relative to the reading);
    /// the conversion happens in the parser, so this option is ms like every other instant here.
    pub const QUOTA_RESETS_AT: &str = "@agent_quota_resets_at";
    /// Epoch **ms** of the evidence behind the quota trio and [`COST_USD`]: written last in the quota
    /// mini-chain and the `not older` arbitration basis, exactly as [`CONTEXT_AT`] is for the gauge.
    pub const QUOTA_AT: &str = "@agent_quota_at";
    /// The agent's own reported session cost in USD, a string with two decimals (`3.50`). Absent for
    /// a channel that publishes none. It is the VENDOR's live figure for THIS session, not a total
    /// tma computed and not a price table; tma still aggregates nothing across sessions. Written in
    /// the same guarded chain as the quota trio, and cleared by an observation that carries no cost.
    pub const COST_USD: &str = "@agent_cost_usd";
    /// The `context_high` notify marker: a present/absent **armed flag**, never an episode
    /// stamp compared against a `since`. Absent = armed; present = already fired (rearmed by unsetting
    /// it below `threshold - 10`). Its value is an epoch **ms** for debuggability only, not a
    /// comparison basis. Written only by the context-high notifier, guarded set-from-absent so
    /// concurrent firers resolve to one bell. Never the state lane's [`NOTIFIED_AT`]. Pane scope.
    pub const CONTEXT_NOTIFIED_AT: &str = "@agent_context_notified_at";
    /// The agent's model name, a best-effort label the file-tail intake reads from the rollout
    /// window. Not part of the [`super::StampedState`] tuple and never load-bearing for a gauge; it
    /// only feeds `tma doctor`'s recognized-model line (a model no `[telemetry.windows]` entry
    /// names). Plain-set and cleared on deregister; absent when no model record sat in the tail. Pane scope.
    pub const MODEL: &str = "@agent_model";
    /// The pending OpenCode permission request id: stamped by the event intake from a
    /// `permission.asked` edge (ownership-filtered against [`SESSION`]), cleared on the edges that
    /// end the prompt (a working/idle transition, or a `permission.replied`). The action broker reads
    /// it to answer an `api` `permission-reply` op; an empty value refuses that op `requires-unmet`.
    /// Pane scope.
    pub const PERMISSION_REQUEST: &str = "@agent_permission_request";
    /// The tool name of the call a permission prompt is asking about (Claude's `PermissionRequest`
    /// `tool_name`). Stamped with [`PENDING_CALL`] and [`PENDING_SUMMARY`] by the event intake and
    /// cleared with them on every edge that ends the prompt. Pane scope.
    pub const PENDING_TOOL: &str = "@agent_pending_tool";
    /// The pending call's id (`tool_use_id`), so a consumer can tell one prompt from the next on a
    /// pane that blocks twice on the same tool. Pane scope.
    pub const PENDING_CALL: &str = "@agent_pending_call";
    /// A one-line, 120-byte summary of the pending call derived from `tool_input`: the command for
    /// Bash, the path for Edit/Write/Read, else the first string field. **Agent-supplied text**, so
    /// it is deliberately confined to this option and the JSON rows, it never enters the
    /// notification payload, the notify audit line, or any env var handed to `sh -c`. Pane scope.
    pub const PENDING_SUMMARY: &str = "@agent_pending_summary";
    /// The OpenCode server base URL: stamped at registration by the plugin from its
    /// `PluginInput.serverUrl`. The broker's `permission-reply` endpoint, with a config
    /// `[api.opencode] api_base` fallback; absent-and-no-fallback refuses `requires-unmet`.
    /// Pane scope.
    pub const API_ENDPOINT: &str = "@agent_api_endpoint";
    /// Single-flight action lock. One pane option holding `<expiry>:<nonce>:<pid>:<name>`:
    /// acquired/reclaimed by a server-side conditional write on the leading expiry, released
    /// nonce-conditionally, self-healing via the embedded expiry. Written only by the action broker
    /// (`tma-tmux`'s lock module), never part of the [`super::StampedState`] tuple. Pane scope.
    pub const ACTION: &str = "@agent_action";
    /// The action broker's consecutive-fire run, `<episode_ms>:<action>:<count>`. Written under the
    /// held [`ACTION`] lock on the path that is about to have an effect, so it counts deliveries and
    /// not refusals; a new episode or a different action starts the run over. Read by the `[act] log`
    /// line and by the mis-tap warning, never by the gate. Pane scope.
    pub const ACT_REPEAT: &str = "@agent_act_repeat";
    /// User-set escape hatch: any non-empty value takes the pane out of detection entirely, so a
    /// dev server that a title-narrowed manifest mistakes for an agent stops being one without
    /// disabling the agent type. Written only by the user (`tmux set-option -p @agent_ignore 1`),
    /// never by tma, so no removal path clears it. Pane scope.
    pub const IGNORE: &str = "@agent_ignore";
    /// Notification mute deadline: epoch **ms** past which the pane notifies again, or
    /// [`super::MUTE_FOREVER_MS`] for an indefinite mute. Written by `tma mute`, unset by
    /// `tma mute --clear`. It suppresses the *fire* only: detection, stamping, the episode markers,
    /// and the rollup counts are all unchanged, so a muted pane still shows its state everywhere.
    /// Pane scope.
    pub const MUTE_UNTIL: &str = "@agent_mute_until";
    /// Window-scoped rollup, not part of the per-pane [`super::StampedState`] tuple.
    pub const SUMMARY: &str = "@agent_summary";
    /// The same rollup grammar at session scope, for a per-session status line. A distinct key
    /// rather than [`SUMMARY`] at another scope: a pane-context format read walks pane → window →
    /// session, so one shared name would make an agentless window inherit its session's rollup.
    pub const SESSION_SUMMARY: &str = "@agent_session_summary";
    /// The pid a resident `tma watch` advertises for the SIGUSR1 nudge. Display infra — set
    /// on the surface's OWN pane so a recycled server-scoped pid can't turn a focus change into a stray kill.
    pub const WATCH_PID: &str = "@tma_watch_pid";
    /// Server-wide poll hint: epoch **ms** of the last producing cycle, a stampede guard that skips
    /// redundant producing work; per-pane [`STAMPED_AT`] stays authoritative for freshness. Server scope.
    pub const LAST_POLL: &str = "@tma_last_poll";
    /// Server-wide cache of the `set -pF` conditional-write probe: `"1"` when the server supports
    /// the guarded write, `"0"` for the advisory degrade. Constant for a server's life. Server scope.
    pub const SETPF_OK: &str = "@tma_setpf_ok";
    /// Prefix of the per-client jump-origin option: full key `@tma_origin_<sanitized name>_<hash>`
    /// (the hash separates punctuation-only-differing names). Server scope, keyed by client.
    pub const ORIGIN_PREFIX: &str = "@tma_origin_";
    /// Flicker-stickiness anchor: the agent pid a title-narrowed manifest (cursor) last matched by
    /// `#{pane_title}`. Pane scope. Cursor's title is `Cursor Agent` only when idle and a tool name
    /// during actions, so a per-cycle title check would drop identity mid-action; the resolver holds
    /// the match while the pid is unchanged. Stored here (not daemon memory) so it survives the
    /// one-shot POLL surfaces. Written ONLY for a title-narrowed manifest.
    pub const TITLE_MATCH_PID: &str = "@tma_title_match_pid";
    /// Dead-registration reaper marker: epoch **ms** of the first cycle a hook-registered pane
    /// (`@agent_pid == 0`) was seen with a SHELL-ONLY subtree. Pane scope. It time-bounds the reap:
    /// once shell-only persists past the threshold the poll cycle clears the registration (and this
    /// marker, via `REMOVABLE`). Any non-shell process reappearing clears it, so a live pid-less
    /// agent (gemini's steady `node`, matching no `process_names`) is never shell-only and holds.
    pub const REG_DEAD_SINCE: &str = "@tma_reg_dead_since";
}

/// The deserialized pane-option tuple for one agent pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedState {
    pub state: AgentState,
    pub detail: Option<Detail>,
    /// Provenance of the current state (`@agent_source`).
    pub source: Provenance,
    /// Epoch of the evidence behind the current state.
    pub evidence_at: u64,
    /// Epoch of the state transition — write-once while state is unchanged.
    pub since: u64,
    /// Per-pane freshness marker; written last in the chain.
    pub stamped_at: u64,
    /// Presentation flag (`@agent_attention` = `1` when set, absent otherwise).
    pub attention: bool,
    /// Episode notify marker; `None` when the episode has not been notified.
    pub notified_at: Option<u64>,
    /// Epoch of the last recorded turn end (`@agent_turn_at`); 0 when none was ever recorded.
    /// Only the hook intake writes it, and only for an event the manifest marks `turn_end`.
    pub turn_at: u64,
    /// Hash of the last captured viewport tail, paired with `stamped_at`.
    pub hash: Option<u64>,
    /// Process-group leader pid found by the walk (`@agent_pid`).
    pub pid: u32,
    /// Owning agent session id from hook registration; the subagent guard reads it.
    pub session: Option<String>,
    /// Live subagent session ids (`@agent_subagents`); bookkeeping only.
    pub subagents: Vec<String>,
}

/// A read of the pane-option tuple, tagged with a read-consistency verdict. Producers write
/// `@agent_stamped_at` last, so `stamped_at < since || stamped_at < evidence_at` caught a torn write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadResult {
    Settled(StampedState),
    InProgress(StampedState),
}

impl ReadResult {
    /// The stamped state regardless of settledness — callers that only need the values.
    pub fn into_inner(self) -> StampedState {
        match self {
            ReadResult::Settled(s) | ReadResult::InProgress(s) => s,
        }
    }
}

/// The `@agent_mute_until` value an indefinite mute writes: far enough out (year 5138) that no real
/// clock reaches it, so "muted until you clear it" needs no second option and no separate token to
/// parse. A deadline is a deadline; only the docs and `tma mute` know this one means *forever*.
pub const MUTE_FOREVER_MS: u64 = 99_999_999_999_999;

/// Whether a stored `@agent_mute_until` suppresses notifications at `now`. Pure, with the clock
/// injected, so both fire paths share one rule: an absent or unparsable deadline never mutes, and a
/// deadline that has passed is spent (the option can sit there until something clears it).
pub fn mute_active(mute_until: Option<u64>, now: u64) -> bool {
    mute_until.is_some_and(|until| until > now)
}

/// How far `@agent_since` may sit ahead of `@agent_stamped_at` before the tuple reads as
/// clock-stepped rather than mid-write. A chained stamp commits in milliseconds and both values
/// come from one producer's clock, so 2 s is pure slack for coarse clocks and rounding.
pub const CLOCK_STEP_SKEW_MS: u64 = 2_000;

/// Did a backward wall-clock step (suspend, NTP correction) leave `since` stranded in the future?
/// `since` is write-once while the state is unchanged, so it survives the step while `stamped_at`
/// is rewritten against the corrected clock, and the pair stays inverted until the state changes.
pub fn since_clock_stepped(since: u64, stamped_at: u64) -> bool {
    since > stamped_at.saturating_add(CLOCK_STEP_SKEW_MS)
}

/// Boundary between legacy epoch-**seconds** (10-digit) and epoch-**milliseconds** (13-digit)
/// stamps: 10^12 s is ~33658 AD, so any real wall-clock stamp is unambiguous (seconds below, ms above).
const MILLIS_FLOOR: u64 = 1_000_000_000_000;

/// Normalize a stored timestamp to ms: a nonzero value below [`MILLIS_FLOOR`] is legacy seconds
/// and is scaled up; zero (absent) and already-ms values pass through.
fn to_millis(v: u64) -> u64 {
    if v != 0 && v < MILLIS_FLOOR {
        v.saturating_mul(1000)
    } else {
        v
    }
}

/// Parse a required `u64` option (raw — no unit normalization; used for both epochs and the
/// pid). Timestamp call sites wrap the result in [`to_millis`]; the pid must not be scaled.
fn parse_int(opts: &HashMap<String, String>, key: &'static str) -> Result<u64, GrammarError> {
    match opts.get(key) {
        None => Ok(0),
        Some(v) => v.parse().map_err(|_| GrammarError::BadInteger {
            option: key,
            value: v.clone(),
        }),
    }
}

fn parse_opt_int(
    opts: &HashMap<String, String>,
    key: &'static str,
) -> Result<Option<u64>, GrammarError> {
    match opts.get(key) {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| GrammarError::BadInteger {
            option: key,
            value: v.clone(),
        }),
    }
}

impl StampedState {
    /// The instant this pane's current episode last became noteworthy: the state transition
    /// (`@agent_since`), or the last recorded turn end (`@agent_turn_at`) when a second completion
    /// landed inside an unchanged idle run. The one basis the notify dedup and `wait --since`
    /// compare against, so a re-raised done marker is a new episode to both.
    pub fn episode_at(&self) -> u64 {
        self.since.max(self.turn_at)
    }

    /// Decode the pane-option tuple from a `@agent_*` key ⇒ value map. `Ok(None)` when no
    /// `@agent_state`; `Err` on a malformed value; absent `@agent_source` decodes as overridable `capture`.
    pub fn from_options(
        opts: &HashMap<String, String>,
    ) -> Result<Option<ReadResult>, GrammarError> {
        let state = match opts.get(opt::STATE) {
            None => return Ok(None),
            Some(v) => v.parse::<AgentState>()?,
        };

        let detail = opts
            .get(opt::DETAIL)
            .filter(|v| !v.is_empty())
            .map(|v| Detail::new(v.clone()));

        let source = match opts.get(opt::SOURCE) {
            None => Provenance::Capture,
            Some(v) => v.parse::<Provenance>()?,
        };

        // Epoch fields are normalized to ms on read (legacy seconds → ms); the pid is raw.
        let evidence_at = to_millis(parse_int(opts, opt::EVIDENCE_AT)?);
        let since = to_millis(parse_int(opts, opt::SINCE)?);
        let stamped_at = to_millis(parse_int(opts, opt::STAMPED_AT)?);
        let pid =
            u32::try_from(parse_int(opts, opt::PID)?).map_err(|_| GrammarError::BadInteger {
                option: opt::PID,
                value: opts.get(opt::PID).cloned().unwrap_or_default(),
            })?;
        let notified_at = parse_opt_int(opts, opt::NOTIFIED_AT)?.map(to_millis);
        let turn_at = to_millis(parse_int(opts, opt::TURN_AT)?);
        let hash = parse_opt_int(opts, opt::HASH)?;

        let attention = opts.get(opt::ATTENTION).map(String::as_str) == Some("1");
        let session = opts.get(opt::SESSION).filter(|v| !v.is_empty()).cloned();
        let subagents = opts
            .get(opt::SUBAGENTS)
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();

        let stamp = StampedState {
            state,
            detail,
            source,
            evidence_at,
            since,
            stamped_at,
            attention,
            notified_at,
            turn_at,
            hash,
            pid,
            session,
            subagents,
        };

        // A `since` far past `stamped_at` is a clock step, not a torn write: reading it as
        // in-progress forever would re-capture the pane every cycle. A real tear still trips the
        // `evidence_at` half unless the yield lands between those two writes.
        let in_progress = !since_clock_stepped(stamp.since, stamp.stamped_at)
            && (stamp.stamped_at < stamp.since || stamp.stamped_at < stamp.evidence_at);
        Ok(Some(if in_progress {
            ReadResult::InProgress(stamp)
        } else {
            ReadResult::Settled(stamp)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every optional key present, in epoch **milliseconds** (13-digit) so nothing is rescaled
    /// by the legacy-seconds normalization (tested separately).
    fn full_map() -> HashMap<String, String> {
        [
            (opt::STATE, "blocked"),
            (opt::DETAIL, Detail::PERMISSION),
            (opt::SOURCE, "hook"),
            (opt::SINCE, "1700000001000"),
            (opt::EVIDENCE_AT, "1700000001000"),
            (opt::STAMPED_AT, "1700000002000"),
            (opt::ATTENTION, "1"),
            (opt::NOTIFIED_AT, "1700000002000"),
            (opt::TURN_AT, "1700000001500"),
            (opt::HASH, "3735928559"),
            (opt::PID, "4242"),
            (opt::SESSION, "sess-1"),
            (opt::SUBAGENTS, "sub-a sub-b"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn decodes_every_option() {
        let decoded = StampedState::from_options(&full_map()).unwrap().unwrap();
        assert_eq!(
            decoded,
            ReadResult::Settled(StampedState {
                state: AgentState::Blocked,
                detail: Some(Detail::new(Detail::PERMISSION)),
                source: Provenance::Hook,
                evidence_at: 1_700_000_001_000,
                since: 1_700_000_001_000,
                turn_at: 1_700_000_001_500,
                stamped_at: 1_700_000_002_000,
                attention: true,
                notified_at: Some(1_700_000_002_000),
                hash: Some(0xdead_beef),
                pid: 4242,
                session: Some("sess-1".to_string()),
                subagents: vec!["sub-a".to_string(), "sub-b".to_string()],
            })
        );
    }

    #[test]
    fn absent_optionals_decode_to_their_defaults() {
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "idle".to_string());
        map.insert(opt::SOURCE.to_string(), "capture".to_string());
        map.insert(opt::SINCE.to_string(), "1700000000000".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "1700000000000".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "1700000000000".to_string());
        map.insert(opt::PID.to_string(), "7".to_string());
        let decoded = StampedState::from_options(&map).unwrap().unwrap();
        assert_eq!(
            decoded,
            ReadResult::Settled(StampedState {
                state: AgentState::Idle,
                detail: None,
                source: Provenance::Capture,
                evidence_at: 1_700_000_000_000,
                since: 1_700_000_000_000,
                turn_at: 0,
                stamped_at: 1_700_000_000_000,
                attention: false,
                notified_at: None,
                hash: None,
                pid: 7,
                session: None,
                subagents: vec![],
            })
        );
    }

    /// The episode instant a re-raised done marker is measured against. `@agent_since` is
    /// write-once per state run, so a SECOND completion inside one idle run moves only
    /// `@agent_turn_at` — and reading `since` alone would dedup that completion away as one
    /// already notified. A pane that never had a turn end recorded reads `since` unchanged, so
    /// every comparison degrades to what it was before the field existed.
    #[test]
    fn episode_at_is_the_later_of_the_transition_and_the_last_turn_end() {
        let mut s = StampedState::from_options(&full_map())
            .unwrap()
            .unwrap()
            .into_inner();
        s.since = 1_700_000_001_000;

        s.turn_at = 0;
        assert_eq!(
            s.episode_at(),
            1_700_000_001_000,
            "no turn end: since alone"
        );

        s.turn_at = 1_700_000_009_000;
        assert_eq!(
            s.episode_at(),
            1_700_000_009_000,
            "a second completion inside one idle run is the newer episode"
        );

        // A transition always outruns the turn end that preceded it (`since` is set to the
        // event's own `now`), so the max never resurrects a stale completion.
        s.since = 1_700_000_010_000;
        assert_eq!(s.episode_at(), 1_700_000_010_000);
    }

    /// Legacy epoch-seconds normalization reaches the new field too: a pre-upgrade server holding
    /// a 10-digit value must not read as an epoch 1000x in the past, which would make every
    /// comparison against it meaningless.
    #[test]
    fn a_legacy_seconds_turn_at_scales_to_millis() {
        let mut map = full_map();
        map.insert(opt::TURN_AT.to_string(), "1700000001".to_string());
        let decoded = StampedState::from_options(&map)
            .unwrap()
            .unwrap()
            .into_inner();
        assert_eq!(decoded.turn_at, 1_700_000_001_000);
    }

    #[test]
    fn preserves_unknown_detail_token() {
        let mut map = full_map();
        map.insert(opt::DETAIL.to_string(), "frobnicating".to_string());
        let decoded = StampedState::from_options(&map)
            .unwrap()
            .unwrap()
            .into_inner();
        let d = decoded.detail.unwrap();
        assert_eq!(d.as_str(), "frobnicating");
    }

    #[test]
    fn absent_state_is_not_a_stamp() {
        let map = HashMap::new();
        assert_eq!(StampedState::from_options(&map).unwrap(), None);
    }

    #[test]
    fn in_progress_when_stamped_at_predates_since() {
        // stamped_at written last: an observer that sees it older than `since` caught a
        // torn write.
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "working".to_string());
        map.insert(opt::SINCE.to_string(), "200".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "150".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "199".to_string());
        assert!(matches!(
            StampedState::from_options(&map).unwrap(),
            Some(ReadResult::InProgress(_))
        ));
    }

    #[test]
    fn in_progress_when_stamped_at_predates_evidence_at() {
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "working".to_string());
        map.insert(opt::SINCE.to_string(), "100".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "250".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "249".to_string());
        assert!(matches!(
            StampedState::from_options(&map).unwrap(),
            Some(ReadResult::InProgress(_))
        ));
    }

    #[test]
    fn settled_when_stamped_at_is_newest() {
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "idle".to_string());
        map.insert(opt::SINCE.to_string(), "100".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "100".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "100".to_string());
        assert!(matches!(
            StampedState::from_options(&map).unwrap(),
            Some(ReadResult::Settled(_))
        ));
    }

    #[test]
    fn clock_stepped_since_reads_as_settled() {
        // A backward wall-clock step leaves the write-once `since` (and the `evidence_at` a hold
        // does not rewrite) stranded ahead of the corrected `stamped_at`. Reading that as
        // in-progress would fail the freshness gate every cycle, re-capturing the pane forever.
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "working".to_string());
        map.insert(opt::SINCE.to_string(), "1753203600000".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "1753203600000".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "1753200000000".to_string());
        assert!(matches!(
            StampedState::from_options(&map).unwrap(),
            Some(ReadResult::Settled(_))
        ));
    }

    #[test]
    fn a_torn_write_inside_the_skew_allowance_still_reads_in_progress() {
        // `since` a few hundred ms ahead of `stamped_at` is a chained write caught mid-flight,
        // not a clock step: the read-consistency verdict must still be in-progress.
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "working".to_string());
        map.insert(opt::SINCE.to_string(), "1753200000500".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "1753200000000".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "1753200000000".to_string());
        assert!(matches!(
            StampedState::from_options(&map).unwrap(),
            Some(ReadResult::InProgress(_))
        ));
        assert!(since_clock_stepped(3_001, 1_000));
        assert!(!since_clock_stepped(3_000, 1_000));
        assert!(!since_clock_stepped(0, 0));
    }

    #[test]
    fn legacy_seconds_stamp_normalizes_to_millis_on_read() {
        // A store written by a pre-migration (epoch-seconds) build: 10-digit `*_at` values.
        // They must read back as milliseconds so the fold/freshness see one unit, while the
        // pid (also a small integer) is left untouched.
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "blocked".to_string());
        map.insert(opt::SINCE.to_string(), "1753200000".to_string()); // epoch seconds
        map.insert(opt::EVIDENCE_AT.to_string(), "1753200000".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "1753200001".to_string());
        map.insert(opt::NOTIFIED_AT.to_string(), "1753199999".to_string());
        map.insert(opt::PID.to_string(), "4242".to_string());
        let s = StampedState::from_options(&map)
            .unwrap()
            .unwrap()
            .into_inner();
        assert_eq!(s.since, 1_753_200_000_000, "seconds scaled to ms");
        assert_eq!(s.evidence_at, 1_753_200_000_000);
        assert_eq!(s.stamped_at, 1_753_200_001_000);
        assert_eq!(s.notified_at, Some(1_753_199_999_000));
        assert_eq!(s.pid, 4242, "pid is not a timestamp — never scaled");
    }

    #[test]
    fn millis_stamp_reads_unchanged() {
        // A 13-digit ms value is already normalized and passes through untouched.
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "idle".to_string());
        map.insert(opt::SINCE.to_string(), "1753200000500".to_string());
        map.insert(opt::EVIDENCE_AT.to_string(), "1753200000500".to_string());
        map.insert(opt::STAMPED_AT.to_string(), "1753200000500".to_string());
        let s = StampedState::from_options(&map)
            .unwrap()
            .unwrap()
            .into_inner();
        assert_eq!(s.since, 1_753_200_000_500);
        assert_eq!(s.stamped_at, 1_753_200_000_500);
    }

    #[test]
    fn rejects_bad_state_token() {
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "spinning".to_string());
        assert_eq!(
            StampedState::from_options(&map),
            Err(GrammarError::UnknownState("spinning".to_string()))
        );
    }

    #[test]
    fn rejects_non_numeric_epoch() {
        let mut map = HashMap::new();
        map.insert(opt::STATE.to_string(), "idle".to_string());
        map.insert(opt::SINCE.to_string(), "not-a-number".to_string());
        assert_eq!(
            StampedState::from_options(&map),
            Err(GrammarError::BadInteger {
                option: opt::SINCE,
                value: "not-a-number".to_string(),
            })
        );
    }

    #[test]
    fn mute_holds_until_its_deadline_passes() {
        const NOW: u64 = 1_700_000_000_000;
        assert!(!mute_active(None, NOW), "no deadline never mutes");
        assert!(mute_active(Some(NOW + 1), NOW), "a future deadline mutes");
        // The deadline itself is the first moment sound returns, so a `--for 0s` mute is a no-op
        // rather than a mute that outlives its own duration.
        assert!(!mute_active(Some(NOW), NOW));
        assert!(!mute_active(Some(NOW - 1), NOW), "a spent deadline is over");
        assert!(
            mute_active(Some(MUTE_FOREVER_MS), NOW),
            "the indefinite sentinel outlasts any real clock"
        );
    }

    // ---- option-registry ⇄ docs drift guard ----------------------------------------------
    // User-readable options live in reference/pane-options-and-json.md; internal bookkeeping
    // options live in the internal/ARCHITECTURE.md option table. Every live constant must
    // appear in one of the two.

    const PANE_OPTIONS_MD: &str = include_str!("../../../docs/reference/pane-options-and-json.md");
    const INTERNAL_ARCH_MD: &str = include_str!("../../../docs/internal/ARCHITECTURE.md");

    /// Every option-key constant in [`opt`], by `opt::` path so a rename breaks compilation here. A
    /// new option must be added here AND to one of the two option tables (the friction is the checklist).
    const ALL_OPT_KEYS: &[&str] = &[
        opt::NAME,
        opt::STATE,
        opt::DETAIL,
        opt::SOURCE,
        opt::EVIDENCE_AT,
        opt::SINCE,
        opt::STAMPED_AT,
        opt::ATTENTION,
        opt::NOTIFIED_AT,
        opt::TURN_AT,
        opt::HASH,
        opt::PID,
        opt::SESSION,
        opt::SUBAGENTS,
        opt::IGNORE,
        opt::MUTE_UNTIL,
        opt::CONTEXT_PCT,
        opt::CONTEXT_AT,
        opt::CONTEXT_NOTIFIED_AT,
        opt::MODEL,
        opt::PERMISSION_REQUEST,
        opt::PENDING_TOOL,
        opt::PENDING_CALL,
        opt::PENDING_SUMMARY,
        opt::API_ENDPOINT,
        opt::ACTION,
        opt::ACT_REPEAT,
        opt::SUMMARY,
        opt::SESSION_SUMMARY,
        opt::WATCH_PID,
        opt::LAST_POLL,
        opt::SETPF_OK,
        opt::ORIGIN_PREFIX,
        opt::TITLE_MATCH_PID,
        opt::REG_DEAD_SINCE,
    ];

    /// Backtick-quoted `@…` tokens from the first cell of each row of the `| option | scope |
    /// semantics |` table. Scoped to the table so `@agent_*` mentions in prose aren't counted.
    fn option_table_tokens(md: &str) -> Vec<String> {
        let mut lines = md.lines();
        // Advance to the table header, then take the contiguous block of `|`-rows after it.
        let found = lines.by_ref().any(|l| {
            let t = l.trim();
            t.starts_with("| option") && t.contains("| scope") && t.contains("| semantics")
        });
        assert!(
            found,
            "option table header (`| option | scope | semantics |`) not found"
        );
        let mut tokens = Vec::new();
        for line in lines {
            let t = line.trim_start();
            if !t.starts_with('|') {
                break; // first non-row line ends the table
            }
            // First cell: between the first and second unescaped `|`.
            let first_cell = t[1..].split('|').next().unwrap_or("");
            for tok in first_cell.split('`') {
                if tok.starts_with('@') {
                    tokens.push(tok.to_string());
                }
            }
        }
        tokens
    }

    /// Does the documented table cover `key`? An exact-key row matches by equality; a prefix
    /// key (ends with `_`, e.g. `@tma_origin_`) matches a `@tma_origin_<client>` row by prefix.
    fn table_documents(tokens: &[String], key: &str) -> bool {
        if key.ends_with('_') {
            tokens.iter().any(|t| t.starts_with(key))
        } else {
            tokens.iter().any(|t| t == key)
        }
    }

    #[test]
    fn every_opt_constant_is_documented_in_reference_table() {
        let mut tokens = option_table_tokens(PANE_OPTIONS_MD);
        tokens.extend(option_table_tokens(INTERNAL_ARCH_MD));
        // The separator rows (`|---|`) yield no `@` tokens; the real tables have plenty.
        assert!(
            tokens.len() >= ALL_OPT_KEYS.len(),
            "option tables extracted only {} option tokens",
            tokens.len()
        );
        for key in ALL_OPT_KEYS {
            assert!(
                table_documents(&tokens, key),
                "option `{key}` is a live constant but is missing from both option tables \
                 (docs/reference/pane-options-and-json.md and docs/internal/ARCHITECTURE.md)"
            );
        }
    }

    #[test]
    fn reference_option_table_drift_is_caught() {
        // Failability: drop the `@agent_state` row from the doc and the guard must fire.
        let mutated: String = PANE_OPTIONS_MD
            .lines()
            .filter(|l| !l.trim_start().starts_with("| `@agent_state`"))
            .collect::<Vec<_>>()
            .join("\n");
        let tokens = option_table_tokens(&mutated);
        assert!(
            !table_documents(&tokens, opt::STATE),
            "dropping the @agent_state row must make the guard see it as undocumented"
        );
    }
}
