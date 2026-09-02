//! Pure telemetry-metric parsers: bytes in, metric out, no I/O. A `[telemetry.context]`
//! channel's `format` id selects one of these; the intake (`tma event context`, a `tma-runtime` edge)
//! applies the ownership and evidence-time guards around the value this returns.
//!
//! Parsers are trusted core code, not user configuration: a new vendor format needs a function here
//! and a fixture, the same discipline as screen rules. A malformed payload never errors — it yields
//! `None` (ignore) — because the push path is fire-and-forget and must never break the agent.

/// A parsed context observation. `pct = None` is a null-clear (the channel reported no window, e.g.
/// Claude right after `/compact` or early in a session): the intake unsets the gauge under the
/// evidence-time guard. `session` is the owning session id the intake filters on before stamping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextReport {
    pub session: Option<String>,
    pub pct: Option<u8>,
    /// Absolute tokens **currently in the context window**, `None` when the channel does not report a
    /// count this parser can call a footprint. That is a per-channel fact, not a fallback: pi and
    /// Cursor publish the same number their own percent is computed from, Claude publishes one only
    /// from the release that fixed its cumulative-fields bug (see [`parse_claude_statusline`]), and
    /// Codex's `total_token_usage` mixes a footprint input term with a session-cumulative output one
    /// (see [`parse_codex_rollout`]). A `None` here clears the stored count rather than leaving a
    /// stale one beside a fresh gauge.
    pub tokens: Option<u64>,
}

/// Parse a context payload for `format`. `Some(report)` stamps (`pct = None` clears); `None` is a
/// deliberate ignore — a payload with no compiled-in parser, or one this parser rejects as garbage
/// (see [`parse_claude_statusline`]). There is no error path: the fire-and-forget push must never fail.
pub fn parse_context(format: &str, payload: &str) -> Option<ContextReport> {
    match format {
        "claude-statusline-json" => parse_claude_statusline(payload),
        "codex-rollout-jsonl" => parse_codex_rollout(payload),
        "pi-context-json" => parse_pi_context(payload),
        "cursor-statusline-json" => parse_cursor_statusline(payload),
        // Future formats land with their own parser; an unknown format is a silent ignore here (the
        // intake stamps nothing) rather than a stamp of the wrong shape.
        _ => None,
    }
}

/// The Claude Code statusline payload: a per-turn JSON object carrying a `context_window`
/// object with a precomputed `used_percentage`, plus the owning `session_id`. Semantics:
/// - `used_percentage` in `0..=100` ⇒ stamp that percent;
/// - `context_window` null, or `used_percentage` absent/null ⇒ a null-clear (`pct = None`);
/// - `used_percentage` outside `0..=100` ⇒ ignore the payload entirely (`None`). This is the
///   pre-v2.1.132 cumulative-fields bug (#13783): the value is garbage, so it must not clear either,
///   or a reordered duplicate of a real pre-bug push could resurrect a stale gauge.
///
/// `total_input_tokens` (in the same `context_window` object) is stamped as the count, but only
/// when the payload's own `version` is at least 2.1.132. That is the release that fixed the
/// cumulative-fields bug, which corrupted the count and the percent together — the cumulative
/// fixture reads `used_percentage: 247` beside `total_input_tokens: 494000`. The out-of-range gate
/// above catches a corrupt payload only once the drift is large enough to show; early in a buggy
/// session the percent still reads plausible while the count is already wrong, so the version is the
/// only gate that separates them. Below it (or with no parsable version) the count is absent, which
/// clears any stored one rather than leaving a stale number beside a fresh gauge.
pub fn parse_claude_statusline(payload: &str) -> Option<ContextReport> {
    let session = find_string(payload, "session_id");
    let tokens = claude_token_count(payload);
    // Anchored inside `context_window`: `rate_limits.{five_hour,seven_day,spend_limit}` each carry
    // their own `used_percentage`, so an unanchored read stamps an account-quota percent as the
    // context gauge whenever the payload happens to order `rate_limits` first.
    let window = find_object(payload, "context_window");
    match window.and_then(|w| find_number(w, "used_percentage")) {
        Some(n) => {
            if !(0.0..=100.0).contains(&n) {
                return None; // cumulative-shape garbage: ignore, do not clear
            }
            Some(ContextReport {
                session,
                pct: Some(n.round() as u8),
                tokens,
            })
        }
        None => Some(ContextReport {
            session,
            pct: None,
            tokens,
        }),
    }
}

/// The Claude release that fixed the cumulative-fields bug (#13783); older payloads report a
/// corrupt `total_input_tokens`.
const CLAUDE_TOKEN_COUNT_MIN_VERSION: (u32, u32, u32) = (2, 1, 132);

/// The context footprint from a Claude statusline payload, gated on the payload's `version`.
/// Anchored inside `context_window` so a subagent payload's `tokenCount` sibling cannot stand in.
fn claude_token_count(payload: &str) -> Option<u64> {
    let version = semver_triple(&find_string(payload, "version")?)?;
    if version < CLAUDE_TOKEN_COUNT_MIN_VERSION {
        return None;
    }
    count(find_number(
        find_object(payload, "context_window")?,
        "total_input_tokens",
    ))
}

/// A `major.minor.patch` version string as a comparable triple. `None` unless all three components
/// are present and numeric; a build/prerelease suffix on the patch (`2.1.132-rc.1`) is ignored, and
/// anything else fails closed on the caller's side.
fn semver_triple(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?;
    let digits = patch
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| &patch[..i])
        .unwrap_or(patch);
    Some((major, minor, digits.parse().ok()?))
}

/// The Codex rollout JSONL payload: the bounded tail of `~/.codex/sessions/…/rollout-*.jsonl`,
/// one JSON object per line. A `token_count` event carries `info.total_token_usage` and
/// `info.model_context_window`, so the percentage is computable with no model table. Semantics:
/// - the **newest** `token_count` record in the window wins (scan every line, keep the last);
/// - `pct = round(total_token_usage.total_tokens / model_context_window * 100)`, clamped `0..=100`;
/// - a line that is not a `token_count` record, or one missing either field (a truncated tail line,
///   or a `model_context_window` of 0), is ignored — a malformed line never errors the tail;
/// - no `token_count` record at all ⇒ `None` (no observation, not a clear): Codex has no null-clear,
///   so an absent record leaves the stored gauge untouched (the reader scans further back).
///
/// The field choice (`total_token_usage.total_tokens` over `model_context_window`) follows the
/// record's own field names. **No token count is reported** (`tokens` is always `None`): the record's two terms
/// do not agree on what they measure. Across the fixture's records `total_token_usage.input_tokens`
/// equals `last_token_usage.input_tokens` exactly (88010, then 151234) — the per-request context
/// sent, a footprint — while `output_tokens` climbs past the last turn's (2140 against 1044, then
/// 4820 against 1204), a session-cumulative counter. Their sum is a hybrid no single label fits, so
/// the percent (which the footprint term dominates) ships and the absolute stays unstamped rather
/// than stamped under a name that would be wrong half the time. Live verification of `token_count`
/// would settle it (ACTIONS.md open question 6); until then the gauge is the honest half.
pub fn parse_codex_rollout(payload: &str) -> Option<ContextReport> {
    let mut pct: Option<u8> = None;
    for line in payload.lines() {
        if let Some(p) = codex_line_pct(line) {
            pct = Some(p);
        }
    }
    pct.map(|p| ContextReport {
        session: None, // discovery already keyed the file to the owning pane; ownership is implicit
        pct: Some(p),
        tokens: None,
    })
}

