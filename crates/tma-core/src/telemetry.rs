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
    match find_number(payload, "used_percentage") {
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
    let anchor = payload.find("\"context_window\"")?;
    count(find_number(&payload[anchor..], "total_input_tokens"))
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
