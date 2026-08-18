//! The fuzzy picker: the default subcommand. A ratatui popup (works under `display-popup -E` or
//! inline) listing agent panes bar the one it was opened from (jumping there is a no-op), sorted
//! blocked → working → idle then time-in-state, with a nucleo fuzzy filter, a live preview of the
//! highlighted pane, and Enter-to-jump (clearing its attention flag; Esc closes). It paints a first
//! frame from stamps (stale-but-instant), then owns a 1 s refresh while open (itself a producer, so
//! each tick runs the guarded poll cycle). That tick also hot-reloads config + manifests from the
//! paths startup resolved, so an edit (new manifest, changed glyph/colour) takes effect within a
//! tick; re-parsing two small TOMLs is negligible next to the poll, so there is no mtime cache, and
//! a mid-save error is all-or-nothing (last good pair kept).
//!
//! This shell owns only the impure edges: seeding from stamps, the nudge wiring, and the draw. The
//! filter/selection/refresh/preview fold lives in [`tma_ui_core::picker::PickerModel`]; the loop and
//! effect execution live in [`crate::runner`].

use std::io;
use std::path::PathBuf;
use std::process;

use nucleo::{Config, Matcher};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, ListItem, Paragraph};
use ratatui::Frame;
use tma_runtime::config;
use tma_runtime::cycle;
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::{nudge, ui, Server, Tmux};
use tma_ui_core::layout::picker_geom;
use tma_ui_core::{Effect, Event, PickerModel, RowPalette};

use tma_ui_core::render::{branch_span, fmt_since, row_style, truncate, BRANCH_W};

use crate::dash;
use crate::runner::{self, Surface};

/// Run the picker to completion: first frame from stamps, a 1 s refresh, and on Enter a jump to the
/// highlighted agent that clears its attention flag. `config`/`manifests` are owned for hot-reload.
pub fn run_picker(
    tmux: &Tmux,
    server: &Server,
    config: config::Config,
    manifests: Vec<LoadedManifest>,
    config_path: Option<PathBuf>,
    manifest_dir: Option<PathBuf>,
    acting_client: Option<&str>,
) -> io::Result<()> {
    let mut matcher = Matcher::new(Config::DEFAULT);

    // Hide the pane the picker was opened from: jumping there is a no-op. Resolved once, through the
    // client (inside a popup `$TMUX_PANE` is the popup's hidden pane); an unresolvable pane hides
    // nothing. The refresh cycle behind the picker still stamps every pane.
    let filter = runner::RowFilter::excluding(ui::active_pane_id(tmux, acting_client));

    // First frame from stamps: instant, stale-tolerant. The next refresh runs a cycle.
    let mut first_rows = cycle::stamp_rows(tmux).unwrap_or_default();
    filter.apply(&mut first_rows);
    // Label the seed too, so the branch column is populated on the frame you actually see rather
    // than appearing a refresh later. One batched git per cold checkout, after the filter dropped
    // the rows we will not draw, and on the seed budget: this runs before the terminal is in raw
    // mode, so a slow git must cost the column rather than the popup.
    tma_runtime::repo::annotate_seed_rows(&mut first_rows);
    // Resolve the session scope from the invoking client, not the most-recently-active.
    let current_session = ui::active_session(tmux, acting_client);
    let model = PickerModel::new(first_rows, current_session, unix_now(), &mut matcher);

    // Middle-tier nudge: install the SIGUSR1 handler, then let the guard advertise our
    // pid on `$TMUX_PANE` once raw mode + the alternate screen come up, so `nudge_watchers` finds us
    // and a `clear-attention` from another pane forces an immediate refresh. Absent `$TMUX_PANE`
    // (outside tmux) advertises nothing and falls back to the 1 s self-refresh; the guard unsets the
    // pid and restores the terminal on every exit path (Drop).
    nudge::install_nudge_handler();
    let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty());
    let guard = crate::term::TerminalGuard::enter(tmux, pane, Some(process::id()))?;

    runner::run_surface(
        model,
        matcher,
        guard,
        runner::SurfaceEnv {
            tmux,
            server,
            config,
            manifests,
            config_path,
            manifest_dir,
            acting_client,
            filter,
            // The picker closes on its jump, so there is nothing left to move.
        },
    )
}

impl Surface for PickerModel {
    type Res = Matcher;

    fn update(&mut self, ev: Event, now: u64, res: &mut Matcher) -> Vec<Effect> {
        PickerModel::update(self, ev, now, res)
    }

    fn view(&self, f: &mut Frame, now: u64, palette: &RowPalette) {
        draw(f, self, now, palette);
    }
}