/// The context percent from one Codex rollout line, `None` when the line is not a computable
/// `token_count` record. Anchors `total_tokens` inside `total_token_usage` so the sibling
/// `last_token_usage.total_tokens` cannot be read by mistake.
fn codex_line_pct(line: &str) -> Option<u8> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let window = find_number(line, "model_context_window")?;
    if window <= 0.0 {
        return None;
    }
    let anchor = line.find("\"total_token_usage\"")?;
    let used = find_number(&line[anchor..], "total_tokens")?;
    if used < 0.0 {
        return None;
    }
    Some((used / window * 100.0).round().clamp(0.0, 100.0) as u8)
}

/// The pi context payload: the pi extension forwards `ctx.getContextUsage()` on the
/// turn-settled `agent_settled` event as `{ "session_id", "context_usage": { tokens, contextWindow,
/// percent } | null }`. pi's `ContextUsage` (earendil-works/pi, verified 2026-07-29) always carries a
/// precomputed `percent` alongside an absolute `contextWindow`, with `tokens`/`percent` `null` right
/// after a `/compact` (until the next assistant response) and the whole object omitted when no
/// model/window is available. Semantics:
/// - a finite `percent` ⇒ stamp `round(percent)` clamped `0..=100` (pi computed it against its own
///   window, so no `[telemetry.windows]` lookup is needed and none is guessed);
/// - no `percent` but a finite `tokens` and a `contextWindow > 0` ⇒ compute from them — a defensive
///   fallback should a future pi build ever emit tokens without the precomputed percent;
/// - anything else — usage omitted/`null`, a post-`/compact` `null` reading, or a `tokens` with no
///   usable window — ⇒ `None`: no observation, NOT a clear. pi has no genuine null-clear like Claude's
///   post-`/compact` `context_window: null`, and the no-silent-window rule forbids guessing a window.
///
/// `tokens` is pi's own `tokens` field whenever it is a usable count: it is the number pi's
/// `percent` is computed against, so it is the context footprint by construction.
pub fn parse_pi_context(payload: &str) -> Option<ContextReport> {
    let session = find_string(payload, "session_id");
    let tokens = count(find_number(payload, "tokens"));
    if let Some(percent) = find_number(payload, "percent") {
        return Some(ContextReport {
            session,
            pct: Some(percent.round().clamp(0.0, 100.0) as u8),
            tokens,
        });
    }
    // No precomputed percent: compute only when both raw fields are usable. An absent or zero window
    // yields no stamp rather than a silent 200k guess (the acceptance's fail-safe case).
    let used = find_number(payload, "tokens")?;
    let window = find_number(payload, "contextWindow")?;
    if used < 0.0 || window <= 0.0 {
        return None;
    }
    Some(ContextReport {
        session,
        pct: Some((used / window * 100.0).round().clamp(0.0, 100.0) as u8),
        tokens,
    })
}

/// The Cursor CLI statusline payload: Cursor's `~/.cursor/cli-config.json`
/// `statusLine` command runs an external script per turn with a JSON object on stdin carrying a
/// `context_window` object with `total_input_tokens` and `context_window_size` (confirmed live
/// 2026-07-29). The mechanism works but is ABSENT from Cursor's documented config reference, so it is
/// the highest-churn channel of the batch: the parser reads only the two confirmed numeric fields and
/// fails safe on anything else. Semantics:
/// - both `total_input_tokens` and a positive `context_window_size` ⇒ stamp
///   `round(total_input_tokens / context_window_size * 100)`, clamped `0..=100` (window from the
///   payload's own figure, so no `[telemetry.windows]` lookup and no guess);
/// - `context_window` absent, either field missing/non-numeric, or `context_window_size <= 0` ⇒
///   `None` — IGNORE, not a clear. Unlike Claude's post-`/compact` `context_window: null` (a
///   deliberate reset signal tma clears on), Cursor documents no null-clear, so a missing field on
///   this undocumented channel is most likely a payload-shape change; clearing would walk a live
///   gauge to absent on every churn event (the wrong-gauge-worse-than-none inversion). The stored
///   value stays and surfaces grey it via `context_at`, degrading to absent only as it ages.
///
/// `session_id` is read best-effort for the ownership guard (Cursor's hook envelope carries it); an
/// absent one leaves ownership implicit, which the single-pane statusline context makes safe.
///
/// `tokens` is `total_input_tokens`, the same number the percent above divides: a context footprint,
/// not session spend. Claude's identically-named field in the identically-named object is the
/// cross-check — its payload carries `used_percentage: 78` beside `total_input_tokens: 156000` over
/// a 200000 window, so where a vendor publishes both, the count is what the percent is made of.
pub fn parse_cursor_statusline(payload: &str) -> Option<ContextReport> {
    let session = find_string(payload, "session_id");
    let window = find_number(payload, "context_window_size")?;
    if window <= 0.0 {
        return None;
    }
    let used = find_number(payload, "total_input_tokens")?;
    if used < 0.0 {
        return None;
    }
    Some(ContextReport {
        session,
        pct: Some((used / window * 100.0).round().clamp(0.0, 100.0) as u8),
        tokens: count(Some(used)),
    })
}

/// A parsed JSON number as a token count: `None` unless it is finite and non-negative. Truncates the
/// fraction (a count is whole; no vendor sends one fractional, and rounding would invent precision).
fn count(n: Option<f64>) -> Option<u64> {
    n.filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as u64)
}

// ---- quota and cost ----------------------------------------------------------------------------

/// One account rate-limit window a channel reports. Unlike the context gauge, which is per-pane and
/// recoverable by a compact, these are account-wide: every pane signed into the same account shares
/// them, so the same numbers land on several rows at once and that is correct, not a duplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaWindow {
    /// Claude `rate_limits.five_hour`: the rolling five-hour window.
    FiveHour,
    /// Claude `rate_limits.seven_day`: the weekly window.
    SevenDay,
    /// Claude `rate_limits.spend_limit`: a Claude-apps-gateway spend cap, when one applies.
    Spend,
    /// Codex `rate_limits.primary`: the shorter of the two windows the rollout reports.
    Primary,
    /// Codex `rate_limits.secondary`: the longer one, absent (`null`) on many plans.
    Secondary,
}

impl QuotaWindow {
    /// The machine token stamped into `@agent_quota_window`, as with every other option value.
    pub fn token(self) -> &'static str {
        match self {
            QuotaWindow::FiveHour => "5h",
            QuotaWindow::SevenDay => "7d",
            QuotaWindow::Spend => "spend",
            QuotaWindow::Primary => "primary",
            QuotaWindow::Secondary => "secondary",
        }
    }
}

/// One window's reading: its utilization percent and, when the channel states it, the instant it
/// resets. `resets_at_ms` is epoch **milliseconds** — the parser converts, never the consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaWindowReading {
    pub window: QuotaWindow,
    pub pct: u8,
    pub resets_at_ms: Option<u64>,
}

