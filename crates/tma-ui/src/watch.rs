//! `tma watch`: a persistent, non-modal dashboard for a normal tmux pane
//! (`split-window -h -l 32 "tma watch"`); popups are modal, so the persistent surface must be a
//! pane. It is the picker's pattern minus the popup, fuzzy filter, and session scope: the same
//! state-priority rows, 1 s guarded-poll refresh (an ambient producer), config + manifest hot-
//! reload, and first-frame-from-stamps paint. Preview is width-driven: below `PREVIEW_MIN_WIDTH` a
//! single list, at or above it a live ANSI preview beside it. Enter jumps the acting client and
//! clears its attention flag but keeps the dashboard running; `q`/Esc/Ctrl-C quit. Startup advertises
//! a pid for the middle-tier SIGUSR1 nudge (see [`run_watch`]); styling builds a `RowPalette` from
//! the `[picker]` config.
//!
//! This shell owns only the impure edges: seeding from stamps, the nudge wiring, and the draw. The
//! rows/selection/refresh/preview/layout fold lives in [`tma_ui_core::watch::WatchModel`]; the loop
//! and effect execution live in [`crate::runner`].

use std::io;
use std::path::PathBuf;
use std::process;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use tma_core::{AgentRow, Selector};
use tma_runtime::config;
use tma_runtime::cycle;
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::{nudge, Server, Tmux};
use tma_ui_core::layout::{watch_geom, WatchGeom};
use tma_ui_core::render::{branch_span, fmt_since, row_style, truncate};
use tma_ui_core::watch::{
    table_header, table_row, table_title_width, DisplayItem, WatchLayout, WidePref,
};
use tma_ui_core::{Effect, Event, RowPalette, WatchModel};

use crate::dash;
use crate::picker::unix_now;
use crate::runner::{self, Surface};

/// Run the watch sidebar to completion: first frame from stamps, a 1 s guarded-poll refresh (an
/// ambient producer), Enter jumps the acting client without closing. `config`/`manifests` owned for
/// hot-reload. `selector` narrows the rows the fold ever sees — the sidebar's cycle still refreshes
/// every pane, so a scoped sidebar remains a full ambient producer.
#[allow(clippy::too_many_arguments)]
pub fn run_watch(
    tmux: &Tmux,
    server: &Server,
    config: config::Config,
    manifests: Vec<LoadedManifest>,
    config_path: Option<PathBuf>,
    manifest_dir: Option<PathBuf>,
    acting_client: Option<&str>,
    start_table: bool,
    selector: Selector,
) -> io::Result<()> {
    // First frame from stamps: instant, stale-tolerant. The next refresh runs a cycle. Stamp rows
    // carry no repo label yet, so a repo/branch selector shows an empty first frame that the first
    // refresh fills in.
    let mut first_rows = cycle::stamp_rows(tmux).unwrap_or_default();
    selector.retain(&mut first_rows);
    // Wide-mode preference: `--table` opens straight into the table, else the preview. `p` flips it
    // at runtime; it is session-local (never persisted). Below the width threshold it is dormant.
    let pref = if start_table {
        WidePref::Table
    } else {
        WidePref::Preview
    };
    let model = WatchModel::new(first_rows, pref, unix_now());

    // Middle-tier nudge: install the SIGUSR1 handler first, then let the RAII terminal guard
    // advertise our pid in the pane-scoped `@tma_watch_pid` on our own pane, but only *after* raw
    // mode + the alternate screen come up, so a failed setup never strands a stale pid. `$TMUX_PANE`
    // is our real pane (a normally-launched pane process inherits it, unlike a `run-shell` hook's
    // stale value); absent means we run outside tmux, so the guard advertises nothing and we fall
    // back to the 1 s self-refresh. The guard unsets the pid and restores the terminal on every exit.
    nudge::install_nudge_handler();
    let watch_pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty());
    let guard = crate::term::TerminalGuard::enter(tmux, watch_pane, Some(process::id()))?;

    runner::run_surface(
        model,
        (),
        guard,
        runner::SurfaceEnv {
            tmux,
            server,
            config,
            manifests,
            config_path,
            manifest_dir,
            acting_client,
            filter: runner::RowFilter::from_selector(selector),
        },
    )
}