fn draw(f: &mut Frame, model: &PickerModel, now: u64, palette: &RowPalette) {
    // The core owns the split, because the fold hit-tests clicks against it: a popup below the
    // preview gate gives the list the whole body (45% of a narrow popup is a preview too narrow to
    // read, and the fold captures nothing for it either).
    let geom = picker_geom(f.area(), model.preview_visible());
    let (list_area, preview_area, footer_area) = (geom.list.rect, geom.preview, geom.footer);

    // Same show/hide rule as watch: the branch column appears only when a visible row resolved one.
    let show_branch = model.show_branch();
    // Agent list, flat (the fuzzy query is the way to narrow it, not grouping). No leading index:
    // the digits that once jumped to one now type into the query, and a number printed beside a row
    // would promise a shortcut that no longer exists.
    let items: Vec<ListItem> = model
        .visible_rows()
        .map(|r| {
            let (glyph, color) = row_style(palette, r);
            let time = fmt_since(now, r.since);
            let mut spans = vec![Span::styled(
                format!("{glyph} "),
                Style::default().fg(color),
            )];
            spans.extend(dash::grid_columns(&r.agent, &r.locator(), &time));
            spans.push(branch_span(r.branch(), BRANCH_W, show_branch));
            spans.push(Span::raw(format!(" {}", truncate(&r.title, 40))));
            ListItem::new(Line::from(spans))
        })
        .collect();
    dash::render_agent_list(
        f,
        list_area,
        items,
        &dash::ListSelection {
            selected: model.selected_index(),
            hovered: model.hover(),
            scroll: model.scroll(),
            count: model.visible_count(),
        },
    );

    // Preview.
    if let Some(area) = preview_area {
        let preview_title = model
            .selected_row()
            .map(|r| format!(" {} ", r.locator()))
            .unwrap_or_else(|| " preview ".to_string());
        let preview_widget = Paragraph::new(model.preview_text().clone())
            .block(Block::default().borders(Borders::ALL).title(preview_title));
        f.render_widget(preview_widget, area);
    }

    // Query / status line. Every hint here fires whatever the query holds — no key is conditional
    // any more, because none of them is a character you might want to search for.
    let scope = if model.scoped() { "session" } else { "all" };
    let prompt = Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Yellow)),
        Span::raw(model.query().to_string()),
        Span::styled(
            format!("   [scope: {scope}]  enter=jump  tab=act  ctrl-s=scope  esc=quit"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(prompt), footer_area);
}

/// Wall-clock epoch **milliseconds** (stamp grammar); [`fmt_since`] converts the age to seconds.
/// Thin alias for [`tma_runtime::now_ms`], shared with `watch`.
pub(crate) fn unix_now() -> u64 {
    tma_runtime::now_ms()
}

#[cfg(test)]
mod draw_tests {
    use super::*;
    use crate::test_render::{lines, render, reversed_rows, row, underlined_rows, with_repo};
    use tma_core::{AgentRow, AgentState};
    use tma_ui_core::{Key, Mouse, MouseKind};

    const NOW: u64 = 100_000;
    const SINCE: u64 = 95_000; // fmt_since(NOW, SINCE) == "5s"

    fn matcher() -> Matcher {
        Matcher::new(Config::DEFAULT)
    }

    /// Three same-session rows, agents distinct so each buffer row is identifiable.
    fn three_rows() -> Vec<AgentRow> {
        let mut a = row("%a", "proj", 1, 0, AgentState::Blocked, SINCE);
        a.agent = "alpha".to_string();
        let mut b = row("%b", "proj", 2, 0, AgentState::Working, SINCE);
        b.agent = "bravo".to_string();
        let mut c = row("%c", "proj", 3, 0, AgentState::Idle, SINCE);
        c.agent = "charlie".to_string();
        vec![a, b, c]
    }

    /// Drive the model the way the runner does: keys through the fold, never a field poke.
    fn key(p: &mut PickerModel, k: Key, mtch: &mut Matcher) {
        p.update(Event::Key(k), NOW, mtch);
    }

    /// A model over `rows`, seeded by the shell's initial Resize at `width` like the real popup.
    fn model(rows: Vec<AgentRow>, width: u16, mtch: &mut Matcher) -> PickerModel {
        let mut p = PickerModel::new(rows, "proj".to_string(), NOW, mtch);
        p.update(Event::Resize { width, height: 14 }, NOW, mtch);
        p
    }

