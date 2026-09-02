//! Pure rendering of a [`WritePlan`](crate::verdict::WritePlan) into the chained `tmux
//! set-option` invocation that commits it: string building only, no spawn, no clock.
//!
//! Chain rule: every field carries the same suppression [`Guard`] (a server-side `set -pF`
//! conditional re-evaluated at write time, TOCTOU-safe), so the tuple commits or holds as one.
//! `@agent_since` writes first (write-once reads the old state), `@agent_stamped_at` last.
//! Atomicity assumes tmux never interleaves another client mid-`;`-chain: a yield could flip
//! `@agent_source` between fields and settle a contradictory tuple (`cross_chain_guard_tear_*`).

use crate::evidence::Provenance;
use crate::stamp::{opt, StampedState};
use crate::state::{AgentState, Detail};

/// One tmux command: the argv after the `tmux <server-args>` prefix the binary prepends. A
/// publish renders to a `Vec` of these, `;`-joined into one spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampCommand {
    pub argv: Vec<String>,
}

/// The suppression guard shared by every field of a chained write. Each variant renders to a
/// tmux format that is truthy when the write must be suppressed (held at the stored value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// Never suppress: the producer may override any stored claim (a fresh hook event,
    /// process evidence / foreground cap, a decayed hook claim, or an episode reset).
    Unconditional,
    /// A working/idle capture write must not clobber *any* hook claim. Suppress iff the
    /// stored source is `hook`.
    ProtectHook,
    /// Blocker chrome overrides a working/idle hook claim iff the capture *postdates* the
    /// stamped evidence. Suppress iff stored source is `hook` and capture ≤ `@agent_evidence_at`.
    CarveOut { capture_at: u64 },
    /// Advance a hook claim's `@agent_evidence_at` (resetting decay) only while the store still
    /// shows that claim. Suppress iff stored `@agent_state` no longer equals the claim's state.
    RefreshClaim { state: AgentState },
    /// Timestamp arbitration for a fresh hook stamp: suppress iff stored source is `hook` and
    /// this event's `evidence_at` predates the stored `@agent_evidence_at`, so racing hooks
    /// resolve by evidence time, not process finish order. First/equal-or-newer events write.
    /// Fails safe against a legacy 10-digit seconds stamp: a 13-digit ms value never predates it,
    /// so it falls back to last-writer-wins for the one cycle until an ms write lands.
    HookArbitrate { evidence_at: u64 },
}

impl Guard {
    /// The tmux format that evaluates truthy (`1`) when the write must be suppressed.
    fn suppress_expr(self) -> String {
        match self {
            Guard::Unconditional => "0".to_string(),
            Guard::ProtectHook => format!("#{{==:#{{{src}}},hook}}", src = opt::SOURCE),
            Guard::CarveOut { capture_at } => format!(
                "#{{&&:#{{==:#{{{src}}},hook}},#{{e|<=:{cap},#{{{ev}}}}}}}",
                src = opt::SOURCE,
                cap = capture_at,
                ev = opt::EVIDENCE_AT,
            ),
            Guard::RefreshClaim { state } => format!(
                "#{{?#{{==:#{{{st}}},{s}}},0,1}}",
                st = opt::STATE,
                s = state.token(),
            ),
            Guard::HookArbitrate { evidence_at } => format!(
                "#{{&&:#{{==:#{{{src}}},hook}},#{{e|<:{ev},#{{{stored}}}}}}}",
                src = opt::SOURCE,
                ev = evidence_at,
                stored = opt::EVIDENCE_AT,
            ),
        }
    }

    /// Client-side mirror of `suppress_expr`: whether this guard suppresses given prior stored
    /// `prev`. Lets a producer that already read `prev` predict the outcome without a read-back.
    pub fn suppresses(self, prev: Option<&StampedState>) -> bool {
        match self {
            Guard::Unconditional => false,
            Guard::ProtectHook => prev.is_some_and(|p| p.source == Provenance::Hook),
            Guard::CarveOut { capture_at } => {
                prev.is_some_and(|p| p.source == Provenance::Hook && capture_at <= p.evidence_at)
            }
            // suppress = stored @agent_state no longer equals the claimed state; an absent prior
            // (empty stored state) counts as diverged, matching the server format's `!= state`.
            Guard::RefreshClaim { state } => prev.is_none_or(|p| p.state != state),
            Guard::HookArbitrate { evidence_at } => {
                prev.is_some_and(|p| p.source == Provenance::Hook && evidence_at < p.evidence_at)
            }
        }
    }
}

/// A guarded state publish. All values are precomputed by the pure fold + the binary's
/// identity/clock edges; this struct is the complete input to [`render_publish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publish {
    pub pane_id: String,
    pub state: AgentState,
    pub detail: Option<Detail>,
    pub source: Provenance,
    /// Epoch of the evidence behind the new state.
    pub evidence_at: u64,
    /// Transition epoch used *only if* the state actually changes (write-once).
    pub since: u64,
    pub stamped_at: u64,
    pub hash: Option<u64>,
    pub pid: u32,
    /// Agent name for `@agent_name` (identity; deterministic, written unconditionally).
    pub name: String,
    /// Set `@agent_attention` on a noteworthy transition (blocked; working→idle).
    pub set_attention: bool,
    /// Pid-change episode boundary: write unconditionally, reset `since`, clear attention
    /// and `notified_at`.
    pub episode_reset: bool,
    /// The shared suppression guard (ignored when `episode_reset`, which writes unguarded).
    pub guard: Guard,
}

fn set_fmt(pane: &str, key: &str, value: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-F".into(),
            "-t".into(),
            pane.into(),
            key.into(),
            value.into(),
        ],
    }
}

fn set_plain(pane: &str, key: &str, value: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-t".into(),
            pane.into(),
            key.into(),
            value.into(),
        ],
    }
}

fn unset_pane(pane: &str, key: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-u".into(),
            "-t".into(),
            pane.into(),
            key.into(),
        ],
    }
}

/// Wrap `new_value` so a suppressed write keeps the stored value of `key`:
/// `#{?<suppress>,#{<key>},<new_value>}`.
fn guarded(suppress: &str, key: &str, new_value: &str) -> String {
    format!("#{{?{suppress},#{{{key}}},{new_value}}}")
}

/// Drop a detail token that would break the `set -pF` guard chain (a `#{},` or whitespace byte
/// corrupts the tuple). The manifest loader rejects these at declaration; last-ditch guard here.
fn safe_detail(detail: &str) -> &str {
    let corrupt = detail.bytes().any(|b| {
        matches!(b, b'#' | b'{' | b'}' | b',') || b.is_ascii_whitespace() || b.is_ascii_control()
    });
    if corrupt {
        ""
    } else {
        detail
    }
}