/// A parsed quota observation: the window closest to exhausted, plus every window the payload
/// carried. **Highest percent wins** because that is the one that will stop the account first; a tie
/// goes to the window listed first, which is the shorter (and so more urgent) one on both channels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaReport {
    pub pct: u8,
    pub window: QuotaWindow,
    pub resets_at_ms: Option<u64>,
    /// Every window present, in the channel's own declaration order. Kept because the parse already
    /// walked them, so a surface that wants "5h and 7d side by side" needs no second parse.
    pub windows: Vec<QuotaWindowReading>,
}

impl QuotaReport {
    /// The report for a set of window readings: the highest percent wins. `None` for an empty set —
    /// no window parsed is no observation, never a zero.
    fn worst(windows: Vec<QuotaWindowReading>) -> Option<QuotaReport> {
        // `min_by_key` over the reversed percent, not `max_by_key`: `max_by_key` keeps the LAST of
        // several equal maxima, and a tie belongs to the window declared first (the shorter one).
        let worst = *windows.iter().min_by_key(|w| std::cmp::Reverse(w.pct))?;
        Some(QuotaReport {
            pct: worst.pct,
            window: worst.window,
            resets_at_ms: worst.resets_at_ms,
            windows,
        })
    }
}

/// A parsed quota/cost/model observation from the very payload the context parsers already read.
/// Every field is independently optional: a channel reports what it reports, and an absent field is
/// never inferred from a present one.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageReport {
    /// The owning session id, for the intake's ownership filter (same field the context path reads).
    pub session: Option<String>,
    pub quota: Option<QuotaReport>,
    /// The vendor's own session cost in USD, `None` when the channel publishes none. A live reading
    /// of one session, never a total tma computed and never a price table.
    pub cost_usd: Option<f64>,
    /// The model id, for channels whose payload carries one the registration path cannot reach.
    pub model: Option<String>,
}

impl UsageReport {
    /// Whether this observation has anything for the quota chain to write. A payload carrying only a
    /// model name stamps the label and leaves the quota lane alone.
    pub fn has_quota_observation(&self) -> bool {
        self.quota.is_some() || self.cost_usd.is_some()
    }
}

/// Parse the quota/cost half of a context payload for `format`. `now_ms` is the caller's clock: a
/// channel that reports a RELATIVE reset offset (older Codex builds) has no other way to reach an
/// absolute instant, and this module holds no clock of its own.
///
/// `None` is a deliberate ignore — an unknown format, or a payload carrying no quota, no cost and no
/// model. As on the context path there is no error case: a fire-and-forget push must never fail.
pub fn parse_usage(format: &str, payload: &str, now_ms: u64) -> Option<UsageReport> {
    let report = match format {
        "claude-statusline-json" => UsageReport {
            session: find_string(payload, "session_id"),
            quota: claude_quota(payload),
            cost_usd: claude_cost_usd(payload),
            model: claude_statusline_model(payload),
        },
        "codex-rollout-jsonl" => UsageReport {
            session: None, // discovery already keyed the file to the pane, as on the context path
            quota: codex_rollout_quota(payload, now_ms),
            cost_usd: None, // the rollout carries no cost figure
            model: None,    // `codex_rollout_model` already covers the tail's model record
        },
        _ => return None,
    };
    let anything = report.has_quota_observation() || report.model.is_some();
    anything.then_some(report)
}

/// Claude's rate-limit windows, shortest first (which decides a tie in [`QuotaReport::worst`]).
const CLAUDE_WINDOWS: [(&str, QuotaWindow); 3] = [
    ("five_hour", QuotaWindow::FiveHour),
    ("seven_day", QuotaWindow::SevenDay),
    ("spend_limit", QuotaWindow::Spend),
];

/// The quota half of a Claude statusline payload: `rate_limits.{five_hour,seven_day,spend_limit}`,
/// each `{ used_percentage, resets_at }` with `resets_at` in epoch **seconds**.
///
/// Every read is anchored inside its own window object. `used_percentage` is a key the payload
/// carries in four different objects (the three windows and `context_window`), so an unanchored read
/// is not a shortcut, it is a wrong answer waiting for a field-order change.
///
/// `None` — no `rate_limits` object, or no window inside it that parses — is an IGNORE, never a
/// clear. The block is absent for API-key auth, absent before the first API response, and dropped
/// per window once its `resets_at` passes, so a missing block says nothing about the account.
fn claude_quota(payload: &str) -> Option<QuotaReport> {
    let block = find_object(payload, "rate_limits")?;
    QuotaReport::worst(
        CLAUDE_WINDOWS
            .iter()
            .filter_map(|&(key, window)| {
                let obj = find_object(block, key)?;
                Some(QuotaWindowReading {
                    window,
                    pct: quota_pct(find_number(obj, "used_percentage")?)?,
                    resets_at_ms: epoch_seconds_to_ms(find_number(obj, "resets_at")),
                })
            })
            .collect(),
    )
}

/// The session cost from a Claude statusline payload, anchored inside its own `cost` object so no
/// future sibling of the same name can stand in. `None` when absent or not a usable amount.
fn claude_cost_usd(payload: &str) -> Option<f64> {
    let v = find_number(find_object(payload, "cost")?, "total_cost_usd")?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// The model id from a Claude **statusline** payload, whose `model` is an OBJECT
/// (`{ "id", "display_name" }`) and so out of reach of [`hook_payload_model`]'s top-level string
/// read. `id` over `display_name`: the id is what a `[telemetry.windows]` entry names.
pub fn claude_statusline_model(payload: &str) -> Option<String> {
    let id = find_string(find_object(payload, "model")?, "id")?;
    let id = id.trim();
    // A model name is a short safe label; reject empty/oversized junk before it reaches a pane option.
    (!id.is_empty() && id.len() <= 64).then(|| id.to_string())
}

/// Codex's rate-limit windows, shortest first (a tie goes to `primary`, whose `window_minutes` is
/// the smaller of the two on every observed record).
const CODEX_WINDOWS: [(&str, QuotaWindow); 2] = [
    ("primary", QuotaWindow::Primary),
    ("secondary", QuotaWindow::Secondary),
];

/// The quota half of a Codex rollout tail: the newest `token_count` record's `rate_limits`, matching
/// the context parser's newest-record-wins rule so gauge and quota describe the same instant. A
/// record with no `rate_limits` leaves the previous record's reading standing within the window.
fn codex_rollout_quota(payload: &str, now_ms: u64) -> Option<QuotaReport> {
    let mut latest = None;
    for line in payload.lines() {
        if let Some(q) = codex_line_quota(line, now_ms) {
            latest = Some(q);
        }
    }
    latest
}

/// One rollout line's quota reading. `rate_limits` sits beside `info` on the `token_count` payload,
/// carrying `primary`/`secondary` as `{ used_percent, window_minutes, resets_at | resets_in_seconds }`.
/// A `"secondary": null` is not an object, so [`find_object`] skips it rather than reading through
/// it into the next window's fields.
fn codex_line_quota(line: &str, now_ms: u64) -> Option<QuotaReport> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let block = find_object(line, "rate_limits")?;
    QuotaReport::worst(
        CODEX_WINDOWS
            .iter()
            .filter_map(|&(key, window)| {
                let obj = find_object(block, key)?;
                Some(QuotaWindowReading {
                    window,
                    pct: quota_pct(find_number(obj, "used_percent")?)?,
                    resets_at_ms: codex_resets_at_ms(obj, now_ms),
                })
            })
            .collect(),
    )
}

