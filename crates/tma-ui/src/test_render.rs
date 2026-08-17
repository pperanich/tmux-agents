//! Test-only draw harness: render a `Frame` closure through ratatui's in-memory `TestBackend` and
//! read the resulting buffer back as row strings (plus a per-row REVERSED probe, since the list
//! surfaces mark the selected row by style, not a glyph). Shared by the `watch`/`picker`/`dash`
//! draw-layer tests so they assert on real cell contents without a PTY.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use ratatui::{Frame, Terminal};
use tma_core::{AgentRow, AgentState, RepoLabel};

/// Draw one frame at `width`x`height` and return a clone of the rendered buffer.
pub(crate) fn render(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

/// Each buffer row joined into a string of its cell symbols (top to bottom).
pub(crate) fn lines(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        })
        .collect()
}

/// The row indices carrying at least one REVERSED cell: the highlight the list widgets paint on the
/// selected row (no `highlight_symbol` is set, so selection is only assertable through the style).
pub(crate) fn reversed_rows(buf: &Buffer) -> Vec<usize> {
    (0..buf.area.height as usize)
        .filter(|&y| {
            (0..buf.area.width).any(|x| {
                buf.cell((x, y as u16))
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
            })
        })
        .collect()
}

/// The row indices carrying at least one DIM cell: the hover highlight, which is deliberately the
/// dim sibling of the selection's REVERSED (a hovered row carries both).
pub(crate) fn dim_rows(buf: &Buffer) -> Vec<usize> {
    (0..buf.area.height as usize)
        .filter(|&y| {
            (0..buf.area.width).any(|x| {
                buf.cell((x, y as u16))
                    .is_some_and(|c| c.modifier.contains(Modifier::DIM))
            })
        })
        .collect()
}

/// A minimal agent row for the draw tests; callers mutate `agent`/`title`/`repo` as needed.
pub(crate) fn row(
    pane: &str,
    session: &str,
    w: u32,
    p: u32,
    state: AgentState,
    since: u64,
) -> AgentRow {
    AgentRow {
        pane_id: pane.to_string(),
        agent: "claude".to_string(),
        state,
        detail: None,
        since,
        session: session.to_string(),
        window_index: w,
        pane_index: p,
        title: "task".to_string(),
        attention: false,
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

/// Attach a resolved repo/branch label to a row (drives grouping and the branch column).
pub(crate) fn with_repo(mut r: AgentRow, name: &str, branch: &str) -> AgentRow {
    r.repo = Some(RepoLabel {
        name: name.to_string(),
        branch: branch.to_string(),
        worktree: false,
    });
    r
}