/// Render a guarded state publish into its chained `set-option` commands, in write order
/// (`@agent_since` first, `@agent_stamped_at` last).
pub fn render_publish(p: &Publish) -> Vec<StampCommand> {
    let t = &p.pane_id;
    let mut cmds = Vec::with_capacity(12);
    let suppress = p.guard.suppress_expr();
    let detail_new = safe_detail(p.detail.as_ref().map(Detail::as_str).unwrap_or(""));

    if p.episode_reset {
        // A different pid owns the pane now: the stored tuple belongs to a dead agent —
        // overwrite it unconditionally and reset the episode fields.
        cmds.push(set_plain(t, opt::SINCE, &p.since.to_string()));
        cmds.push(set_plain(t, opt::STATE, p.state.token()));
        match &p.detail {
            Some(d) => cmds.push(set_plain(t, opt::DETAIL, d.as_str())),
            None => cmds.push(unset_pane(t, opt::DETAIL)),
        }
        cmds.push(set_plain(t, opt::SOURCE, p.source.token()));
        cmds.push(set_plain(t, opt::EVIDENCE_AT, &p.evidence_at.to_string()));
        // Episode boundary clears attention, the notification marker, and the last turn end: the
        // completion `@agent_turn_at` records belongs to the agent that just went away. Under a
        // monotone clock the fresh `since` dominates it in `episode_at()` anyway, but a backward
        // clock step would let the dead agent's turn decide the new episode's instant.
        if p.set_attention {
            cmds.push(set_plain(t, opt::ATTENTION, "1"));
        } else {
            cmds.push(unset_pane(t, opt::ATTENTION));
        }
        cmds.push(unset_pane(t, opt::NOTIFIED_AT));
        cmds.push(unset_pane(t, opt::TURN_AT));
    } else {
        // `@agent_since` is write-once per episode: hold the stored value while state is
        // unchanged, else record the transition. The guard wraps it so a held tuple never bumps
        // `since` (which would re-fire the episode notify). The second arm rewrites a stored
        // `since` stranded in the future by a backward clock step, which write-once would keep
        // forever (see `stamp::since_clock_stepped`).
        let once = format!(
            "#{{?#{{&&:#{{==:#{{{st}}},{new}}},#{{e|<=:#{{{since}}},{limit}}}}},#{{{since}}},{new_since}}}",
            st = opt::STATE,
            new = p.state.token(),
            since = opt::SINCE,
            limit = p.stamped_at.saturating_add(crate::stamp::CLOCK_STEP_SKEW_MS),
            new_since = p.since,
        );
        let since_val = guarded(&suppress, opt::SINCE, &once);
        cmds.push(set_fmt(t, opt::SINCE, &since_val));

        cmds.push(set_fmt(
            t,
            opt::STATE,
            &guarded(&suppress, opt::STATE, p.state.token()),
        ));
        cmds.push(set_fmt(
            t,
            opt::DETAIL,
            &guarded(&suppress, opt::DETAIL, detail_new),
        ));
        cmds.push(set_fmt(
            t,
            opt::SOURCE,
            &guarded(&suppress, opt::SOURCE, p.source.token()),
        ));
        cmds.push(set_fmt(
            t,
            opt::EVIDENCE_AT,
            &guarded(&suppress, opt::EVIDENCE_AT, &p.evidence_at.to_string()),
        ));
        // Attention rides the same guard: a suppressed non-transition sets nothing, and a publish
        // without a noteworthy transition leaves any existing flag untouched (focus hooks clear
        // it). Suppressing a guarded write whose stored option is absent expands to empty
        // (present-but-empty), which decoders read as absent — inert.
        if p.set_attention {
            cmds.push(set_fmt(
                t,
                opt::ATTENTION,
                &guarded(&suppress, opt::ATTENTION, "1"),
            ));
        }
    }

    // Freshness/baseline/identity fields — always written (even a held tuple refreshes
    // stamped_at + hash). Identity is deterministic for the same pid.
    if let Some(h) = p.hash {
        cmds.push(set_plain(t, opt::HASH, &h.to_string()));
    }
    cmds.push(set_plain(t, opt::PID, &p.pid.to_string()));
    if !p.name.is_empty() {
        cmds.push(set_plain(t, opt::NAME, &p.name));
    }
    cmds.push(set_plain(t, opt::STAMPED_AT, &p.stamped_at.to_string()));
    cmds
}

/// The (state, since, attention) a guarded [`render_publish`] leaves in the store after its
/// chained write, as a pure function of the prior stored tuple `prev`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowProjection {
    pub state: AgentState,
    pub since: u64,
    pub attention: bool,
}

/// What a guarded publish leaves in the store, for the poll row: it must show what the store
/// will hold, not the intended verdict (a suppressed producer commits nothing). Deriving the row
/// from this one mirror of [`render_publish`] keeps it from drifting from the write; evaluated
/// against `prev`, so a hook landing after that read can diverge server-side (self-corrects).
pub fn project_publish(prev: Option<&StampedState>, p: &Publish) -> RowProjection {
    // A suppressed guard HOLDS the store; with a stored tuple it keeps that tuple. But suppression
    // does not imply a stored tuple: `RefreshClaim`/`ProtectHook` treat an absent store as diverged
    // and suppress with `prev == None`. That case holds absence (no tuple commits), so the row
    // reflects this cycle's fold verdict (`p.state`) with the publish's own since/attention.
    if !p.episode_reset && p.guard.suppresses(prev) {
        return match prev {
            Some(held) => RowProjection {
                state: held.state,
                since: held.since,
                attention: held.attention,
            },
            None => RowProjection {
                state: p.state,
                since: p.since,
                attention: p.set_attention,
            },
        };
    }
    // The write commits: `@agent_since` is write-once (kept while the state is unchanged, reset
    // by an episode boundary), attention is set on a noteworthy transition and otherwise held
    // (cleared only by an episode reset without one) — mirroring render_publish exactly.
    let since = match prev {
        Some(prev)
            if !p.episode_reset
                && prev.state == p.state
                && !crate::stamp::since_clock_stepped(prev.since, p.stamped_at) =>
        {
            prev.since
        }
        _ => p.since,
    };
    let attention =
        p.set_attention || (!p.episode_reset && prev.is_some_and(|prev| prev.attention));
    RowProjection {
        state: p.state,
        since,
        attention,
    }
}

/// **Documented degrade**: advisory (unguarded, plain) rendering for a tmux lacking `set -pF`
/// expansion, where a `-F` write would store the literal `#{?...}` and corrupt the tuple. Fields
/// resolve producer-side; `@agent_since` write-once uses the caller's `prev_state`/`prev_since`.
/// Read-then-write and NOT TOCTOU-safe: a hook landing between read and write is clobbered (the
/// accepted degrade). Write order matches [`render_publish`], `@agent_stamped_at` last.
pub fn render_publish_advisory(
    p: &Publish,
    prev_state: Option<AgentState>,
    prev_since: u64,
) -> Vec<StampCommand> {
    let t = &p.pane_id;
    let mut cmds = Vec::with_capacity(12);

    if p.episode_reset {
        // A different pid owns the pane now: overwrite the dead agent's tuple and reset the
        // episode fields, exactly as the guarded path does.
        cmds.push(set_plain(t, opt::SINCE, &p.since.to_string()));
        cmds.push(set_plain(t, opt::STATE, p.state.token()));
        match &p.detail {
            Some(d) => cmds.push(set_plain(t, opt::DETAIL, d.as_str())),
            None => cmds.push(unset_pane(t, opt::DETAIL)),
        }
        cmds.push(set_plain(t, opt::SOURCE, p.source.token()));
        cmds.push(set_plain(t, opt::EVIDENCE_AT, &p.evidence_at.to_string()));
        if p.set_attention {
            cmds.push(set_plain(t, opt::ATTENTION, "1"));
        } else {
            cmds.push(unset_pane(t, opt::ATTENTION));
        }
        cmds.push(unset_pane(t, opt::NOTIFIED_AT));
        cmds.push(unset_pane(t, opt::TURN_AT));
    } else {
        // Producer-side write-once: keep the stored `since` while the state is unchanged, else
        // record the transition — including when a clock step stranded the stored value in the
        // future. (The guarded path does this in-server with `-F`.)
        let since = if prev_state == Some(p.state)
            && !crate::stamp::since_clock_stepped(prev_since, p.stamped_at)
        {
            prev_since
        } else {
            p.since
        };
        cmds.push(set_plain(t, opt::SINCE, &since.to_string()));
        cmds.push(set_plain(t, opt::STATE, p.state.token()));
        match &p.detail {
            Some(d) => cmds.push(set_plain(t, opt::DETAIL, d.as_str())),
            None => cmds.push(unset_pane(t, opt::DETAIL)),
        }
        cmds.push(set_plain(t, opt::SOURCE, p.source.token()));
        cmds.push(set_plain(t, opt::EVIDENCE_AT, &p.evidence_at.to_string()));
        // Advisory attention: set `1` on a noteworthy transition, else leave the stored flag.
        // Without a guard there is no clear-iff-suppressed, and focus hooks own the clear anyway.
        if p.set_attention {
            cmds.push(set_plain(t, opt::ATTENTION, "1"));
        }
    }

    // Freshness/baseline/identity fields — always written, `@agent_stamped_at` last.
    if let Some(h) = p.hash {
        cmds.push(set_plain(t, opt::HASH, &h.to_string()));
    }
    cmds.push(set_plain(t, opt::PID, &p.pid.to_string()));
    if !p.name.is_empty() {
        cmds.push(set_plain(t, opt::NAME, &p.name));
    }
    cmds.push(set_plain(t, opt::STAMPED_AT, &p.stamped_at.to_string()));
    cmds
}

/// Render a plain pane-option set. The hook-event path appends the registration fields outside the
/// guarded tuple (`@agent_session`, `@agent_subagents`, `@agent_notified_at`); public for the seam.
pub fn set_pane_option(pane: &str, key: &str, value: &str) -> StampCommand {
    set_plain(pane, key, value)
}