/// One Codex window's reset instant in epoch **ms**. Codex has published it two ways across
/// releases: an absolute `resets_at` in epoch seconds (every rollout observed on this machine,
/// 2026-09), and a relative `resets_in_seconds` offset that only the caller's clock can resolve.
/// Absolute wins where both appear — it needs no clock and so cannot drift.
fn codex_resets_at_ms(obj: &str, now_ms: u64) -> Option<u64> {
    if let Some(ms) = epoch_seconds_to_ms(find_number(obj, "resets_at")) {
        return Some(ms);
    }
    let secs = find_number(obj, "resets_in_seconds")?;
    // A window that resets more than a year out is not a window; reject rather than stamp nonsense.
    if !secs.is_finite() || !(0.0..=31_536_000.0).contains(&secs) {
        return None;
    }
    Some(now_ms.saturating_add((secs * 1000.0) as u64))
}

/// A reported utilization percent as the stamped `0..=100` integer. `None` for a non-finite or
/// negative reading; a spend limit past 100% (which Claude documents) clamps to a full gauge rather
/// than overflowing the option's documented range.
fn quota_pct(n: f64) -> Option<u8> {
    (n.is_finite() && n >= 0.0).then(|| n.round().clamp(0.0, 100.0) as u8)
}

/// An epoch-**seconds** timestamp as epoch **ms**. `None` unless it is a plausible wall-clock
/// instant: `1e11` seconds is the year 5138, past which a value is a unit mix-up or garbage, and a
/// non-positive one is a field the channel had nothing to put in.
fn epoch_seconds_to_ms(secs: Option<f64>) -> Option<u64> {
    let s = secs?;
    (s.is_finite() && s > 0.0 && s <= 1e11).then_some((s * 1000.0) as u64)
}

/// A cost reading as the `@agent_cost_usd` option value: two decimals, the form every surface
/// renders. `None` for an amount no money label fits (non-finite or negative).
pub fn format_cost_usd(v: f64) -> Option<String> {
    (v.is_finite() && v >= 0.0).then(|| format!("{v:.2}"))
}

/// Best-effort model name from a Codex rollout tail window, for `tma doctor`'s recognized-model check:
/// the newest `"model":"…"` in the window (a `turn_context`/`session_meta` record). `None` when the
/// window carries none — the model record can sit before the tail window on a large file, so this is
/// advisory only (an absent model simply yields no model line), never load-bearing.
pub fn codex_rollout_model(payload: &str) -> Option<String> {
    let mut latest = None;
    for line in payload.lines() {
        if let Some(m) = find_string(line, "model") {
            let m = m.trim();
            // A model name is a short safe label; reject empty/oversized junk before it reaches a
            // pane option, and `model_context_window`'s numeric value never matches the `"model"` key.
            if !m.is_empty() && m.len() <= 64 {
                latest = Some(m.to_string());
            }
        }
    }
    latest
}

/// Best-effort model name from a hook **registration** payload: the top-level `"model"`
/// string that Claude's `SessionStart`, Codex's session hooks, and Cursor's `sessionStart` all carry.
/// `None` when the field is absent (Claude omits it after `/clear`/restore; Gemini/OpenCode/pi never
/// send it), when it is not a plain string (Claude's *statusline* JSON nests `model` as an object,
/// but that payload never reaches this path), or when it fails the safe-label bound. Feeds
/// `@agent_model` on registration and the `[telemetry.windows]` name check, coexisting with the
/// Codex rollout tail's identical stamp.
pub fn hook_payload_model(payload: &str) -> Option<String> {
    let m = find_string(payload, "model")?;
    let m = m.trim();
    // A model name is a short safe label; reject empty/oversized junk before it reaches a pane option.
    (!m.is_empty() && m.len() <= 64).then(|| m.to_string())
}

/// Extract a top-level JSON string field `"<key>": "<value>"` (first occurrence), decoding the common
/// escapes. Dependency-free, the same discipline as the event bridge's `session_id` reader; the fields
/// read here are unambiguous in the captured payloads.
fn find_string(payload: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = payload[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => return None,
            },
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// The `{…}` slice of the object at `"<key>":` (first occurrence), brace-balanced and quote-aware so
/// a nested object or a brace inside a string cannot end it early. `None` when the key is absent or
/// its value is not an object — `null`, a number, a string — which is what makes it a safe anchor:
/// reading a field "inside" `"context_window": null` would otherwise spill into the next object.
///
/// This is the whole reason the parsers below are not one `find_number` each. `used_percentage`
/// appears in four objects of a Claude payload and `total_tokens` in two of a Codex record; an
/// unanchored read picks whichever the vendor happened to serialize first.
fn find_object<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = payload[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in rest.bytes().enumerate() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..=i]);
                }
            }
            _ => {}
        }
    }
    None // unterminated: a truncated payload, which every caller treats as no reading
}

