//! Pure render helpers shared by the picker and watch surfaces: the agent-row column widths and the
//! small text/style formatters (`row_style`, `fmt_since`, `truncate`, `truncate_locator`). A neutral
//! home so both folds and the shell draws reach one copy; no terminal, no tmux handle, no I/O.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use tma_core::AgentRow;

use crate::palette::RowPalette;

/// Shared column widths for the picker and watch agent rows, so a column lands at the same offset
/// across every row of a surface and at the same width in both. Widths count Rust `char`s, not
/// terminal cells: a user-chosen tmux session name with wide glyphs (CJK, emoji) will nudge the
/// columns after it on that one row. That matches the rest of this UI, which is ASCII-width
/// throughout; cell-width machinery for the rare wide-name case is deliberately out of scope.
pub const AGENT_W: usize = 8;
pub const LOCATOR_W: usize = 16;
/// Sized to [`fmt_since`]'s widest realistic output (`999h`, `-`); right-aligned.
pub const TIME_W: usize = 4;
/// The branch-label column width for the watch table and the picker span. The tighter
/// list arms (32-col sidebar) pass `10` to [`branch_span`] instead; the table cell reserves this.
pub const BRANCH_W: usize = 12;

/// The `(glyph, color)` for an agent row: the "done" style for an idle pane still carrying
/// `@agent_attention` (unreviewed), else the plain per-state style. Shared by the picker and watch.
pub fn row_style(palette: &RowPalette, r: &AgentRow) -> (String, Color) {
    if r.state == tma_core::AgentState::Idle && r.attention {
        palette.done()
    } else {
        palette.state(r.state)
    }
}