/// Render a plain server-option set (`set-option -s`). Chainable, so the poll cycle's
/// `@tma_last_poll` claim can ride the stamp invocation instead of costing a second one.
pub fn set_server_option(key: &str, value: &str) -> StampCommand {
    StampCommand {
        argv: vec!["set-option".into(), "-s".into(), key.into(), value.into()],
    }
}

/// Render a companion pane-option set carrying the same suppression `guard` as the state tuple,
/// so it commits iff the state write does. Appended unguarded, a hook event that lost arbitration
/// would still clobber the notify marker or session. A suppressed write against an absent option
/// expands to present-but-empty, which decoders read as absent (inert, per [`render_publish`]).
pub fn set_pane_option_guarded(pane: &str, key: &str, guard: Guard, value: &str) -> StampCommand {
    set_fmt(pane, key, &guarded(&guard.suppress_expr(), key, value))
}

/// Render the removal of one pane option (`set-option -pu`). Companion to [`set_pane_option`]
/// for registration fields (e.g. clearing `@agent_subagents` when the last subagent stops).
pub fn unset_pane_option(pane: &str, key: &str) -> StampCommand {
    unset_pane(pane, key)
}

/// The evidence-time suppress expr for the context metric pair: truthy (suppress the write)
/// iff the stored `@agent_context_at` is strictly newer than this observation's `evidence_at`, so a
/// reordered stale push cannot walk the gauge backward. An absent `@agent_context_at` expands to the
/// empty string, which `e|>` treats as less-than any number, so a first observation always writes.
fn context_suppress_expr(evidence_at: u64) -> String {
    format!(
        "#{{e|>:#{{{at}}},{ev}}}",
        at = opt::CONTEXT_AT,
        ev = evidence_at
    )
}

/// Render the context metric write: `@agent_context_pct`, the `@agent_tokens` pair, then
/// `@agent_context_at` LAST, all under the evidence-time guard, as one chained invocation (atomic
/// w.r.t. other clients). `pct = None` is a null-clear: it writes an empty `@agent_context_pct`
/// (decoders read empty as absent) and still advances `@agent_context_at`, so a reordered pre-clear
/// duplicate cannot resurrect a stale value. `tokens = None` clears the count and its marker the same
/// way — a percent-only channel must not leave a previous channel's absolute standing beside a fresh
/// gauge, and an absent count has no evidence time to carry.
pub fn render_context(
    pane_id: &str,
    pct: Option<u8>,
    tokens: Option<u64>,
    evidence_at: u64,
) -> Vec<StampCommand> {
    let suppress = context_suppress_expr(evidence_at);
    let pct_new = pct.map(|p| p.to_string()).unwrap_or_default();
    // The count and its marker are written together, so an absent count carries no stamped time.
    let tokens_new = tokens.map(|t| t.to_string()).unwrap_or_default();
    let tokens_at_new = tokens.map(|_| evidence_at.to_string()).unwrap_or_default();
    vec![
        set_fmt(
            pane_id,
            opt::CONTEXT_PCT,
            &guarded(&suppress, opt::CONTEXT_PCT, &pct_new),
        ),
        set_fmt(
            pane_id,
            opt::TOKENS,
            &guarded(&suppress, opt::TOKENS, &tokens_new),
        ),
        set_fmt(
            pane_id,
            opt::TOKENS_AT,
            &guarded(&suppress, opt::TOKENS_AT, &tokens_at_new),
        ),
        set_fmt(
            pane_id,
            opt::CONTEXT_AT,
            &guarded(&suppress, opt::CONTEXT_AT, &evidence_at.to_string()),
        ),
    ]
}

/// **Documented degrade**: advisory (unguarded) context write for a tmux lacking `set -pF` expansion.
/// The caller has already decided this observation is not older than the stored `@agent_context_at`
/// (a producer-side read-decide-write, NOT TOCTOU-safe). `pct = None` unsets `@agent_context_pct` and
/// `tokens = None` unsets the count pair; `@agent_context_at` is written last, matching
/// [`render_context`]'s order.
pub fn render_context_advisory(
    pane_id: &str,
    pct: Option<u8>,
    tokens: Option<u64>,
    evidence_at: u64,
) -> Vec<StampCommand> {
    let mut cmds = Vec::with_capacity(4);
    match pct {
        Some(p) => cmds.push(set_plain(pane_id, opt::CONTEXT_PCT, &p.to_string())),
        None => cmds.push(unset_pane(pane_id, opt::CONTEXT_PCT)),
    }
    match tokens {
        Some(t) => {
            cmds.push(set_plain(pane_id, opt::TOKENS, &t.to_string()));
            cmds.push(set_plain(pane_id, opt::TOKENS_AT, &evidence_at.to_string()));
        }
        None => {
            cmds.push(unset_pane(pane_id, opt::TOKENS));
            cmds.push(unset_pane(pane_id, opt::TOKENS_AT));
        }
    }
    cmds.push(set_plain(
        pane_id,
        opt::CONTEXT_AT,
        &evidence_at.to_string(),
    ));
    cmds
}

/// One quota/cost observation to stamp. Every field is independently optional and a `None` CLEARS
/// its option: the four are written as one chain from one payload, so an observation that reports a
/// quota but no cost must not leave the previous payload's cost standing beside it.
#[derive(Clone, Copy, Debug, Default)]
pub struct QuotaStamp<'a> {
    pub pct: Option<u8>,
    /// The window token (`5h` / `7d` / `spend` / `primary` / `secondary`).
    pub window: Option<&'a str>,
    pub resets_at_ms: Option<u64>,
    /// The cost as its rendered two-decimal string (`tma_core::format_cost_usd`).
    pub cost_usd: Option<&'a str>,
}

/// The evidence-time suppress expr for the quota chain: truthy (suppress) iff the stored
/// `@agent_quota_at` is strictly newer than this observation. [`context_suppress_expr`]'s twin on its
/// own marker, so a quota push and a context push never gate each other.
fn quota_suppress_expr(evidence_at: u64) -> String {
    format!(
        "#{{e|>:#{{{at}}},{ev}}}",
        at = opt::QUOTA_AT,
        ev = evidence_at
    )
}

/// Render the quota/cost write: the three quota options, the cost, then `@agent_quota_at` LAST, all
/// under the evidence-time guard as one chained invocation. Same discipline as [`render_context`]: a
/// `None` field writes an empty value (decoders read empty as absent) and the marker advances
/// regardless, so a reordered stale push can neither walk the quota backward nor resurrect a
/// cleared one.
pub fn render_quota(pane_id: &str, q: &QuotaStamp, evidence_at: u64) -> Vec<StampCommand> {
    let suppress = quota_suppress_expr(evidence_at);
    let fields: [(&str, String); 4] = [
        (
            opt::QUOTA_PCT,
            q.pct.map(|p| p.to_string()).unwrap_or_default(),
        ),
        (opt::QUOTA_WINDOW, q.window.unwrap_or_default().to_string()),
        (
            opt::QUOTA_RESETS_AT,
            q.resets_at_ms.map(|v| v.to_string()).unwrap_or_default(),
        ),
        (opt::COST_USD, q.cost_usd.unwrap_or_default().to_string()),
    ];
    let mut cmds: Vec<StampCommand> = fields
        .iter()
        .map(|(key, value)| set_fmt(pane_id, key, &guarded(&suppress, key, value)))
        .collect();
    cmds.push(set_fmt(
        pane_id,
        opt::QUOTA_AT,
        &guarded(&suppress, opt::QUOTA_AT, &evidence_at.to_string()),
    ));
    cmds
}

/// **Documented degrade**: advisory (unguarded) quota write for a tmux lacking `set -pF` expansion.
/// The caller has already decided this observation is not older than the stored `@agent_quota_at`
/// (a producer-side read-decide-write, NOT TOCTOU-safe). Mirrors [`render_context_advisory`]:
/// present fields are set, absent ones unset, and the marker is written last.
pub fn render_quota_advisory(pane_id: &str, q: &QuotaStamp, evidence_at: u64) -> Vec<StampCommand> {
    let fields: [(&str, Option<String>); 4] = [
        (opt::QUOTA_PCT, q.pct.map(|p| p.to_string())),
        (opt::QUOTA_WINDOW, q.window.map(str::to_string)),
        (opt::QUOTA_RESETS_AT, q.resets_at_ms.map(|v| v.to_string())),
        (opt::COST_USD, q.cost_usd.map(str::to_string)),
    ];
    let mut cmds: Vec<StampCommand> = fields
        .iter()
        .map(|(key, value)| match value {
            Some(v) => set_plain(pane_id, key, v),
            None => unset_pane(pane_id, key),
        })
        .collect();
    cmds.push(set_plain(pane_id, opt::QUOTA_AT, &evidence_at.to_string()));
    cmds
}

