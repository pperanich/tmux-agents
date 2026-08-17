//! Shared dashboard scaffolding for the two live agent surfaces: the fuzzy picker and
//! the `watch` sidebar. Both paint a first frame from stamps, own a 1 s guarded-poll
//! refresh with config + manifest hot-reload, and render a state-sorted, bordered "agents (N)"
//! list with a REVERSED highlight and a one-line footer. The pieces that differ (the picker's
//! preview pane and fuzzy filter, each surface's per-row span layout and footer text) stay in
//! the surface; the control flow, selection model, and list chrome live here.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;
use tma_core::AgentRow;
use tma_runtime::config;
use tma_runtime::cycle;
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::Tmux;

use tma_ui_core::render::{truncate, truncate_locator, AGENT_W, LOCATOR_W, TIME_W};

/// The input-poll granularity: `event::poll` waits at most this long, so a keypress (or a
/// SIGUSR1 nudge, for `watch`) is serviced within one interval. Each surface owns its own
/// 1 s refresh cadence in its core `RefreshGate` (epoch-ms), so the cycle deadline is no longer here.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The three shared row columns, single-space separated: agent (left, cyan, truncated to [`AGENT_W`]),
/// locator (left, [`truncate_locator`] to keep its `:window.pane` suffix), and time-in-state (right,
/// grey, [`TIME_W`]). Each surface wraps these with its own prefix (the picker's quick-select index,
/// each surface's state glyph) and a trailing title, so the grid is identical while the chrome differs.
pub(crate) fn grid_columns(agent: &str, locator: &str, time: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{:<w$} ", truncate(agent, AGENT_W), w = AGENT_W),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(format!(
            "{:<w$} ",
            truncate_locator(locator, LOCATOR_W),
            w = LOCATOR_W
        )),
        Span::styled(
            format!("{time:>w$}", w = TIME_W),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

/// The shared refresh tick: one guarded poll cycle over the (already hot-reloaded) pair. `None`
/// when the cycle failed (caller keeps its rows).
pub(crate) fn refresh(
    tmux: &Tmux,
    config: &config::Config,
    manifests: &[LoadedManifest],
) -> Option<Vec<AgentRow>> {
    cycle::run_cycle(tmux, manifests, &config.fold_config())
        .ok()
        .map(|r| {
            // Annotate repo/branch on the refresh path so both the watch and the picker get labels;
            // the memoized resolver runs no git inside `run_cycle` (the first stamp frame stays bare).
            let mut rows = r.rows;
            tma_runtime::repo::annotate_rows(&mut rows);
            rows
        })
}

/// What the list highlights and where its window sits: the selected draw line, the hovered one (the
/// pointer's, dimmer), the scroll offset, and the agent count for the title. The fold owns all four
/// — the offset especially, because the mouse hit-test resolves a click against the same number.
pub(crate) struct ListSelection {
    pub(crate) selected: usize,
    pub(crate) hovered: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) count: usize,
}

/// Mark the hovered line with a dim reversed background: visibly "the mouse is here" without
/// competing with the selection's full REVERSED highlight. A hovered line that is also the selected
/// one keeps the selection style (the widget paints that over this).
pub(crate) fn with_hover(items: Vec<ListItem<'_>>, hovered: Option<usize>) -> Vec<ListItem<'_>> {
    let Some(h) = hovered else {
        return items;
    };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if i == h {
                item.style(Style::default().add_modifier(Modifier::DIM | Modifier::REVERSED))
            } else {
                item
            }
        })
        .collect()
}

/// Render the bordered "agents (N)" list with the REVERSED highlight. `items` are the per-surface
/// row spans (which may include interleaved group-header lines); `sel` carries the highlighted and
/// hovered draw indices, the scroll offset, and the agent count for the title.
pub(crate) fn render_agent_list(
    f: &mut Frame,
    area: Rect,
    items: Vec<ListItem>,
    sel: &ListSelection,
) {
    let mut list_state = ListState::default();
    if sel.count > 0 {
        list_state.select(Some(sel.selected));
    }
    // The fold's own offset, not one the widget picks per frame: a click is resolved back through
    // this number, so the two must be the same.
    *list_state.offset_mut() = sel.scroll;
    let list = List::new(with_hover(items, sel.hovered))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" agents ({}) ", sel.count)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Char width of each shared column span, for the alignment assertions below.
    fn col_widths(agent: &str, locator: &str, time: &str) -> Vec<usize> {
        grid_columns(agent, locator, time)
            .iter()
            .map(|s| s.content.chars().count())
            .collect()
    }

    #[test]
    fn grid_columns_hold_a_fixed_width_across_mixed_rows() {
        // A short/fitting row and a long/truncated row must produce identical column widths so the
        // locator and time land at the same x offset regardless of content length.
        let short = col_widths("cl", "a:1.0", "5s");
        let long = col_widths("verylongagentname", "tmux-agents-experiments:2.0", "120h");
        assert_eq!(short, long, "columns align across rows");
        // agent + separator, locator + separator, time (no trailing separator).
        assert_eq!(short, vec![AGENT_W + 1, LOCATOR_W + 1, TIME_W]);
    }

    #[test]
    fn grid_columns_truncate_agent_and_locator_but_keep_the_time() {
        let cols = grid_columns("verylongagentname", "tmux-agents-experiments:2.0", "9s");
        assert_eq!(cols[0].content.trim_end(), "verylon…");
        assert_eq!(cols[1].content.trim_end(), "tmux-agent…:2.0");
        assert_eq!(cols[2].content, "  9s", "right-aligned in TIME_W");
    }

    #[test]
    fn agent_list_renders_the_titled_grid_and_highlights_selection() {
        use crate::test_render::{lines, render, reversed_rows};
        use ratatui::text::Line;

        let items = vec![
            ListItem::new(Line::from(grid_columns("alpha", "proj:1.0", "5s"))),
            ListItem::new(Line::from(grid_columns("bravo", "proj:2.0", "9s"))),
        ];
        let buf = render(60, 6, |f| {
            let area = f.area();
            render_agent_list(
                f,
                area,
                items,
                &ListSelection {
                    selected: 1,
                    hovered: None,
                    scroll: 0,
                    count: 2,
                },
            );
        });
        let ls = lines(&buf);
        // The bordered title reflects the agent count.
        assert!(
            ls[0].contains("agents (2)"),
            "the list titles the agent count: {:?}",
            ls[0]
        );
        // The grid columns render each row's agent, locator, and time.
        assert!(
            ls[1].contains("alpha") && ls[1].contains("proj:1.0") && ls[1].contains("5s"),
            "first row grid: {:?}",
            ls[1]
        );
        assert!(
            ls[2].contains("bravo") && ls[2].contains("proj:2.0"),
            "second row grid: {:?}",
            ls[2]
        );
        // The selected (second) row carries the highlight.
        assert_eq!(
            reversed_rows(&buf),
            vec![2],
            "the second row is highlighted"
        );
    }
}