impl Surface for WatchModel {
    type Res = ();

    fn update(&mut self, ev: Event, now: u64, res: &mut ()) -> Vec<Effect> {
        WatchModel::update(self, ev, now, res)
    }

    fn view(&self, f: &mut Frame, now: u64, palette: &RowPalette) {
        draw(f, self, now, palette);
    }
}

/// Render the sidebar: a sorted agent list plus a one-line footer, laid out per the model's
/// [`layout`](WatchModel::layout). The two list arms paint the compact 32-column row (glyph+agent+locator, time/title
/// clip) with (for the wide arm) a live preview beside it; the table arm reclaims the preview's width
/// for the full-width status columns.
fn draw(f: &mut Frame, model: &WatchModel, now: u64, palette: &RowPalette) {
    let layout = model.layout();
    // The geometry comes from the core, which is also what the fold hit-tests a click against: the
    // click must resolve to the row the draw painted under the pointer, so there is one split.
    let geom = watch_geom(f.area(), layout);
    let footer_area = geom.footer;
    let sel = dash::ListSelection {
        selected: model.draw_selection(),
        hovered: model.hover(),
        scroll: model.scroll(),
        count: model.row_count(),
    };

    match layout {
        // The narrow MVP stays a FLAT labeled list: header lines would spend scarce vertical rows.
        WatchLayout::ListOnly => {
            let show_branch = model.show_branch();
            let items = model
                .rows()
                .map(|r| compact_row(r, now, palette, show_branch))
                .collect();
            dash::render_agent_list(f, geom.list.rect, items, &sel);
        }
        WatchLayout::ListAndPreview => {
            let show_branch = model.show_branch();
            let items = display_list(model, |r| compact_row(r, now, palette, show_branch));
            dash::render_agent_list(f, geom.list.rect, items, &sel);

            // Preview title mirrors the picker's (`picker.rs`): highlighted locator, `" preview "`
            // fallback when the list is empty.
            let preview_title = model
                .selected_row()
                .map(|r| format!(" {} ", r.locator()))
                .unwrap_or_else(|| " preview ".to_string());
            let preview_widget = Paragraph::new(model.preview_text().clone())
                .block(Block::default().borders(Borders::ALL).title(preview_title));
            if let Some(area) = geom.preview {
                f.render_widget(preview_widget, area);
            }
        }
        WatchLayout::Table => draw_table(f, &geom, model, now, palette, &sel),
    }

    // The `p` hint reflects what the toggle would do from the current wide body; below the width
    // threshold the toggle is dormant, so the narrow footer omits it.
    let toggle_hint = match layout {
        WatchLayout::ListAndPreview => "  p=table",
        WatchLayout::Table => "  p=preview",
        WatchLayout::ListOnly => "",
    };
    // Grouping renders only in the wide arms, so its hint appears only there; it names the flip.
    let group_hint = match layout {
        WatchLayout::ListOnly => "",
        _ if model.grouped() => "  g=flat",
        _ => "  g=group",
    };
    let footer = Line::from(Span::styled(
        format!(" enter=jump  a=act{toggle_hint}{group_hint}  q=quit"),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(footer), footer_area);
}

/// One compact list row (both list arms): glyph + the shared grid + the dimmed branch label
/// (after the time column, before the title) + a clipped title, sized for a ~32-column pane.
fn compact_row(
    r: &AgentRow,
    now: u64,
    palette: &RowPalette,
    show_branch: bool,
) -> ListItem<'static> {
    let (glyph, color) = row_style(palette, r);
    let time = fmt_since(now, r.since);
    let mut spans = vec![Span::styled(
        format!("{glyph} "),
        Style::default().fg(color),
    )];
    spans.extend(dash::grid_columns(&r.agent, &r.locator(), &time));
    spans.push(branch_span(r.branch(), 10, show_branch));
    spans.push(Span::raw(format!(" {}", truncate(&r.title, 40))));
    ListItem::new(Line::from(spans))
}