/// The guarded set-from-absent write for the `context_high` notify marker: stamp `now` into
/// `@agent_context_notified_at` only when it is empty/absent, else hold the stored value. Paired with
/// a mandatory read-back at the tmux edge so two concurrent firers resolve to one bell (the same
/// shape as the lock acquire's first arm). `now` is the marker value (debuggability only).
pub fn render_context_notify_fire(pane_id: &str, now: u64) -> StampCommand {
    let value = format!(
        "#{{?#{{==:#{{{k}}},}},{now},#{{{k}}}}}",
        k = opt::CONTEXT_NOTIFIED_AT
    );
    set_fmt(pane_id, opt::CONTEXT_NOTIFIED_AT, &value)
}

/// **Documented degrade**: advisory (unguarded) plain-set of the `context_high` marker for a tmux
/// lacking `set -pF` expansion. The caller has already checked the marker is absent (a producer-side
/// read-decide-write, NOT TOCTOU-safe), matching [`render_context_advisory`]'s posture.
pub fn render_context_notify_fire_advisory(pane_id: &str, now: u64) -> StampCommand {
    set_plain(pane_id, opt::CONTEXT_NOTIFIED_AT, &now.to_string())
}

/// Rearm the `context_high` notify marker: unset `@agent_context_notified_at` so the next
/// crossing fires. Absent = armed, so a plain unset is the whole rearm.
pub fn render_context_notify_rearm(pane_id: &str) -> StampCommand {
    unset_pane(pane_id, opt::CONTEXT_NOTIFIED_AT)
}

/// Render a writes-on-hold refresh: `@agent_stamped_at` and `@agent_hash` only — never
/// state, never `@agent_evidence_at`. A frozen/suppressed/dwelling verdict.
pub fn render_hold(pane_id: &str, stamped_at: u64, hash: Option<u64>) -> Vec<StampCommand> {
    let mut cmds = Vec::with_capacity(2);
    if let Some(h) = hash {
        cmds.push(set_plain(pane_id, opt::HASH, &h.to_string()));
    }
    cmds.push(set_plain(pane_id, opt::STAMPED_AT, &stamped_at.to_string()));
    cmds
}

/// Every per-pane `@agent_*`/`@tma_*` option removed on agent exit (a pane that outlives its
/// agent must be cleared). The two `@tma_*` anchors ride the same removal for a symmetric
/// deregister: the title anchor (else a stale pid could re-anchor a later pane) and the reaper
/// marker. Server-scoped options (`@tma_last_poll`, `@tma_setpf_ok`, …) are absent, not pane state.
const REMOVABLE: &[&str] = &[
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
    opt::NAME,
    opt::SESSION,
    opt::SUBAGENTS,
    opt::CONTEXT_PCT,
    opt::CONTEXT_AT,
    opt::TOKENS,
    opt::TOKENS_AT,
    opt::CONTEXT_NOTIFIED_AT,
    opt::QUOTA_PCT,
    opt::QUOTA_WINDOW,
    opt::QUOTA_RESETS_AT,
    opt::QUOTA_AT,
    opt::COST_USD,
    opt::MODEL,
    opt::PERMISSION_REQUEST,
    opt::PENDING_TOOL,
    opt::PENDING_CALL,
    opt::PENDING_SUMMARY,
    opt::API_ENDPOINT,
    opt::TITLE_MATCH_PID,
    opt::REG_DEAD_SINCE,
];

/// Render the removal of all `@agent_*` options for a pane whose agent has exited.
pub fn render_remove(pane_id: &str) -> Vec<StampCommand> {
    REMOVABLE.iter().map(|k| unset_pane(pane_id, k)).collect()
}

/// The lanes [`REMOVABLE`] deliberately spares on a deregister but an uninstall must still clear:
/// the single-flight action lock (a live action outlives its agent's exit), the watcher's own pid
/// marker (its owner is a `tma watch`, not the agent), and a `tma mute` deadline (tma wrote it, so
/// tma cleans it up). `@agent_ignore` is spared even here: the user wrote that one.
const PURGEABLE: &[&str] = &[opt::ACTION, opt::WATCH_PID, opt::MUTE_UNTIL];

/// Render the removal of every pane-scoped option tma writes, plus the window and session rollups
/// the pane sits in: the uninstall sweep, after which a user's `#{@agent_state}` format reads
/// nothing rather than a state no longer being refreshed.
pub fn render_purge(pane_id: &str) -> Vec<StampCommand> {
    let mut cmds = render_remove(pane_id);
    cmds.extend(PURGEABLE.iter().map(|k| unset_pane(pane_id, k)));
    cmds.push(render_summary(SummaryScope::Window, pane_id, None));
    cmds.push(render_summary(SummaryScope::Session, pane_id, None));
    cmds
}

/// The `@agent_summary` window rollup for sibling pane states: fixed order `blocked working idle
/// unknown`, zero-count classes omitted, `None` when there are no agents (the caller then unsets).
pub fn summary_string(states: &[AgentState]) -> Option<String> {
    let (mut b, mut w, mut i, mut u) = (0u32, 0u32, 0u32, 0u32);
    for s in states {
        match s {
            AgentState::Blocked => b += 1,
            AgentState::Working => w += 1,
            AgentState::Idle => i += 1,
            AgentState::Unknown => u += 1,
        }
    }
    if b + w + i + u == 0 {
        return None;
    }
    let mut parts = Vec::with_capacity(4);
    if b > 0 {
        parts.push(format!("blocked:{b}"));
    }
    if w > 0 {
        parts.push(format!("working:{w}"));
    }
    if i > 0 {
        parts.push(format!("idle:{i}"));
    }
    if u > 0 {
        parts.push(format!("unknown:{u}"));
    }
    Some(parts.join(" "))
}

/// Which rollup a summary write targets: the per-window `@agent_summary` or its session-scoped
/// mirror. Same grammar, same writers, same guards — only the option key and tmux's scope flag
/// differ (a session option takes no flag; `-t` still accepts any pane in it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummaryScope {
    Window,
    Session,
}

impl SummaryScope {
    pub fn key(self) -> &'static str {
        match self {
            SummaryScope::Window => opt::SUMMARY,
            SummaryScope::Session => opt::SESSION_SUMMARY,
        }
    }

    /// The `set-option` scope flag, `None` for a session option (tmux's default scope).
    fn flag(self) -> Option<&'static str> {
        match self {
            SummaryScope::Window => Some("-w"),
            SummaryScope::Session => None,
        }
    }
}

/// Render the summary write carrying the same suppression `guard` as the pane stamp, so the rollup
/// commits iff the state write does — an unguarded rollup from a losing event would clobber the
/// winning claim's summary. `None` (no agents left in scope) can only unset, which no single `-F`
/// write does conditionally, so it falls back to a plain unset.
pub fn render_summary_guarded(
    scope: SummaryScope,
    target: &str,
    summary: Option<&str>,
    guard: Guard,
) -> StampCommand {
    match summary {
        Some(s) => summary_command(
            scope,
            target,
            Some(&guarded(&guard.suppress_expr(), scope.key(), s)),
            true,
        ),
        None => render_summary(scope, target, None),
    }
}

/// Render the summary option write. `target` is any locator in the scope (tmux resolves the pane's
/// window/session from it); `None` unsets it. Appended to the same chained invocation as the pane
/// stamp.
pub fn render_summary(scope: SummaryScope, target: &str, summary: Option<&str>) -> StampCommand {
    summary_command(scope, target, summary, false)
}