    #[test]
    fn picker_renders_sorted_rows_and_highlights_selection() {
        let mut mtch = matcher();
        let mut p = model(three_rows(), 100, &mut mtch);
        key(&mut p, Key::Down, &mut mtch); // highlight the second row

        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &p, NOW, &palette));
        let ls = lines(&buf);

        // Rows in state-sorted order (blocked, working, idle).
        let a = ls
            .iter()
            .position(|l| l.contains("alpha"))
            .expect("alpha row");
        let b = ls
            .iter()
            .position(|l| l.contains("bravo"))
            .expect("bravo row");
        let c = ls
            .iter()
            .position(|l| l.contains("charlie"))
            .expect("charlie row");
        assert!(a < b && b < c, "rows in state-sorted order: {ls:?}");
        // The row opens with its state glyph, right inside the left border: no leading index, since
        // the digits that once jumped to a row now type into the query.
        for (line, agent) in [(a, "alpha"), (b, "bravo"), (c, "charlie")] {
            let after_border = ls[line].chars().nth(1).unwrap_or(' ');
            assert!(
                !after_border.is_ascii_digit(),
                "{agent}'s row must not lead with an index: {:?}",
                ls[line]
            );
        }
        // The prompt line carries the scope hint.
        assert!(
            ls.iter().any(|l| l.contains("scope: all")),
            "the scope hint renders: {ls:?}"
        );
        // The highlight lands on the second (bravo) row.
        assert_eq!(
            reversed_rows(&buf),
            vec![b],
            "selection marks the highlighted row"
        );
    }

    #[test]
    fn picker_prompt_echoes_the_typed_query() {
        let mut mtch = matcher();
        let mut p = model(three_rows(), 100, &mut mtch);
        // A query matching nothing: the prompt still echoes it verbatim while the list empties.
        for c in "zzz".chars() {
            key(&mut p, Key::Char(c), &mut mtch);
        }
        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &p, NOW, &palette));
        let ls = lines(&buf);
        assert!(
            ls.iter().any(|l| l.contains("zzz")),
            "the query renders in the prompt: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("agents (0)")),
            "the filtered-out list titles zero agents: {ls:?}"
        );
    }

    #[test]
    fn picker_branch_column_follows_the_show_branch_memo() {
        let palette = RowPalette::default();

        // No row resolves a branch: the column stays hidden.
        let mut mtch = matcher();
        let plain = model(three_rows(), 100, &mut mtch);
        assert!(!plain.show_branch());
        let buf = render(100, 14, |f| draw(f, &plain, NOW, &palette));
        assert!(
            lines(&buf).iter().all(|l| !l.contains("feature-x")),
            "no branch label while the column is hidden"
        );

        // One row carries a resolved branch: `show_branch` flips and the label renders.
        let mut rows = three_rows();
        rows[0] = with_repo(rows[0].clone(), "app", "feature-x");
        let branched = model(rows, 100, &mut mtch);
        assert!(branched.show_branch());
        let buf = render(100, 14, |f| draw(f, &branched, NOW, &palette));
        assert!(
            lines(&buf).iter().any(|l| l.contains("feature-x")),
            "the branch label renders once the column is shown"
        );
    }

    #[test]
    fn picker_preview_pane_appears_only_when_the_popup_clears_the_gate() {
        let palette = RowPalette::default();
        let mut mtch = matcher();

        // Wide: the list is bordered at 55% and the preview takes the rest.
        let wide = model(three_rows(), 100, &mut mtch);
        let buf = render(100, 14, |f| draw(f, &wide, NOW, &palette));
        let top = &lines(&buf)[0];
        assert!(
            top.contains("agents (3)") && top.matches('┐').count() == 2,
            "two bordered panes side by side: {top:?}"
        );
        assert!(
            top.contains("proj:1.0"),
            "the preview titles the highlighted pane: {top:?}"
        );

        // Narrow: one bordered pane spanning the popup, no preview title beside it.
        let narrow = model(three_rows(), 60, &mut mtch);
        assert!(!narrow.preview_visible());
        let buf = render(60, 14, |f| draw(f, &narrow, NOW, &palette));
        let ls = lines(&buf);
        assert_eq!(
            ls[0].matches('┐').count(),
            1,
            "the list takes the whole body: {:?}",
            ls[0]
        );
        assert!(
            ls[0].ends_with('┐'),
            "the list border reaches the last column: {:?}",
            ls[0]
        );
        assert!(
            !ls[0].contains("proj"),
            "no preview pane to title in a narrow popup: {:?}",
            ls[0]
        );
    }

    /// The click the fold resolved and the row the draw painted are the same one: hover the second
    /// row, click it, and the highlight lands where the pointer was.
    #[test]
    fn picker_click_and_hover_land_on_the_row_under_the_pointer() {
        let mut mtch = matcher();
        let mut p = model(three_rows(), 100, &mut mtch);
        let at = |p: &mut PickerModel, kind, row, mtch: &mut Matcher| {
            p.update(Event::Mouse(Mouse { kind, col: 4, row }), NOW, mtch);
        };
        at(&mut p, MouseKind::Moved, 2, &mut mtch);
        at(&mut p, MouseKind::Down, 2, &mut mtch);

        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &p, NOW, &palette));
        let ls = lines(&buf);
        assert!(ls[2].contains("bravo"), "the pointed row: {ls:?}");
        assert_eq!(
            underlined_rows(&buf),
            vec![2],
            "the hovered row is the one under the pointer"
        );
        assert!(
            reversed_rows(&buf).contains(&2),
            "and the click selected it: {ls:?}"
        );
    }

    #[test]
    fn picker_empty_titles_zero_agents_without_panic() {
        let mut mtch = matcher();
        let p = model(vec![], 100, &mut mtch);
        let palette = RowPalette::default();
        let buf = render(100, 14, |f| draw(f, &p, NOW, &palette));
        let ls = lines(&buf);
        assert!(
            ls.iter().any(|l| l.contains("agents (0)")),
            "the empty list titles zero agents: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("preview")),
            "the empty preview keeps its fallback title: {ls:?}"
        );
        assert!(
            reversed_rows(&buf).is_empty(),
            "no highlight with zero rows"
        );
    }
}