/// A dimmed `▸ repo-name` group-header line, interleaved into the wide list arms above each group.
fn group_header(name: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("▸ {name}"),
        Style::default().fg(Color::DarkGray),
    )))
}

/// Both wide arms' list items: walk the model's display order, rendering each header line as a
/// dimmed `▸ repo` and each row through `row`. The selection index that goes with it is
/// [`WatchModel::display_selection`]; neither side computes an offset here.
fn display_list(
    model: &WatchModel,
    row: impl Fn(&AgentRow) -> ListItem<'static>,
) -> Vec<ListItem<'static>> {
    model
        .display_items()
        .map(|item| match item {
            DisplayItem::Header(name) => group_header(name),
            DisplayItem::Row(r) => row(r),
        })
        .collect()
}

/// Render the full-width status table: a column header on the top line, the state-sorted rows below
/// as a borderless list with the shared REVERSED highlight (so selection and Enter-jump carry over
/// from the list arms). The model column appears only when a visible row carries `@agent_model`; the
/// pure column/header builders live in [`tma_ui_core::watch`].
fn draw_table(
    f: &mut Frame,
    geom: &WatchGeom,
    model: &WatchModel,
    now: u64,
    palette: &RowPalette,
    sel: &dash::ListSelection,
) {
    let show_model = model.show_model();
    let show_branch = model.show_branch();
    if let Some(area) = geom.table_header {
        f.render_widget(Paragraph::new(table_header(show_model, show_branch)), area);
    }

    let rows_area = geom.list.rect;
    let title_w = table_title_width(rows_area.width, show_model, show_branch);
    // The `▸ repo` headers are draw-only, so Enter-jump still reads the model's flat selection.
    let items = display_list(model, |r| {
        ListItem::new(table_row(palette, r, now, show_model, show_branch, title_w))
    });
    let mut list_state = ListState::default();
    if sel.count > 0 {
        list_state.select(Some(sel.selected));
    }
    *list_state.offset_mut() = sel.scroll;
    let list = List::new(dash::with_hover(items, sel.hovered))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, rows_area, &mut list_state);
}

#[cfg(test)]
mod draw_tests {
    use super::*;
    use crate::test_render::{dim_rows, lines, render, reversed_rows, row, with_repo};
    use tma_core::AgentState;
    use tma_ui_core::{Key, Mouse, MouseKind};

    const NOW: u64 = 100_000;
    const SINCE: u64 = 95_000; // fmt_since(NOW, SINCE) == "5s"

    /// Two repos plus a no-repo row, agents distinct so each buffer row is identifiable.
    fn three_rows() -> Vec<AgentRow> {
        let mut app = with_repo(
            row("%app", "s", 1, 0, AgentState::Blocked, SINCE),
            "app",
            "main",
        );
        app.agent = "alpha".to_string();
        let mut lib = with_repo(
            row("%lib", "s", 2, 0, AgentState::Working, SINCE),
            "lib",
            "dev",
        );
        lib.agent = "bravo".to_string();
        let mut none = row("%none", "s", 3, 0, AgentState::Idle, SINCE);
        none.agent = "charlie".to_string();
        vec![app, lib, none]
    }

    /// Drive the model to a real width so the layout matches the render dimensions.
    fn at_width(m: &mut WatchModel, width: u16) {
        m.update(Event::Resize { width, height: 20 }, NOW, &mut ());
    }

    /// Drive the model the way the runner does: keys through the fold, never a field poke.
    fn key(m: &mut WatchModel, k: Key) {
        m.update(Event::Key(k), NOW, &mut ());
    }