/// The one argv builder both summary writers share: `set-option [-w] [-F] [-u] -t <target> <key>
/// [value]`.
fn summary_command(
    scope: SummaryScope,
    target: &str,
    value: Option<&str>,
    fmt: bool,
) -> StampCommand {
    let mut argv = vec!["set-option".to_string()];
    if let Some(flag) = scope.flag() {
        argv.push(flag.into());
    }
    if fmt {
        argv.push("-F".into());
    }
    if value.is_none() {
        argv.push("-u".into());
    }
    argv.push("-t".into());
    argv.push(target.into());
    argv.push(scope.key().into());
    if let Some(v) = value {
        argv.push(v.into());
    }
    StampCommand { argv }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(c: &StampCommand) -> Vec<&str> {
        c.argv.iter().map(String::as_str).collect()
    }

    /// Find the value written for `key` in a publish (the last argv element of its command).
    fn value_for<'a>(cmds: &'a [StampCommand], key: &str) -> Option<&'a str> {
        cmds.iter()
            .find(|c| c.argv.iter().any(|a| a == key))
            .and_then(|c| c.argv.last())
            .map(String::as_str)
    }

    fn publish(state: AgentState, source: Provenance, guard: Guard) -> Publish {
        Publish {
            pane_id: "%7".into(),
            state,
            detail: None,
            source,
            evidence_at: 200,
            since: 200,
            stamped_at: 201,
            hash: Some(0xabc),
            pid: 4242,
            name: "claude".into(),
            set_attention: false,
            episode_reset: false,
            guard,
        }
    }

    #[test]
    fn stamped_at_is_written_last() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        let last = cmds.last().unwrap();
        assert_eq!(last.argv[last.argv.len() - 2], opt::STAMPED_AT);
    }

    #[test]
    fn since_is_written_before_state() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        let since_pos = cmds
            .iter()
            .position(|c| c.argv.contains(&opt::SINCE.to_string()));
        let state_pos = cmds
            .iter()
            .position(|c| c.argv.contains(&opt::STATE.to_string()));
        assert!(
            since_pos < state_pos,
            "since must precede state (write-once reads old state)"
        );
    }

    #[test]
    fn protect_hook_guard_wraps_every_tuple_field() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        let g = "#{==:#{@agent_source},hook}";
        for key in [opt::STATE, opt::SOURCE, opt::EVIDENCE_AT, opt::DETAIL] {
            let v = value_for(&cmds, key).unwrap();
            assert!(
                v.contains(g),
                "{key} value {v:?} must carry the shared guard"
            );
        }
        // since carries it too, plus its nested write-once.
        let since = value_for(&cmds, opt::SINCE).unwrap();
        assert!(since.contains(g));
        assert!(
            since.contains("@agent_state"),
            "since compares old state for write-once"
        );
    }

    #[test]
    fn state_write_keeps_stored_value_when_suppressed() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        assert_eq!(
            value_for(&cmds, opt::STATE).unwrap(),
            "#{?#{==:#{@agent_source},hook},#{@agent_state},working}"
        );
    }

    #[test]
    fn carveout_guard_compares_capture_against_stored_evidence() {
        let cmds = render_publish(&publish(
            AgentState::Blocked,
            Provenance::Capture,
            Guard::CarveOut { capture_at: 1500 },
        ));
        let v = value_for(&cmds, opt::STATE).unwrap();
        assert!(
            v.contains("#{e|<=:1500,#{@agent_evidence_at}}"),
            "carve-out embeds the capture time as a literal: {v}"
        );
        assert!(v.contains("#{==:#{@agent_source},hook}"));
    }

    #[test]
    fn unconditional_guard_has_no_source_check() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Hook,
            Guard::Unconditional,
        ));
        let v = value_for(&cmds, opt::STATE).unwrap();
        assert_eq!(v, "#{?0,#{@agent_state},working}");
    }

    #[test]
    fn hook_arbitrate_suppresses_against_strictly_newer_stored_hook() {
        // DAEMON.md items 3 & 6: the guard is truthy (suppress) iff stored source is hook AND
        // this event's evidence_at (`1500`) predates the stored `@agent_evidence_at`.
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Hook,
            Guard::HookArbitrate { evidence_at: 1500 },
        ));
        let v = value_for(&cmds, opt::STATE).unwrap();
        assert!(
            v.contains("#{e|<:1500,#{@agent_evidence_at}}"),
            "embeds the event time and compares it against the stored evidence: {v}"
        );
        assert!(
            v.contains("#{==:#{@agent_source},hook}"),
            "only a stored hook claim can suppress a hook write: {v}"
        );
    }

    #[test]
    fn refresh_claim_suppresses_when_stored_state_diverged() {
        let cmds = render_publish(&publish(
            AgentState::Working,
            Provenance::Hook,
            Guard::RefreshClaim {
                state: AgentState::Working,
            },
        ));
        let v = value_for(&cmds, opt::EVIDENCE_AT).unwrap();
        // suppress = state != working
        assert!(v.contains("#{?#{==:#{@agent_state},working},0,1}"), "{v}");
    }

    #[test]
    fn episode_reset_writes_unguarded_and_clears_episode_fields() {
        let mut p = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p.episode_reset = true;
        let cmds = render_publish(&p);
        // No -F guard on the state write — plain value.
        assert_eq!(value_for(&cmds, opt::STATE).unwrap(), "working");
        assert_eq!(value_for(&cmds, opt::SINCE).unwrap(), "200");
        // notified_at and attention are unset (argv ends with the key, not a value).
        let has_unset = |key: &str| {
            cmds.iter().any(|c| {
                c.argv.contains(&"-u".to_string()) && c.argv.last().map(String::as_str) == Some(key)
            })
        };
        assert!(
            has_unset(opt::NOTIFIED_AT),
            "episode reset clears notified_at"
        );
        assert!(
            has_unset(opt::ATTENTION),
            "episode reset clears attention (none set)"
        );
    }

    #[test]
    fn attention_only_emitted_when_set() {
        let no = render_publish(&publish(
            AgentState::Idle,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        assert!(value_for(&no, opt::ATTENTION).is_none());

        let mut p = publish(AgentState::Idle, Provenance::Capture, Guard::ProtectHook);
        p.set_attention = true;
        let yes = render_publish(&p);
        assert_eq!(
            value_for(&yes, opt::ATTENTION).unwrap(),
            "#{?#{==:#{@agent_source},hook},#{@agent_attention},1}"
        );
    }

    #[test]
    fn metacharacter_detail_renders_empty_not_corrupt_chain() {
        // Defense at the render site: a detail carrying a format metacharacter (which the
        // manifest loader rejects, but could reach here via display-tolerant stored state)
        // must not leak into the guarded `-pF` chain. It renders as an empty detail.
        let mut p = publish(AgentState::Blocked, Provenance::Capture, Guard::ProtectHook);
        p.detail = Some(Detail::new("per,mission}"));
        let cmds = render_publish(&p);
        assert_eq!(
            value_for(&cmds, opt::DETAIL).unwrap(),
            "#{?#{==:#{@agent_source},hook},#{@agent_detail},}"
        );
    }

    #[test]
    fn detail_clears_to_empty_when_none() {
        let cmds = render_publish(&publish(
            AgentState::Idle,
            Provenance::Capture,
            Guard::ProtectHook,
        ));
        assert_eq!(
            value_for(&cmds, opt::DETAIL).unwrap(),
            "#{?#{==:#{@agent_source},hook},#{@agent_detail},}"
        );
    }

    #[test]
    fn advisory_writes_are_plain_and_stamped_at_last() {
        // Advisory degrade: every field is a plain pane set/unset — no `-F`, no `#{...}` guard.
        let cmds = render_publish_advisory(
            &publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook),
            Some(AgentState::Working),
            150,
        );
        for c in &cmds {
            assert!(
                !c.argv.iter().any(|a| a == "-F"),
                "advisory must not use -F: {:?}",
                c.argv
            );
            assert!(
                !c.argv.iter().any(|a| a.contains("#{")),
                "advisory values must be literal, not formats: {:?}",
                c.argv
            );
        }
        assert_eq!(value_for(&cmds, opt::STATE).unwrap(), "working");
        let last = cmds.last().unwrap();
        assert_eq!(last.argv[last.argv.len() - 2], opt::STAMPED_AT);
    }

    #[test]
    fn advisory_since_is_write_once_producer_side() {
        // State unchanged (working == prev working): keep the stored `since`.
        let same = render_publish_advisory(
            &publish(
                AgentState::Working,
                Provenance::Capture,
                Guard::Unconditional,
            ),
            Some(AgentState::Working),
            150,
        );
        assert_eq!(
            value_for(&same, opt::SINCE).unwrap(),
            "150",
            "unchanged state keeps the stored since"
        );

        // Transition (idle → working): record the new `since`.
        let changed = render_publish_advisory(
            &publish(
                AgentState::Working,
                Provenance::Capture,
                Guard::Unconditional,
            ),
            Some(AgentState::Idle),
            150,
        );
        assert_eq!(
            value_for(&changed, opt::SINCE).unwrap(),
            "200",
            "a transition records the new since"
        );

        // No prior state (fresh pane): use the new `since`.
        let fresh = render_publish_advisory(
            &publish(
                AgentState::Working,
                Provenance::Capture,
                Guard::Unconditional,
            ),
            None,
            0,
        );
        assert_eq!(value_for(&fresh, opt::SINCE).unwrap(), "200");
    }

    #[test]
    fn a_clock_stepped_since_is_rewritten_not_held() {
        use crate::stamp::CLOCK_STEP_SKEW_MS;
        let p = publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::Unconditional,
        );
        let limit = p.stamped_at + CLOCK_STEP_SKEW_MS;

        // Guarded path: write-once keeps the stored value only while it is not stranded ahead of
        // this write's stamped_at, so the server-side expression carries that bound.
        let since = value_for(&render_publish(&p), opt::SINCE)
            .unwrap()
            .to_string();
        assert!(
            since.contains(&format!("#{{e|<=:#{{@agent_since}},{limit}}}")),
            "{since}"
        );

        // Advisory path: same rule, decided producer-side.
        let stepped = render_publish_advisory(&p, Some(AgentState::Working), limit + 1);
        assert_eq!(
            value_for(&stepped, opt::SINCE).unwrap(),
            "200",
            "a since past the skew allowance is rewritten to this write's since"
        );
        let held = render_publish_advisory(&p, Some(AgentState::Working), limit);
        assert_eq!(
            value_for(&held, opt::SINCE).unwrap(),
            limit.to_string(),
            "inside the allowance write-once still holds"
        );

        // The row projection mirrors the write.
        let mut prev = stored(AgentState::Working, Provenance::Capture, 100);
        prev.since = limit + 1;
        assert_eq!(project_publish(Some(&prev), &p).since, p.since);
        prev.since = limit;
        assert_eq!(project_publish(Some(&prev), &p).since, limit);
    }

    #[test]
    fn advisory_episode_reset_clears_attention_and_notified_at() {
        let mut p = publish(
            AgentState::Working,
            Provenance::Capture,
            Guard::Unconditional,
        );
        p.episode_reset = true;
        let cmds = render_publish_advisory(&p, Some(AgentState::Idle), 150);
        let has_unset = |key: &str| {
            cmds.iter().any(|c| {
                c.argv.contains(&"-u".to_string()) && c.argv.last().map(String::as_str) == Some(key)
            })
        };
        assert!(has_unset(opt::NOTIFIED_AT), "reset clears notified_at");
        assert!(
            has_unset(opt::ATTENTION),
            "reset clears attention (none set)"
        );
        assert_eq!(
            value_for(&cmds, opt::SINCE).unwrap(),
            "200",
            "reset uses new since"
        );
    }

    #[test]
    fn hold_writes_hash_then_stamped_at() {
        let cmds = render_hold("%7", 900, Some(0x1234));
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].argv.contains(&opt::HASH.to_string()));
        assert!(cmds[1].argv.contains(&opt::STAMPED_AT.to_string()));
        assert_eq!(cmds[1].argv.last().unwrap(), "900");
    }

    #[test]
    fn hold_without_hash_is_stamped_at_only() {
        let cmds = render_hold("%7", 900, None);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].argv.contains(&opt::STAMPED_AT.to_string()));
    }

    #[test]
    fn remove_unsets_every_agent_option() {
        let cmds = render_remove("%7");
        assert_eq!(cmds.len(), REMOVABLE.len());
        assert!(cmds.iter().all(|c| c.argv.contains(&"-u".to_string())));
        assert!(cmds
            .iter()
            .any(|c| c.argv.last().map(String::as_str) == Some(opt::STATE)));
        // Deregister symmetry: the two `@tma_*` identity/reaper anchors are cleared too,
        // so a killed-then-replaced pane leaves no stale pid to re-anchor and no reaper marker.
        for anchor in [opt::TITLE_MATCH_PID, opt::REG_DEAD_SINCE] {
            assert!(
                cmds.iter()
                    .any(|c| c.argv.last().map(String::as_str) == Some(anchor)),
                "render_remove must unset {anchor}"
            );
        }
    }

    /// The whole episode lane comes down at every teardown: the deregister, and both episode-reset
    /// arms (a new pid owns the pane, so the stored tuple belongs to a dead agent). `@agent_turn_at`
    /// is the one most easily forgotten — it is written on a different path from the rest of the
    /// tuple (only a `turn_end` hook writes it), so a miss leaves it on the user's server after an
    /// uninstall, or lets a dead agent's completion decide the new episode's instant the moment the
    /// wall clock steps backward and `since` stops dominating it.
    #[test]
    fn every_episode_teardown_clears_the_whole_episode_lane() {
        let unset_keys = |cmds: &[StampCommand]| -> Vec<String> {
            cmds.iter()
                .filter(|c| c.argv.contains(&"-u".to_string()))
                .filter_map(|c| c.argv.last().cloned())
                .collect()
        };

        let removed = unset_keys(&render_remove("%7"));
        for key in [opt::SINCE, opt::ATTENTION, opt::NOTIFIED_AT, opt::TURN_AT] {
            assert!(
                removed.iter().any(|k| k == key),
                "render_remove must unset {key}"
            );
        }

        // An episode reset rewrites `since` rather than unsetting it; the rest of the lane goes.
        let mut p = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p.episode_reset = true;
        for (label, cmds) in [
            ("render_publish", render_publish(&p)),
            (
                "render_publish_advisory",
                render_publish_advisory(&p, Some(AgentState::Idle), 150),
            ),
        ] {
            let unset = unset_keys(&cmds);
            for key in [opt::ATTENTION, opt::NOTIFIED_AT, opt::TURN_AT] {
                assert!(
                    unset.iter().any(|k| k == key),
                    "{label}'s episode reset must unset {key}"
                );
            }
            assert_eq!(
                value_for(&cmds, opt::SINCE).unwrap(),
                "200",
                "{label}'s episode reset writes the new since"
            );
        }
    }

    /// The uninstall sweep clears everything a deregister does, plus the lanes it spares and the
    /// two rollups: a key added to either list must show up here without another edit.
    #[test]
    fn purge_covers_the_deregister_set_the_spared_lanes_and_both_rollups() {
        let cmds = render_purge("%7");
        let keys: Vec<&str> = cmds
            .iter()
            .map(|c| c.argv.last().map(String::as_str).unwrap_or_default())
            .collect();
        for key in REMOVABLE.iter().chain(PURGEABLE) {
            assert!(keys.contains(key), "purge must unset {key}");
        }
        for key in [opt::SUMMARY, opt::SESSION_SUMMARY] {
            assert!(keys.contains(&key), "purge must unset the {key} rollup");
        }
        assert!(cmds.iter().all(|c| c.argv.contains(&"-u".to_string())));
    }

    fn stored(state: AgentState, source: Provenance, evidence_at: u64) -> StampedState {
        StampedState {
            state,
            detail: None,
            source,
            evidence_at,
            since: 42,
            turn_at: 0,
            stamped_at: evidence_at,
            attention: true,
            notified_at: None,
            hash: None,
            pid: 7,
            session: None,
            subagents: vec![],
        }
    }

    #[test]
    fn suppresses_mirrors_each_guard_variant() {
        let hook = stored(AgentState::Blocked, Provenance::Hook, 1000);
        let cap = stored(AgentState::Working, Provenance::Capture, 1000);

        // Unconditional never suppresses.
        assert!(!Guard::Unconditional.suppresses(Some(&hook)));
        // ProtectHook suppresses iff the stored source is hook.
        assert!(Guard::ProtectHook.suppresses(Some(&hook)));
        assert!(!Guard::ProtectHook.suppresses(Some(&cap)));
        assert!(!Guard::ProtectHook.suppresses(None));
        // CarveOut suppresses a hook claim the capture does not postdate.
        assert!(Guard::CarveOut { capture_at: 1000 }.suppresses(Some(&hook)));
        assert!(!Guard::CarveOut { capture_at: 1001 }.suppresses(Some(&hook)));
        assert!(!Guard::CarveOut { capture_at: 500 }.suppresses(Some(&cap)));
        // RefreshClaim suppresses when the stored state diverged (or is absent).
        assert!(!Guard::RefreshClaim {
            state: AgentState::Blocked
        }
        .suppresses(Some(&hook)));
        assert!(Guard::RefreshClaim {
            state: AgentState::Idle
        }
        .suppresses(Some(&hook)));
        assert!(Guard::RefreshClaim {
            state: AgentState::Blocked
        }
        .suppresses(None));
        // HookArbitrate suppresses a strictly-newer stored hook claim.
        assert!(Guard::HookArbitrate { evidence_at: 999 }.suppresses(Some(&hook)));
        assert!(!Guard::HookArbitrate { evidence_at: 1000 }.suppresses(Some(&hook)));
        assert!(!Guard::HookArbitrate { evidence_at: 999 }.suppresses(Some(&cap)));
    }

    #[test]
    fn project_publish_holds_prev_when_suppressed() {
        // A working capture publish (ProtectHook) against a stored hook claim: suppressed, so the
        // row shows the held hook tuple, not the producer's intended working state.
        let prev = stored(AgentState::Blocked, Provenance::Hook, 500);
        let mut p = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p.since = 900;
        p.set_attention = true;
        let proj = project_publish(Some(&prev), &p);
        assert_eq!(proj.state, AgentState::Blocked, "held state");
        assert_eq!(proj.since, 42, "held since");
        assert!(proj.attention, "held attention");
    }

    #[test]
    fn project_publish_holds_absent_when_suppressed_with_no_prior() {
        // A `RefreshClaim` treats an absent store as diverged and suppresses even with `prev ==
        // None` (the trap the old `if let Some` branch fell through, projecting a committed write
        // it never made). The store holds absence, so the row reflects this cycle's fold verdict
        // (`p.state`) with the publish's own since/attention — there is no prior tuple to carry.
        let mut p = publish(
            AgentState::Working,
            Provenance::Hook,
            Guard::RefreshClaim {
                state: AgentState::Working,
            },
        );
        p.since = 900;
        p.set_attention = true;
        assert!(
            p.guard.suppresses(None),
            "RefreshClaim suppresses against an absent store"
        );
        let proj = project_publish(None, &p);
        assert_eq!(
            proj.state,
            AgentState::Working,
            "row shows the fold verdict"
        );
        assert_eq!(proj.since, 900, "no prior tuple: the publish's own since");
        assert!(
            proj.attention,
            "no prior tuple: the publish's own attention"
        );
    }

    #[test]
    fn project_publish_commits_write_once_since_and_attention() {
        // Unsuppressed transition (idle prev → working): new since, no attention (working is not
        // a noteworthy transition and the prior had none).
        let prev = stored(AgentState::Idle, Provenance::Capture, 500);
        // stored() sets attention=true; use a fresh prev without it for the hold check.
        let prev = StampedState {
            attention: false,
            ..prev
        };
        let mut p = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p.since = 900;
        let proj = project_publish(Some(&prev), &p);
        assert_eq!(proj.state, AgentState::Working);
        assert_eq!(proj.since, 900, "a transition records the new since");
        assert!(!proj.attention);

        // Unchanged state keeps the stored since; a set_attention transition sets the flag.
        let prev2 = StampedState {
            state: AgentState::Working,
            attention: false,
            ..prev
        };
        let mut p2 = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p2.since = 900;
        p2.set_attention = true;
        let proj2 = project_publish(Some(&prev2), &p2);
        assert_eq!(proj2.since, 42, "unchanged state keeps stored since");
        assert!(proj2.attention, "noteworthy transition sets attention");
    }

    #[test]
    fn project_publish_episode_reset_writes_unguarded() {
        // An episode reset writes unconditionally (even against a stored hook), resetting since.
        let prev = stored(AgentState::Blocked, Provenance::Hook, 500);
        let mut p = publish(AgentState::Working, Provenance::Capture, Guard::ProtectHook);
        p.episode_reset = true;
        p.since = 900;
        let proj = project_publish(Some(&prev), &p);
        assert_eq!(
            proj.state,
            AgentState::Working,
            "reset overrides the hook claim"
        );
        assert_eq!(proj.since, 900, "reset records the new since");
        assert!(!proj.attention, "reset without set_attention clears it");
    }

    #[test]
    fn summary_guarded_holds_stored_when_suppressed() {
        let cmd = render_summary_guarded(
            SummaryScope::Window,
            "%7",
            Some("blocked:1"),
            Guard::ProtectHook,
        );
        assert!(cmd.argv.contains(&"-F".to_string()));
        let v = cmd.argv.last().unwrap();
        assert_eq!(
            v, "#{?#{==:#{@agent_source},hook},#{@agent_summary},blocked:1}",
            "guarded summary holds the stored window rollup when the pane stamp is suppressed"
        );
        // None (agentless) can only fall back to a plain unset.
        let unset = render_summary_guarded(SummaryScope::Window, "%7", None, Guard::ProtectHook);
        assert!(unset.argv.contains(&"-u".to_string()));
        // The session mirror holds its OWN stored key, never the window's.
        let session = render_summary_guarded(
            SummaryScope::Session,
            "%7",
            Some("blocked:1"),
            Guard::ProtectHook,
        );
        assert_eq!(
            session.argv.last().unwrap(),
            "#{?#{==:#{@agent_source},hook},#{@agent_session_summary},blocked:1}"
        );
    }

    #[test]
    fn set_pane_option_guarded_wraps_the_companion() {
        let cmd = set_pane_option_guarded(
            "%7",
            opt::NOTIFIED_AT,
            Guard::HookArbitrate { evidence_at: 1500 },
            "1500",
        );
        let v = cmd.argv.last().unwrap();
        assert!(
            v.contains("#{@agent_notified_at}") && v.contains("1500"),
            "companion holds the stored marker when suppressed: {v}"
        );
    }

    #[test]
    fn context_write_guards_pct_and_at_on_evidence_time() {
        // pct written first, at last; both wrap the evidence-time suppress expr so a reordered stale
        // push holds the stored value. The suppress compares the stored @agent_context_at against the
        // incoming evidence time with `e|>` (stored strictly newer ⇒ suppress).
        let cmds = render_context("%7", Some(78), Some(156_000), 1500);
        assert_eq!(cmds.len(), 4);
        assert!(cmds[0].argv.contains(&opt::CONTEXT_PCT.to_string()));
        assert!(cmds[3].argv.contains(&opt::CONTEXT_AT.to_string()));
        let pct = value_for(&cmds, opt::CONTEXT_PCT).unwrap();
        assert_eq!(
            pct,
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_context_pct},78}"
        );
        let at = value_for(&cmds, opt::CONTEXT_AT).unwrap();
        assert_eq!(
            at,
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_context_at},1500}"
        );
        // The absolute count rides the same guard and carries the same evidence time.
        assert_eq!(
            value_for(&cmds, opt::TOKENS).unwrap(),
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_tokens},156000}"
        );
        assert_eq!(
            value_for(&cmds, opt::TOKENS_AT).unwrap(),
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_tokens_at},1500}"
        );
    }

    #[test]
    fn context_null_clear_writes_empty_pct_and_advances_at() {
        // A null observation writes an empty @agent_context_pct (read back as absent) but still
        // advances @agent_context_at under the same guard.
        let cmds = render_context("%7", None, None, 1500);
        assert_eq!(
            value_for(&cmds, opt::CONTEXT_PCT).unwrap(),
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_context_pct},}"
        );
        assert_eq!(
            value_for(&cmds, opt::CONTEXT_AT).unwrap(),
            "#{?#{e|>:#{@agent_context_at},1500},#{@agent_context_at},1500}"
        );
    }

    #[test]
    fn context_without_a_count_clears_the_token_pair() {
        // A percent-only channel (Claude) stamps a gauge and no count: both token options go empty,
        // so a previous count cannot sit beside a fresh gauge, and no evidence time is left behind.
        let cmds = render_context("%7", Some(64), None, 2000);
        assert_eq!(
            value_for(&cmds, opt::TOKENS).unwrap(),
            "#{?#{e|>:#{@agent_context_at},2000},#{@agent_tokens},}"
        );
        assert_eq!(
            value_for(&cmds, opt::TOKENS_AT).unwrap(),
            "#{?#{e|>:#{@agent_context_at},2000},#{@agent_tokens_at},}"
        );
        assert!(
            value_for(&cmds, opt::CONTEXT_PCT)
                .unwrap()
                .ends_with(",64}"),
            "the gauge itself still writes"
        );
    }

    #[test]
    fn context_advisory_is_plain_and_at_last() {
        let set = render_context_advisory("%7", Some(42), Some(84_000), 900);
        for c in &set {
            assert!(
                !c.argv.iter().any(|a| a == "-F"),
                "advisory must not use -F"
            );
        }
        assert_eq!(value_for(&set, opt::CONTEXT_PCT).unwrap(), "42");
        assert_eq!(value_for(&set, opt::TOKENS).unwrap(), "84000");
        assert_eq!(value_for(&set, opt::TOKENS_AT).unwrap(), "900");
        let last = set.last().unwrap();
        assert_eq!(last.argv[last.argv.len() - 2], opt::CONTEXT_AT);
        // A null-clear unsets the pct and both token options (argv ends with the key, `-u`).
        let clear = render_context_advisory("%7", None, None, 900);
        assert!(clear[0].argv.contains(&"-u".to_string()));
        assert_eq!(
            clear[0].argv.last().map(String::as_str),
            Some(opt::CONTEXT_PCT)
        );
        for key in [opt::TOKENS, opt::TOKENS_AT] {
            let cmd = clear
                .iter()
                .find(|c| c.argv.last().map(String::as_str) == Some(key))
                .unwrap_or_else(|| panic!("{key} is unset by the advisory clear"));
            assert!(cmd.argv.contains(&"-u".to_string()));
        }
    }

    #[test]
    fn quota_write_guards_every_field_on_its_own_marker_with_at_last() {
        // Five writes, `@agent_quota_at` last, each wrapping the quota chain's OWN suppress expr,
        // it must compare against `@agent_quota_at`, never the context chain's marker, or a quiet
        // gauge would gate a fresh quota push.
        let q = QuotaStamp {
            pct: Some(63),
            window: Some("spend"),
            resets_at_ms: Some(1_790_787_200_000),
            cost_usd: Some("3.50"),
        };
        let cmds = render_quota("%7", &q, 1500);
        assert_eq!(cmds.len(), 5);
        assert!(cmds[0].argv.contains(&opt::QUOTA_PCT.to_string()));
        assert!(cmds[4].argv.contains(&opt::QUOTA_AT.to_string()));
        assert_eq!(
            value_for(&cmds, opt::QUOTA_PCT).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},1500},#{@agent_quota_pct},63}"
        );
        assert_eq!(
            value_for(&cmds, opt::QUOTA_WINDOW).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},1500},#{@agent_quota_window},spend}"
        );
        assert_eq!(
            value_for(&cmds, opt::QUOTA_RESETS_AT).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},1500},#{@agent_quota_resets_at},1790787200000}"
        );
        assert_eq!(
            value_for(&cmds, opt::COST_USD).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},1500},#{@agent_cost_usd},3.50}"
        );
        assert_eq!(
            value_for(&cmds, opt::QUOTA_AT).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},1500},#{@agent_quota_at},1500}"
        );
    }

    #[test]
    fn a_quota_field_the_payload_omitted_is_cleared_not_left_standing() {
        // Codex reports a quota and no cost; a reset the channel did not state is likewise empty.
        // Both write empty (read back as absent) rather than leaving the previous payload's values
        // beside a fresh reading, and the marker advances either way.
        let q = QuotaStamp {
            pct: Some(18),
            window: Some("primary"),
            ..QuotaStamp::default()
        };
        let cmds = render_quota("%7", &q, 2000);
        for key in [opt::QUOTA_RESETS_AT, opt::COST_USD] {
            assert!(
                value_for(&cmds, key).unwrap().ends_with(",}"),
                "{key} clears when the payload carried none"
            );
        }
        assert!(value_for(&cmds, opt::QUOTA_PCT).unwrap().ends_with(",18}"));
        assert_eq!(
            value_for(&cmds, opt::QUOTA_AT).unwrap(),
            "#{?#{e|>:#{@agent_quota_at},2000},#{@agent_quota_at},2000}"
        );
    }

    #[test]
    fn quota_advisory_is_plain_and_at_last() {
        let q = QuotaStamp {
            pct: Some(91),
            window: Some("5h"),
            resets_at_ms: Some(1_788_425_600_000),
            cost_usd: Some("0.00"),
        };
        let set = render_quota_advisory("%7", &q, 900);
        for c in &set {
            assert!(
                !c.argv.iter().any(|a| a == "-F"),
                "advisory must not use -F"
            );
        }
        assert_eq!(value_for(&set, opt::QUOTA_PCT).unwrap(), "91");
        assert_eq!(value_for(&set, opt::QUOTA_WINDOW).unwrap(), "5h");
        assert_eq!(value_for(&set, opt::COST_USD).unwrap(), "0.00");
        let last = set.last().unwrap();
        assert_eq!(last.argv[last.argv.len() - 2], opt::QUOTA_AT);
        // An observation with nothing to report unsets each option (argv ends with the key, `-u`).
        let clear = render_quota_advisory("%7", &QuotaStamp::default(), 900);
        for key in [
            opt::QUOTA_PCT,
            opt::QUOTA_WINDOW,
            opt::QUOTA_RESETS_AT,
            opt::COST_USD,
        ] {
            let cmd = clear
                .iter()
                .find(|c| c.argv.last().map(String::as_str) == Some(key))
                .unwrap_or_else(|| panic!("{key} is unset by the advisory clear"));
            assert!(cmd.argv.contains(&"-u".to_string()));
        }
    }

    /// A deregister must take the quota lane with it: a pane that outlives its agent carrying a
    /// stale `@agent_quota_pct` would show a fleet-wide gauge for an account nothing is signed into.
    #[test]
    fn remove_clears_the_quota_lane() {
        let keys: Vec<&str> = render_remove("%7")
            .iter()
            .filter_map(|c| c.argv.last().map(String::as_str))
            .map(|k| REMOVABLE.iter().find(|r| **r == k).copied().unwrap_or(""))
            .collect();
        for key in [
            opt::QUOTA_PCT,
            opt::QUOTA_WINDOW,
            opt::QUOTA_RESETS_AT,
            opt::QUOTA_AT,
            opt::COST_USD,
        ] {
            assert!(keys.contains(&key), "{key} is removed on deregister");
        }
    }

    #[test]
    fn summary_grammar_fixed_order_zeros_omitted() {
        use AgentState::*;
        assert_eq!(
            summary_string(&[Blocked, Working, Working, Idle]).as_deref(),
            Some("blocked:1 working:2 idle:1")
        );
        assert_eq!(
            summary_string(&[Idle, Unknown]).as_deref(),
            Some("idle:1 unknown:1")
        );
        assert_eq!(summary_string(&[]), None);
        assert_eq!(summary_string(&[Working]).as_deref(), Some("working:1"));
    }

    #[test]
    fn summary_command_sets_and_unsets_window_option() {
        let set = render_summary(SummaryScope::Window, "%7", Some("blocked:1"));
        assert_eq!(
            argv_of(&set),
            [
                "set-option",
                "-w",
                "-t",
                "%7",
                "@agent_summary",
                "blocked:1"
            ]
        );
        let unset = render_summary(SummaryScope::Window, "%7", None);
        assert_eq!(
            argv_of(&unset),
            ["set-option", "-w", "-u", "-t", "%7", "@agent_summary"]
        );
    }

    /// The session mirror writes the same grammar to its own key with no scope flag: tmux resolves
    /// the containing session from any pane target, and no flag means the session scope.
    #[test]
    fn session_summary_command_targets_the_session_scope() {
        let set = render_summary(SummaryScope::Session, "%7", Some("blocked:1 idle:2"));
        assert_eq!(
            argv_of(&set),
            [
                "set-option",
                "-t",
                "%7",
                "@agent_session_summary",
                "blocked:1 idle:2"
            ]
        );
        let unset = render_summary(SummaryScope::Session, "%7", None);
        assert_eq!(
            argv_of(&unset),
            ["set-option", "-u", "-t", "%7", "@agent_session_summary"]
        );
    }
}