/// Human time-in-state (`now - since`): `12s`, `4m`, `2h`. `now`/`since` are epoch **milliseconds**;
/// the age is converted to whole seconds for display. Shared by the picker and watch.
pub fn fmt_since(now: u64, since: u64) -> String {
    if since == 0 {
        return "-".to_string();
    }
    let secs = now.saturating_sub(since) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Truncate to `max` characters with a trailing `…`. Shared by the picker and watch; used for titles.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Truncate a `session:window.pane` locator to `max` chars, eliding the *session* head and keeping
/// the `:window.pane` suffix intact (`tmux-agents-experiments:2.0` at 16 → `tmux-agent…:2.0`, not
/// `tmux-agents-exp…`). Falls back to plain [`truncate`] when there is no suffix, or when the suffix
/// leaves no room for at least one head char plus the ellipsis. Char-count, not cell-count (see the
/// grid-width note in `dash`).
pub fn truncate_locator(loc: &str, max: usize) -> String {
    if loc.chars().count() <= max {
        return loc.to_string();
    }
    // The suffix is the final `:window.pane`; split on the last `:` so the window/pane coordinates
    // always survive even if the session name itself contains a colon.
    let Some((session, tail)) = loc.rsplit_once(':') else {
        return truncate(loc, max);
    };
    let suffix_len = tail.chars().count() + 1; // the `:` plus `window.pane`
    if suffix_len + 2 > max {
        return truncate(loc, max);
    }
    format!("{}:{}", truncate(session, max - suffix_len - 1), tail)
}

/// The dimmed branch label shared by the list arms (ListOnly/ListAndPreview) and the picker: the
/// pane's `branch` truncated to `width` and blank-padded to it (so titles after it stay aligned),
/// with a leading space separating it from the preceding time column. An empty span when
/// `show_branch` is false — no visible row resolved a branch, so no column is spent. List arms pass
/// `10`; the picker passes [`BRANCH_W`].
pub fn branch_span(branch: Option<&str>, width: usize, show_branch: bool) -> Span<'static> {
    if !show_branch {
        return Span::raw(String::new());
    }
    let label = branch.map(|b| truncate(b, width)).unwrap_or_default();
    Span::styled(
        format!(" {label:<width$}"),
        Style::default().fg(Color::DarkGray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::AgentState;

    fn row(state: AgentState, attention: bool, since: u64) -> AgentRow {
        AgentRow {
            pane_id: "%00".to_string(),
            agent: "c".to_string(),
            state,
            detail: None,
            since,
            session: "a".to_string(),
            window_index: 0,
            pane_index: 0,
            title: "t".to_string(),
            attention,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
        }
    }

    #[test]
    fn done_row_uses_done_style_idle_row_does_not() {
        let palette = RowPalette::default();
        let idle = row(AgentState::Idle, false, 10);
        assert_eq!(row_style(&palette, &idle), ("○".to_string(), Color::Green));
        // Same pane once attention is set: the done glyph/color.
        let done = row(AgentState::Idle, true, 10);
        assert_eq!(
            row_style(&palette, &done),
            ("✓".to_string(), Color::Magenta)
        );
        // Attention on a non-idle state is not "done" — blocked keeps its own style.
        let blocked = row(AgentState::Blocked, true, 10);
        assert_eq!(row_style(&palette, &blocked), ("⚑".to_string(), Color::Red));
    }

    #[test]
    fn truncate_locator_preserves_the_window_pane_suffix() {
        // The documented case: elide the session head, keep `:2.0`.
        assert_eq!(
            truncate_locator("tmux-agents-experiments:2.0", 16),
            "tmux-agent…:2.0"
        );
        // A short session still keeps its suffix when the whole thing is over budget.
        let out = truncate_locator("myproject:1.2", 12);
        assert!(out.ends_with(":1.2"), "suffix survives: {out}");
        assert!(out.chars().count() <= 12);
    }

    #[test]
    fn truncate_locator_returns_a_fitting_locator_unchanged() {
        // Exactly at the column and under it: no ellipsis, no change.
        assert_eq!(truncate_locator("a:1.0", 5), "a:1.0");
        assert_eq!(truncate_locator("a:1.0", 16), "a:1.0");
    }

    #[test]
    fn truncate_locator_falls_back_when_the_suffix_alone_overflows() {
        // The suffix `:100.200` cannot fit in 4 columns: plain truncation, suffix sacrificed.
        assert_eq!(truncate_locator("s:100.200", 4), truncate("s:100.200", 4));
    }

    #[test]
    fn truncate_locator_handles_an_empty_session() {
        // Empty session, fits: unchanged. Empty session, over budget: plain-truncate fallback.
        assert_eq!(truncate_locator(":1.0", 10), ":1.0");
        assert_eq!(truncate_locator(":10.20", 5), truncate(":10.20", 5));
    }

    #[test]
    fn branch_span_hidden_is_empty() {
        // No visible row resolved a branch: the helper spends no column.
        let s = branch_span(Some("main"), 10, false);
        assert_eq!(s.content, "");
    }

    #[test]
    fn branch_span_pads_and_truncates_to_a_fixed_cell() {
        // Shown: a short branch pads and a long branch truncates, both to the same char width so the
        // titles after them align. Leading space separates from the time column; the span is dimmed.
        let short = branch_span(Some("main"), 10, true);
        let long = branch_span(Some("feature/very-long-branch"), 10, true);
        assert_eq!(short.content.chars().count(), long.content.chars().count());
        assert!(short.content.starts_with(" main"));
        assert!(
            short.content.ends_with(' '),
            "short branch pads out: {short:?}"
        );
        assert!(
            long.content.contains('…'),
            "long branch truncates: {long:?}"
        );
        assert_eq!(short.style.fg, Some(Color::DarkGray), "dimmed");
    }

    #[test]
    fn branch_span_unresolved_row_still_pads_when_shown() {
        // A pane with no branch, but some visible row resolved one: a blank, fixed-width cell keeps
        // the title aligned with the resolved rows.
        let none = branch_span(None, 10, true);
        let some = branch_span(Some("x"), 10, true);
        assert_eq!(none.content.chars().count(), some.content.chars().count());
        assert!(!none.content.contains('…'));
    }

    #[test]
    fn fmt_since_scales() {
        // now/since are epoch ms; the age divides to whole seconds.
        assert_eq!(fmt_since(100_000, 0), "-");
        assert_eq!(fmt_since(130_000, 100_000), "30s");
        assert_eq!(fmt_since(100_000 + 300_000, 100_000), "5m");
        assert_eq!(fmt_since(100_000 + 7_200_000, 100_000), "2h");
    }
}