    #[test]
    fn watch_grouped_wide_shows_headers_rows_and_selection() {
        let mut m = WatchModel::new(three_rows(), WidePref::Preview, NOW);
        at_width(&mut m, 100);
        assert_eq!(m.layout(), WatchLayout::ListAndPreview);
        assert!(m.grouped());
        // Select the lib pane (display index 1: each row is its own repo group, urgency-ordered).
        key(&mut m, Key::Down);
        assert_eq!(m.selected_row().unwrap().pane_id, "%lib");

        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &m, NOW, &palette));
        let ls = lines(&buf);

        // Group headers, urgency-ordered: app (blocked), lib (working), then the no-repo bucket.
        let app_h = ls
            .iter()
            .position(|l| l.contains("▸ app"))
            .expect("app header");
        let lib_h = ls
            .iter()
            .position(|l| l.contains("▸ lib"))
            .expect("lib header");
        let none_h = ls
            .iter()
            .position(|l| l.contains("▸ (no repo)"))
            .expect("no-repo header");
        assert!(
            app_h < lib_h && lib_h < none_h,
            "headers in urgency order: {ls:?}"
        );
        // Each group's row renders directly under its header.
        assert!(
            ls[app_h + 1].contains("alpha"),
            "app row under its header: {ls:?}"
        );
        assert!(
            ls[lib_h + 1].contains("bravo"),
            "lib row under its header: {ls:?}"
        );
        assert!(
            ls[none_h + 1].contains("charlie"),
            "no-repo row under its header: {ls:?}"
        );
        // The highlight lands on the lib row, its display index shifted past the two headers.
        assert_eq!(
            reversed_rows(&buf),
            vec![lib_h + 1],
            "selection marks the lib row"
        );
    }

    #[test]
    fn watch_flat_drops_headers_and_keeps_the_selection() {
        let mut m = WatchModel::new(three_rows(), WidePref::Preview, NOW);
        at_width(&mut m, 100);
        key(&mut m, Key::Down);
        // `g` flips to the flat list; the selection follows its pane across the reorder.
        key(&mut m, Key::Char('g'));
        assert!(!m.grouped());
        assert_eq!(m.selected_row().unwrap().pane_id, "%lib");

        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &m, NOW, &palette));
        let ls = lines(&buf);
        assert!(
            ls.iter().all(|l| !l.contains('▸')),
            "the flat view renders no group headers: {ls:?}"
        );
        // Flat state order (blocked, working, idle): the working lib row is second, at buffer y 2.
        assert!(
            ls[2].contains("bravo"),
            "lib row second in flat order: {ls:?}"
        );
        assert_eq!(
            reversed_rows(&buf),
            vec![2],
            "selection intact on the lib row"
        );
    }

    #[test]
    fn watch_preview_pane_appears_only_when_wide() {
        const MARKER: &str = "PREVIEWCELL";
        let palette = RowPalette::default();

        /// Land a capture for the highlighted pane, the only way text enters the cache.
        fn capture(m: &mut WatchModel, ansi: &str) {
            let pane = m.selected_row().unwrap().pane_id.clone();
            m.update(
                Event::PreviewCaptured {
                    pane,
                    ansi: ansi.to_string(),
                },
                NOW,
                &mut (),
            );
        }

        // Wide: the preview paragraph renders its text beside the list.
        let mut wide = WatchModel::new(three_rows(), WidePref::Preview, NOW);
        at_width(&mut wide, 100);
        capture(&mut wide, MARKER);
        let buf = render(100, 14, |f| draw(f, &wide, NOW, &palette));
        assert!(
            lines(&buf).iter().any(|l| l.contains(MARKER)),
            "the wide layout shows the preview pane"
        );

        // Narrow: below the threshold the body is a single list, no preview pane.
        let mut narrow = WatchModel::new(three_rows(), WidePref::Preview, NOW);
        at_width(&mut narrow, 40);
        capture(&mut narrow, MARKER);
        assert_eq!(narrow.layout(), WatchLayout::ListOnly);
        let buf = render(40, 14, |f| draw(f, &narrow, NOW, &palette));
        assert!(
            lines(&buf).iter().all(|l| !l.contains(MARKER)),
            "the narrow layout hides the preview pane"
        );
    }

    #[test]
    fn watch_table_renders_the_column_header_and_rows() {
        let mut m = WatchModel::new(three_rows(), WidePref::Table, NOW);
        at_width(&mut m, 120);
        assert_eq!(m.layout(), WatchLayout::Table);

        let palette = RowPalette::default();
        let buf = render(120, 14, |f| draw(f, &m, NOW, &palette));
        let ls = lines(&buf);
        // The column header sits on the top line.
        assert!(
            ls[0].contains("agent") && ls[0].contains("state") && ls[0].contains("where"),
            "table column header: {:?}",
            ls[0]
        );
        assert!(
            ls.iter().any(|l| l.contains("▸ app")),
            "the grouped table interleaves repo headers: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("blocked")),
            "the blocked row shows its state token: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("5s")),
            "the fixed since renders deterministically as 5s: {ls:?}"
        );
    }

    /// Hover paints the line the pointer is on, dim, while the selection keeps its own highlight —
    /// and the draw puts them on exactly the screen rows the fold's hit-test named.
    #[test]
    fn watch_hover_marks_the_pointed_row_beside_the_selection() {
        let mut m = WatchModel::new(three_rows(), WidePref::Preview, NOW);
        m.update(
            Event::Resize {
                width: 32,
                height: 10,
            },
            NOW,
            &mut (),
        );
        assert_eq!(m.layout(), WatchLayout::ListOnly);
        // Pointer on the third list row (screen row 3, the border being row 0).
        m.update(
            Event::Mouse(Mouse {
                kind: MouseKind::Moved,
                col: 4,
                row: 3,
            }),
            NOW,
            &mut (),
        );
        let palette = RowPalette::default();
        let buf = render(32, 10, |f| draw(f, &m, NOW, &palette));
        assert_eq!(dim_rows(&buf), vec![3], "the hovered row is dimmed");
        assert!(
            reversed_rows(&buf).contains(&1),
            "the selection keeps its own highlight on the first row"
        );
    }

    /// A list taller than its pane draws the fold's own window, so the row a click resolves to is
    /// the row the user sees under the pointer.
    #[test]
    fn watch_draws_the_window_the_fold_scrolled_to() {
        let rows: Vec<AgentRow> = (0..8)
            .map(|i| {
                let mut r = row("%r", "s", i, 0, AgentState::Working, SINCE);
                r.agent = format!("agent{i}");
                r.pane_id = format!("%{i}");
                r
            })
            .collect();
        let mut m = WatchModel::new(rows, WidePref::Preview, NOW);
        // 32x6: one footer line, a bordered list with three visible rows.
        m.update(
            Event::Resize {
                width: 32,
                height: 6,
            },
            NOW,
            &mut (),
        );
        for _ in 0..7 {
            key(&mut m, Key::Down);
        }
        assert_eq!(m.selected_index(), 7);
        assert_eq!(m.scroll(), 5, "the window followed the selection down");

        let palette = RowPalette::default();
        let buf = render(32, 6, |f| draw(f, &m, NOW, &palette));
        let ls = lines(&buf);
        assert!(
            ls[1].contains("agent5"),
            "the window starts at row 5: {ls:?}"
        );
        assert!(
            ls[3].contains("agent7"),
            "and ends at the selection: {ls:?}"
        );
        assert!(
            !ls.iter().any(|l| l.contains("agent0")),
            "the scrolled-past rows are not drawn: {ls:?}"
        );
    }

    #[test]
    fn watch_empty_wide_titles_zero_agents_without_panic() {
        let mut m = WatchModel::new(vec![], WidePref::Preview, NOW);
        at_width(&mut m, 100);
        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &m, NOW, &palette));
        let ls = lines(&buf);
        assert!(
            ls.iter().any(|l| l.contains("agents (0)")),
            "the empty list titles zero agents: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("preview")),
            "an empty selection falls back to the preview title: {ls:?}"
        );
        assert!(
            reversed_rows(&buf).is_empty(),
            "no selection highlight with zero rows"
        );
        assert!(
            ls.iter().all(|l| !l.contains('▸')),
            "an empty model renders no group headers"
        );
    }
}