/// Extract a JSON number field `"<key>": <number>` (first occurrence). `None` when the key is absent
/// or its value is `null` (the two collapse to the same "no reading" outcome for the caller).
fn find_number(payload: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = payload[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(rest.len());
    let token = &rest[..end];
    if token.is_empty() {
        return None; // e.g. `null`
    }
    token.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // REAL-shaped Claude statusline captures (constructed from the verified statusline schema, paths
    // redacted): a normal reading, a null `context_window`, a pre-fix cumulative-shape payload, and a
    // subagent-shaped payload (its own session id, filtered by ownership at the intake, not here).
    const NORMAL: &str = include_str!("../fixtures/claude_statusline_context.json");
    const NULL_CONTEXT: &str = include_str!("../fixtures/claude_statusline_null_context.json");
    const CUMULATIVE: &str = include_str!("../fixtures/claude_statusline_cumulative.json");
    const SUBAGENT: &str = include_str!("../fixtures/claude_statusline_subagent.json");
    /// A pre-2.1.132 payload whose drift has not yet shown: the percent reads plausible while
    /// `total_input_tokens` is already the corrupt cumulative field.
    const PRE_FIX: &str = include_str!("../fixtures/claude_statusline_pre_fix_version.json");

    #[test]
    fn parses_used_percentage_and_session() {
        let r = parse_claude_statusline(NORMAL).unwrap();
        assert_eq!(r.pct, Some(78));
        assert_eq!(
            r.session.as_deref(),
            Some("019f8aac-ff01-75d0-9bb1-7f0eab253ce7")
        );
    }

    #[test]
    fn null_context_window_is_a_clear() {
        let r = parse_claude_statusline(NULL_CONTEXT).unwrap();
        assert_eq!(r.pct, None, "a null context_window clears the gauge");
        assert!(r.session.is_some());
    }

    #[test]
    fn cumulative_shape_payload_is_ignored() {
        // A pre-v2.1.132 cumulative reading (>100) is garbage: ignore it (no stamp, no clear).
        assert_eq!(parse_claude_statusline(CUMULATIVE), None);
    }

    #[test]
    fn subagent_payload_parses_with_its_own_session() {
        // The parser still reads it; ownership filtering (the intake) is what drops it, keyed on this
        // foreign session id differing from the pane's `@agent_session`.
        let r = parse_claude_statusline(SUBAGENT).unwrap();
        assert_eq!(r.pct, Some(15));
        assert_eq!(
            r.session.as_deref(),
            Some("0b03d2a0-d44c-4c51-8de3-57f2c043e737")
        );
    }

    #[test]
    fn the_claude_token_count_is_gated_on_the_payload_version() {
        // 2.1.212: past the fix, so the count rides alongside the gauge.
        let fixed = parse_claude_statusline(NORMAL).unwrap();
        assert_eq!(fixed.pct, Some(78));
        assert_eq!(fixed.tokens, Some(156_000));

        // 2.1.131: the percent is in range and stamps, but the count is the corrupt field, so it
        // stays absent (and clears any stored one).
        let pre = parse_claude_statusline(PRE_FIX).unwrap();
        assert_eq!(pre.pct, Some(41), "the gauge is unaffected by the gate");
        assert_eq!(pre.tokens, None);

        // The boundary itself: 2.1.132 is the fix, so it counts as fixed.
        let at = |v: &str| {
            let p = format!(
                r#"{{"version":"{v}","context_window":{{"used_percentage":10,"total_input_tokens":20000,"context_window_size":200000}}}}"#
            );
            parse_claude_statusline(&p).unwrap().tokens
        };
        assert_eq!(at("2.1.132"), Some(20_000));
        assert_eq!(at("2.1.131"), None);
        assert_eq!(at("2.2.0"), Some(20_000));
        assert_eq!(at("3.0.0"), Some(20_000));
        assert_eq!(at("2.0.999"), None);
        // A prerelease of the fix release still carries the fix.
        assert_eq!(at("2.1.132-rc.1"), Some(20_000));
        // An unparsable or absent version fails closed.
        assert_eq!(at("nightly"), None);
        assert_eq!(at("2.1"), None);
        assert_eq!(
            parse_claude_statusline(
                r#"{"context_window":{"used_percentage":10,"total_input_tokens":20000}}"#
            )
            .unwrap()
            .tokens,
            None
        );
    }

    #[test]
    fn a_subagent_token_count_is_never_read_as_the_pane_footprint() {
        // The subagent payload carries `subagentStatusLine.tokenCount` beside the real
        // `context_window`; the count must come from the anchored object, not the sibling.
        assert_eq!(
            parse_claude_statusline(SUBAGENT).unwrap().tokens,
            Some(30_000)
        );
    }

    #[test]
    fn rounds_a_fractional_percent() {
        let r = parse_claude_statusline(r#"{"context_window":{"used_percentage":42.7}}"#).unwrap();
        assert_eq!(r.pct, Some(43));
    }

    #[test]
    fn boundary_values_accepted() {
        for (raw, want) in [("0", 0u8), ("100", 100)] {
            let p = format!(r#"{{"context_window":{{"used_percentage":{raw}}}}}"#);
            assert_eq!(parse_claude_statusline(&p).unwrap().pct, Some(want));
        }
    }

    /// The token-count contract, per channel, in one place: a count rides only the payloads whose
    /// absolute is unambiguously the current-context footprint. See each parser's doc for the why.
    #[test]
    fn only_footprint_channels_report_a_token_count() {
        // pi and Cursor publish the number their own percent divides.
        assert_eq!(parse_pi_context(PI_CONTEXT).unwrap().tokens, Some(124_000));
        assert_eq!(
            parse_cursor_statusline(CURSOR_CONTEXT).unwrap().tokens,
            Some(130_000)
        );
        // Claude publishes one from 2.1.132 on; a null window carries no count to publish.
        assert_eq!(
            parse_claude_statusline(NORMAL).unwrap().tokens,
            Some(156_000)
        );
        assert_eq!(parse_claude_statusline(NULL_CONTEXT).unwrap().tokens, None);
        // Codex's absolute is a hybrid, so it stamps none.
        assert_eq!(parse_codex_rollout(CODEX_ROLLOUT).unwrap().tokens, None);
        // The gauges themselves are untouched by any of that.
        assert_eq!(parse_pi_context(PI_CONTEXT).unwrap().pct, Some(62));
        assert_eq!(parse_codex_rollout(CODEX_ROLLOUT).unwrap().pct, Some(57));
    }

    #[test]
    fn a_count_needs_a_usable_number() {
        // pi computing from raw fields still reports the count it computed from.
        let both = r#"{"context_usage":{"tokens":124000,"contextWindow":200000}}"#;
        assert_eq!(parse_pi_context(both).unwrap().tokens, Some(124_000));
        // A null count with a live percent stamps the gauge and clears the count (post-`/compact`
        // pi reports `tokens: null` only once `percent` is back).
        let no_count = r#"{"context_usage":{"contextWindow":200000,"percent":30}}"#;
        let r = parse_pi_context(no_count).unwrap();
        assert_eq!(r.pct, Some(30));
        assert_eq!(r.tokens, None);
        // A negative reading is not a count.
        let negative = r#"{"context_usage":{"tokens":-5,"contextWindow":200000,"percent":0}}"#;
        assert_eq!(parse_pi_context(negative).unwrap().tokens, None);
        // Cursor's fractional count truncates rather than inventing precision.
        let frac =
            r#"{"context_window":{"total_input_tokens":95000.7,"context_window_size":200000}}"#;
        assert_eq!(parse_cursor_statusline(frac).unwrap().tokens, Some(95_000));
    }

    #[test]
    fn unknown_format_is_ignored() {
        assert_eq!(parse_context("carrier-pigeon", NORMAL), None);
        assert_eq!(
            parse_context("claude-statusline-json", NORMAL).unwrap().pct,
            Some(78)
        );
    }

    // REAL-shaped Codex rollout captures (constructed from the verified rollout JSONL shape, paths
    // redacted): a two-`token_count` window, a truncated trailing line, and a window with no record.
    const CODEX_ROLLOUT: &str = include_str!("../fixtures/codex_rollout_token_count.jsonl");
    const CODEX_TRUNCATED: &str = include_str!("../fixtures/codex_rollout_truncated.jsonl");
    const CODEX_NO_RECORD: &str = include_str!("../fixtures/codex_rollout_no_token_count.jsonl");

    #[test]
    fn codex_rollout_takes_the_newest_token_count() {
        // Two records in the window (90150 then 156054 tokens / 272000); the newest wins ⇒ 57%.
        let r = parse_codex_rollout(CODEX_ROLLOUT).unwrap();
        assert_eq!(r.pct, Some(57));
        // The file-tail path is ownership-implicit (discovery keyed the file to the pane).
        assert!(r.session.is_none());
        // Same via the dispatcher.
        assert_eq!(
            parse_context("codex-rollout-jsonl", CODEX_ROLLOUT)
                .unwrap()
                .pct,
            Some(57)
        );
    }

    #[test]
    fn codex_rollout_reads_total_usage_not_last_usage() {
        // The record carries both `total_token_usage.total_tokens` (156054) and
        // `last_token_usage.total_tokens` (152438); the anchor must read the former (57, not 56).
        assert_eq!(parse_codex_rollout(CODEX_ROLLOUT).unwrap().pct, Some(57));
    }

    #[test]
    fn codex_rollout_truncated_trailing_line_falls_back_to_the_last_complete_record() {
        // The trailing line is cut mid-write (no window/total): it parses to nothing, so the last
        // complete record (90150 / 272000 ⇒ 33%) is the newest computable reading.
        assert_eq!(parse_codex_rollout(CODEX_TRUNCATED).unwrap().pct, Some(33));
    }

    #[test]
    fn codex_rollout_with_no_token_count_is_none() {
        // A window of session_meta / turn_context / message lines carries no gauge: no observation
        // (not a clear) — the reader scans further back or leaves the stored value untouched.
        assert_eq!(parse_codex_rollout(CODEX_NO_RECORD), None);
    }

    // REAL-shaped pi context captures (constructed from the verified `ContextUsage` shape —
    // earendil-works/pi `getContextUsage()`, session id redacted): a normal reading, a payload whose
    // `getContextUsage()` returned undefined (`context_usage: null`), and a tokens-with-no-window
    // reading (the no-silent-window fail-safe).
    const PI_CONTEXT: &str = include_str!("../fixtures/pi_context.json");
    const PI_MISSING: &str = include_str!("../fixtures/pi_context_missing.json");
    const PI_UNKNOWN_WINDOW: &str = include_str!("../fixtures/pi_context_unknown_window.json");

    #[test]
    fn pi_context_reads_precomputed_percent_and_session() {
        let r = parse_pi_context(PI_CONTEXT).unwrap();
        assert_eq!(r.pct, Some(62));
        assert_eq!(r.session.as_deref(), Some("ses_0789d5f61ffeW6yCmb3x7wLH1X"));
        // Same via the dispatcher.
        assert_eq!(
            parse_context("pi-context-json", PI_CONTEXT).unwrap().pct,
            Some(62)
        );
    }

    #[test]
    fn pi_context_missing_usage_is_ignored_not_cleared() {
        // `getContextUsage()` returned undefined ⇒ `context_usage: null`: no observation, and NOT a
        // clear (unlike Claude's null `context_window`) — the stored gauge is left untouched.
        assert_eq!(parse_pi_context(PI_MISSING), None);
    }

    #[test]
    fn pi_context_unknown_window_stays_absent() {
        // A raw token count with no usable window: the no-silent-window rule forbids guessing 200k,
        // so the gauge stays absent rather than wrong (the acceptance's fail-safe).
        assert_eq!(parse_pi_context(PI_UNKNOWN_WINDOW), None);
    }

    #[test]
    fn pi_context_post_compaction_null_is_ignored() {
        // Right after `/compact` pi returns a defined usage with null tokens/percent (window known):
        // that is unknown, not zero ⇒ no stamp, no clear.
        let post = r#"{"session_id":"ses_x","context_usage":{"tokens":null,"contextWindow":200000,"percent":null}}"#;
        assert_eq!(parse_pi_context(post), None);
    }

    #[test]
    fn pi_context_computes_from_tokens_when_percent_absent() {
        // Defensive fallback: a build that emits tokens+window but no precomputed percent still resolves.
        let both = r#"{"context_usage":{"tokens":124000,"contextWindow":200000}}"#;
        assert_eq!(parse_pi_context(both).unwrap().pct, Some(62));
        // A zero window is not a silent 200k guess.
        let zero = r#"{"context_usage":{"tokens":124000,"contextWindow":0}}"#;
        assert_eq!(parse_pi_context(zero), None);
    }

    #[test]
    fn pi_context_rounds_and_clamps_percent() {
        let frac = r#"{"context_usage":{"tokens":1,"contextWindow":2,"percent":49.6}}"#;
        assert_eq!(parse_pi_context(frac).unwrap().pct, Some(50));
        // Over-budget (tokens exceed the window) clamps to a full gauge rather than reject.
        let over = r#"{"context_usage":{"tokens":3,"contextWindow":2,"percent":140}}"#;
        assert_eq!(parse_pi_context(over).unwrap().pct, Some(100));
    }

    // REAL-shaped Cursor statusline captures (constructed from the confirmed `context_window` shape —
    // undocumented but verified live 2026-07-29, workspace path redacted): a normal reading, a payload
    // with no `context_window` object, and a malformed one whose token fields are non-numeric.
    const CURSOR_CONTEXT: &str = include_str!("../fixtures/cursor_statusline_context.json");
    const CURSOR_NO_CONTEXT: &str = include_str!("../fixtures/cursor_statusline_no_context.json");
    const CURSOR_MALFORMED: &str = include_str!("../fixtures/cursor_statusline_malformed.json");

    #[test]
    fn cursor_statusline_computes_percent_from_tokens_and_window() {
        // 130000 / 200000 = 65%.
        let r = parse_cursor_statusline(CURSOR_CONTEXT).unwrap();
        assert_eq!(r.pct, Some(65));
        assert_eq!(
            r.session.as_deref(),
            Some("3f1c8d2e-9a44-4b17-9c0e-2b6a1d7e4f88")
        );
        // Same via the dispatcher.
        assert_eq!(
            parse_context("cursor-statusline-json", CURSOR_CONTEXT)
                .unwrap()
                .pct,
            Some(65)
        );
    }

    #[test]
    fn cursor_statusline_missing_context_window_is_ignored_not_cleared() {
        // No `context_window` object: IGNORE (no observation), NOT a clear — Cursor documents no
        // null-clear, so a missing field on this undocumented channel must not erase a live gauge.
        assert_eq!(parse_cursor_statusline(CURSOR_NO_CONTEXT), None);
    }

    #[test]
    fn cursor_statusline_malformed_payload_is_ignored() {
        // Non-numeric token fields (a plausible payload-shape change): no stamp, no clear.
        assert_eq!(parse_cursor_statusline(CURSOR_MALFORMED), None);
        // Total garbage yields the same fail-safe.
        assert_eq!(parse_cursor_statusline("<html>500</html>"), None);
    }

    #[test]
    fn cursor_statusline_rounds_and_clamps() {
        // Fractional: 95000 / 200000 = 47.5 ⇒ 48 (round half up via f64::round).
        let frac =
            r#"{"context_window":{"total_input_tokens":95000,"context_window_size":200000}}"#;
        assert_eq!(parse_cursor_statusline(frac).unwrap().pct, Some(48));
        // Over-budget clamps to a full gauge rather than exceeding 100.
        let over =
            r#"{"context_window":{"total_input_tokens":260000,"context_window_size":200000}}"#;
        assert_eq!(parse_cursor_statusline(over).unwrap().pct, Some(100));
        // A zero window is not a silent guess.
        let zero = r#"{"context_window":{"total_input_tokens":1000,"context_window_size":0}}"#;
        assert_eq!(parse_cursor_statusline(zero), None);
    }

    #[test]
    fn hook_payload_model_reads_a_top_level_string() {
        // The three registration payloads that carry it, each a plain top-level `"model"` string.
        assert_eq!(
            hook_payload_model(r#"{"hook_event_name":"SessionStart","model":"claude-sonnet-5"}"#)
                .as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            hook_payload_model(r#"{"session_id":"x","hook_event_name":"SessionStart","model":"gpt-5.6-terra","source":"startup"}"#).as_deref(),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            hook_payload_model(r#"{"model":"default","hook_event_name":"sessionStart"}"#)
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn hook_payload_model_absent_or_nonstring_is_none() {
        // Absent (Claude after /clear, Gemini/OpenCode/pi registration payloads).
        assert_eq!(hook_payload_model(r#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(hook_payload_model(r#"{"session_id":"ses_abc"}"#), None);
        // The statusline object shape must not be misread as a label (it never reaches this path,
        // but the extractor fails closed on it rather than stamping `{`-garbage).
        assert_eq!(
            hook_payload_model(r#"{"model":{"id":"claude-opus-5","display_name":"Opus"}}"#),
            None
        );
        // `model_context_window` (a numeric sibling key) must not be mistaken for `"model"`.
        assert_eq!(
            hook_payload_model(r#"{"model_context_window":272000}"#),
            None
        );
        // An oversized value is rejected before it can reach a pane option.
        let huge = format!(r#"{{"model":"{}"}}"#, "x".repeat(65));
        assert_eq!(hook_payload_model(&huge), None);
    }

    // REAL-shaped quota captures. The Claude ones are built from the statusline schema's own
    // `rate_limits` block (code.claude.com/docs/en/statusline, verified 2026-09-01); the Codex ones
    // reproduce the shape of a live `~/.codex/sessions` rollout record observed the same day, with
    // its ids and account details replaced.
    const CLAUDE_QUOTA: &str = include_str!("../fixtures/claude_statusline_quota.json");
    /// `rate_limits` serialized BEFORE `context_window`: the field-order case an unanchored
    /// `used_percentage` read gets wrong.
    const CLAUDE_QUOTA_FIRST: &str = include_str!("../fixtures/claude_statusline_quota_first.json");
    const CLAUDE_QUOTA_BAD_RESET: &str =
        include_str!("../fixtures/claude_statusline_quota_malformed_reset.json");
    const CODEX_QUOTA: &str = include_str!("../fixtures/codex_rollout_rate_limits.jsonl");
    /// The older Codex shape, whose reset is a RELATIVE offset only a clock can resolve.
    const CODEX_QUOTA_RELATIVE: &str =
        include_str!("../fixtures/codex_rollout_rate_limits_relative.jsonl");

    /// The parse clock. Any fixed value works — the absolute-reset path must ignore it entirely.
    const NOW_MS: u64 = 1_788_000_000_000;

    fn usage(format: &str, payload: &str) -> UsageReport {
        parse_usage(format, payload, NOW_MS).unwrap()
    }

    fn quota(format: &str, payload: &str) -> QuotaReport {
        usage(format, payload).quota.unwrap()
    }

    #[test]
    fn claude_quota_reads_each_field_from_its_own_block() {
        // The payload carries `used_percentage` in four objects. Every window must come from its
        // own, and the context gauge must still come from `context_window`.
        let u = usage("claude-statusline-json", CLAUDE_QUOTA);
        let q = u.quota.unwrap();
        assert_eq!(
            q.windows,
            vec![
                QuotaWindowReading {
                    window: QuotaWindow::FiveHour,
                    pct: 24, // 23.5 rounds up
                    resets_at_ms: Some(1_788_425_600_000),
                },
                QuotaWindowReading {
                    window: QuotaWindow::SevenDay,
                    pct: 41,
                    resets_at_ms: Some(1_788_857_600_000),
                },
                QuotaWindowReading {
                    window: QuotaWindow::Spend,
                    pct: 63,
                    resets_at_ms: Some(1_790_787_200_000),
                },
            ]
        );
        // Highest percent wins: the spend limit at 63%, with its own reset instant.
        assert_eq!((q.pct, q.window.token()), (63, "spend"));
        assert_eq!(q.resets_at_ms, Some(1_790_787_200_000));
        assert_eq!(u.cost_usd, Some(3.4972));
        assert_eq!(u.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            u.session.as_deref(),
            Some("019f8aac-ff01-75d0-9bb1-7f0eab253ce7")
        );
        // The same payload's context gauge is unaffected by any of it.
        assert_eq!(parse_claude_statusline(CLAUDE_QUOTA).unwrap().pct, Some(78));
    }

    /// The anchoring regression this feature could have introduced in reverse: a payload that
    /// serializes `rate_limits` first would make an unanchored `used_percentage` read stamp the
    /// account quota as the pane's context gauge.
    #[test]
    fn a_quota_percent_is_never_read_as_the_context_gauge() {
        assert_eq!(
            parse_claude_statusline(CLAUDE_QUOTA_FIRST).unwrap().pct,
            Some(8),
            "the gauge is `context_window`'s 8, not five_hour's 91"
        );
        assert_eq!(
            parse_claude_statusline(CLAUDE_QUOTA_FIRST).unwrap().tokens,
            Some(15_500)
        );
        assert_eq!(quota("claude-statusline-json", CLAUDE_QUOTA_FIRST).pct, 91);
    }

    #[test]
    fn a_payload_with_only_a_context_window_reports_no_quota() {
        // The pre-`rate_limits` payload every other fixture uses: a gauge, and nothing to stamp on
        // the quota lane. NOT a clear — the block is absent for API-key auth and before the first
        // API response, so its absence says nothing about the account.
        let u = usage("claude-statusline-json", NORMAL);
        assert_eq!(u.quota, None);
        assert_eq!(u.cost_usd, None);
        assert!(!u.has_quota_observation(), "nothing for the chain to write");
        // The model still rides along, which is why the report itself is not `None`.
        assert_eq!(u.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn a_malformed_reset_leaves_the_percent_standing() {
        // `null` and a non-numeric string both yield no instant. The percent is the useful half and
        // must not be thrown away with it.
        let q = quota("claude-statusline-json", CLAUDE_QUOTA_BAD_RESET);
        assert_eq!((q.pct, q.window.token()), (55, "5h"));
        assert_eq!(q.resets_at_ms, None);
        assert!(q.windows.iter().all(|w| w.resets_at_ms.is_none()));
        // An out-of-range instant (here epoch MILLIseconds mistakenly sent as seconds) is rejected
        // rather than multiplied into the year 58691.
        let mixed =
            r#"{"rate_limits":{"five_hour":{"used_percentage":10,"resets_at":1788425600000}}}"#;
        assert_eq!(
            quota("claude-statusline-json", mixed).resets_at_ms,
            None,
            "a seconds field holding milliseconds is garbage, not a reading"
        );
    }

    #[test]
    fn codex_quota_takes_the_newest_record_and_skips_a_null_window() {
        // Two `token_count` records: the newest carries both windows, so `secondary` at 74% wins
        // over `primary` at 26%.
        let q = quota("codex-rollout-jsonl", CODEX_QUOTA);
        assert_eq!((q.pct, q.window.token()), (74, "secondary"));
        assert_eq!(q.resets_at_ms, Some(1_788_873_462_000));
        assert_eq!(q.windows.len(), 2);
        // The older record has `"secondary": null`, which is not an object: primary alone, and the
        // read must not fall through into `credits` or `plan_type`.
        let older = CODEX_QUOTA.lines().nth(1).unwrap();
        let q = quota("codex-rollout-jsonl", older);
        assert_eq!((q.pct, q.window.token()), (18, "primary"));
        assert_eq!(q.windows.len(), 1);
        // Codex publishes no cost and no statusline-shaped model.
        let u = usage("codex-rollout-jsonl", CODEX_QUOTA);
        assert_eq!((u.cost_usd, u.model), (None, None));
    }

    #[test]
    fn a_rollout_window_with_no_rate_limits_reports_no_quota() {
        // The pre-`rate_limits` rollout fixture: a gauge, no quota, nothing to write.
        assert_eq!(
            parse_usage("codex-rollout-jsonl", CODEX_ROLLOUT, NOW_MS),
            None
        );
        assert_eq!(
            parse_usage("codex-rollout-jsonl", CODEX_NO_RECORD, NOW_MS),
            None
        );
        // An unknown format is a silent ignore, as on the context path.
        assert_eq!(parse_usage("carrier-pigeon", CLAUDE_QUOTA, NOW_MS), None);
    }

    /// The unit trap, both directions. Claude states an ABSOLUTE epoch-seconds instant, Codex has
    /// published both that and a RELATIVE seconds offset; tma's contract is epoch ms everywhere, and
    /// the conversion happens here so no consumer ever has to know which channel it came from.
    #[test]
    fn reset_instants_convert_to_ms_at_the_parser() {
        // Claude: seconds x 1000, and the clock is not consulted at all.
        let claude = quota("claude-statusline-json", CLAUDE_QUOTA);
        assert_eq!(claude.windows[0].resets_at_ms, Some(1_788_425_600 * 1000));
        assert_eq!(
            parse_usage("claude-statusline-json", CLAUDE_QUOTA, 0)
                .unwrap()
                .quota,
            Some(claude),
            "an absolute instant is independent of the caller's clock"
        );

        // Codex's absolute form behaves the same way.
        assert_eq!(
            quota("codex-rollout-jsonl", CODEX_QUOTA).resets_at_ms,
            Some(1_788_873_462 * 1000)
        );

        // Codex's relative form is `now + offset x 1000`, so it MOVES with the clock. 7200 s on the
        // 42% primary window, which is the one that wins.
        let q = quota("codex-rollout-jsonl", CODEX_QUOTA_RELATIVE);
        assert_eq!((q.pct, q.window.token()), (42, "primary"));
        assert_eq!(q.resets_at_ms, Some(NOW_MS + 7_200_000));
        assert_eq!(
            parse_usage("codex-rollout-jsonl", CODEX_QUOTA_RELATIVE, NOW_MS + 60_000)
                .unwrap()
                .quota
                .unwrap()
                .resets_at_ms,
            Some(NOW_MS + 60_000 + 7_200_000)
        );
        // The seconds are never mistaken for ms: 7200 ms out would be two hours too early.
        assert_ne!(q.resets_at_ms, Some(NOW_MS + 7_200));
    }

    #[test]
    fn the_highest_window_wins_and_a_tie_takes_the_shorter_one() {
        let at = |five: u32, seven: u32| {
            let p = format!(
                r#"{{"rate_limits":{{"five_hour":{{"used_percentage":{five}}},"seven_day":{{"used_percentage":{seven}}}}}}}"#
            );
            let q = quota("claude-statusline-json", &p);
            (q.pct, q.window.token())
        };
        assert_eq!(at(90, 20), (90, "5h"));
        assert_eq!(at(20, 90), (90, "7d"));
        // A tie goes to the window declared first, which is the shorter and so more urgent one.
        assert_eq!(at(50, 50), (50, "5h"));
    }

    #[test]
    fn a_quota_percent_past_the_limit_clamps_to_a_full_gauge() {
        // Claude documents `spend_limit.used_percentage` running above 100 once exceeded; the option
        // is a documented `0..=100`, so it clamps rather than overflowing.
        let over = r#"{"rate_limits":{"spend_limit":{"used_percentage":137.4}}}"#;
        assert_eq!(quota("claude-statusline-json", over).pct, 100);
        // A negative reading is not a percent at all: that window drops out entirely.
        let negative = r#"{"rate_limits":{"five_hour":{"used_percentage":-1},"seven_day":{"used_percentage":30}}}"#;
        let q = quota("claude-statusline-json", negative);
        assert_eq!((q.pct, q.window.token(), q.windows.len()), (30, "7d", 1));
    }

    #[test]
    fn the_cost_comes_from_its_own_object_and_renders_to_two_decimals() {
        assert_eq!(
            usage("claude-statusline-json", CLAUDE_QUOTA).cost_usd,
            Some(3.4972)
        );
        assert_eq!(format_cost_usd(3.4972).as_deref(), Some("3.50"));
        assert_eq!(format_cost_usd(0.0).as_deref(), Some("0.00"));
        assert_eq!(format_cost_usd(12.0).as_deref(), Some("12.00"));
        assert_eq!(format_cost_usd(-1.0), None);
        assert_eq!(format_cost_usd(f64::NAN), None);
        // A same-named field outside the `cost` object cannot stand in for it.
        let decoy = r#"{"totals":{"total_cost_usd":99},"cost":{"total_duration_ms":45000}}"#;
        assert_eq!(parse_usage("claude-statusline-json", decoy, NOW_MS), None);
    }

    #[test]
    fn the_statusline_model_is_read_from_the_nested_object() {
        // `hook_payload_model` deliberately refuses this shape (its `model` is an object); the
        // statusline path needs the `id` inside it.
        assert_eq!(
            claude_statusline_model(r#"{"model":{"id":"claude-opus-5","display_name":"Opus"}}"#)
                .as_deref(),
            Some("claude-opus-5")
        );
        assert_eq!(
            claude_statusline_model(r#"{"model":"claude-opus-5"}"#),
            None
        );
        assert_eq!(
            claude_statusline_model(r#"{"model":{"display_name":"Opus"}}"#),
            None
        );
        let huge = format!(r#"{{"model":{{"id":"{}"}}}}"#, "x".repeat(65));
        assert_eq!(claude_statusline_model(&huge), None);
    }

    #[test]
    fn find_object_is_brace_balanced_and_quote_aware() {
        let nested = r#"{"a":{"b":{"c":1},"d":2},"e":3}"#;
        assert_eq!(find_object(nested, "a"), Some(r#"{"b":{"c":1},"d":2}"#));
        assert_eq!(find_object(nested, "b"), Some(r#"{"c":1}"#));
        // A brace inside a string value does not end the object.
        let braced = r#"{"a":{"path":"/tmp/{x}/y","n":1},"b":2}"#;
        assert_eq!(
            find_object(braced, "a"),
            Some(r#"{"path":"/tmp/{x}/y","n":1}"#)
        );
        // An escaped quote does not end the string either.
        let escaped = r#"{"a":{"t":"he said \"}\" once","n":1}}"#;
        assert_eq!(
            find_object(escaped, "a"),
            Some(r#"{"t":"he said \"}\" once","n":1}"#)
        );
        // A non-object value is not an anchor, so a field read "inside" it cannot spill onward.
        assert_eq!(find_object(r#"{"a":null,"b":{"n":1}}"#, "a"), None);
        assert_eq!(find_object(r#"{"a":7}"#, "a"), None);
        assert_eq!(find_object(r#"{"a":"{}"}"#, "a"), None);
        assert_eq!(find_object(r#"{"b":1}"#, "a"), None);
        // A truncated payload yields nothing rather than a partial slice.
        assert_eq!(find_object(r#"{"a":{"n":1"#, "a"), None);
    }

    #[test]
    fn codex_rollout_model_reads_the_turn_context_model() {
        assert_eq!(
            codex_rollout_model(CODEX_ROLLOUT).as_deref(),
            Some("gpt-5-codex")
        );
        // `model_context_window` is a numeric field, not the `"model"` string key, so a window with
        // only token_count records yields no model name.
        assert_eq!(codex_rollout_model(CODEX_TRUNCATED), None);
    }
}
