//! The `watch` surface's pure fold: the sidebar's rows/selection/refresh/preview state plus the
//! width-driven layout and the full-width table builders, and the `update` that folds an `Event`
//! into them. Width is model state (`width` + `last_layout`), seeded by the shell's initial
//! Resize, so the threshold-cross cache drop is assertable without a terminal. `Res = ()`:
//! `watch` needs no scratch resource. No terminal, no tmux handle; the fold performs no I/O.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use tma_core::AgentRow;

use crate::common::{preview_fits, Common};
use crate::effect::Effect;
use crate::event::{Event, Mouse, MouseKind};
use crate::group::{display_index, display_len, group_rows, row_at_display, Group};
use crate::key::Key;
use crate::layout::watch_geom;
use crate::palette::RowPalette;
use crate::picker::sorted;
use crate::render::{
    fmt_since, row_style, truncate, truncate_locator, AGENT_W, BRANCH_W, LOCATOR_W, TIME_W,
};
use crate::selection::Selection;
use crate::view::{Click, View};

/// Fixed table column widths (chars). Agent, locator, and time reuse the shared grid constants so
/// the table lines up with the list surfaces; state, context, and model are the table-only extras.
/// `STATE_W` fits the widest realistic `state(detail)` (`blocked(permission)`); `CONTEXT_W` fits
/// `100%`.
const STATE_W: usize = 20;
const CONTEXT_W: usize = 4;
const MODEL_W: usize = 14;

/// Rows one wheel notch moves the selection. Three is the terminal convention for a wheel line
/// scroll, and the selection moves with it so the highlight never leaves the window.
const WHEEL_STEP: i32 = 3;

/// A context gauge older than this (by `@agent_context_at`) renders grey: the reading is stale (a
/// quiet pane re-stamps the gauge only when its token count changes), so the value may lag reality.
/// Display heuristic only; five minutes is well past the 1 s refresh yet short enough to flag a truly
/// idle reading.
const CONTEXT_STALE_MS: u64 = 300_000;

/// The body layout for the current frame: the terminal width gates it (a narrow pane is always
/// [`ListOnly`](WatchLayout::ListOnly)), and at or above the threshold the session-local
/// [`WidePref`] picks between the preview and the full-width table. Pure/unit-testable like
/// `Selection`; the draw fn and the capture gate both read it (only the preview arm captures).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WatchLayout {
    /// The 32-column sidebar MVP: one scrollable list, no preview, no capture.
    ListOnly,
    /// Wide enough to split the body and show a live ANSI preview beside the list.
    ListAndPreview,
    /// Wide, preview hidden: full-width status rows under a column header, the reclaimed width spent
    /// on state-detail, context, and (when stamped) model columns. No capture.
    Table,
}

/// The user's chosen wide-mode body, session-local (toggled by `p`, seeded by `--table`). Ignored
/// below [`PREVIEW_MIN_WIDTH`](crate::PREVIEW_MIN_WIDTH), where [`ListOnly`](WatchLayout::ListOnly) always wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WidePref {
    /// Live ANSI preview beside the list (the default).
    Preview,
    /// Full-width status table, no preview.
    Table,
}

impl WidePref {
    /// Flip preview ↔ table (the `p` toggle).
    fn toggled(self) -> WidePref {
        match self {
            WidePref::Preview => WidePref::Table,
            WidePref::Table => WidePref::Preview,
        }
    }
}

/// Decide the body layout from the terminal width and the wide-mode preference. Pure and total so the
/// draw fn and the capture gate read the same decision; see
/// [`PREVIEW_MIN_WIDTH`](crate::PREVIEW_MIN_WIDTH) for the threshold.
pub fn watch_layout(width: u16, pref: WidePref) -> WatchLayout {
    if !preview_fits(width) {
        WatchLayout::ListOnly
    } else {
        match pref {
            WidePref::Preview => WatchLayout::ListAndPreview,
            WidePref::Table => WatchLayout::Table,
        }
    }
}

/// One line of the wide arms' display order: a draw-only group header, or one selectable agent row.
/// The draw walks [`WatchModel::display_items`] and never sees an index, so the header lines cannot
/// drift out of step with the rows they head.
#[derive(Debug)]
pub enum DisplayItem<'a> {
    /// A `▸ repo` header line, rendered above its group's rows.
    Header(&'a str),
    /// One agent row, in display order.
    Row(&'a AgentRow),
}

/// The sidebar's fold: state-sorted rows, the highlighted index, the wide-mode preference, the
/// width-derived layout (model state, seeded by the shell's initial Resize), the refresh gate, and
/// the preview cache. Derives `Debug` so an event-script test can assert a model projection.
///
/// Every field is private: `width`, `pref`, and `last_layout` are tied by the invariant
/// `last_layout == watch_layout(width, pref)`, and `groups` holds indices into `rows`, so a single
/// assignment from outside could turn a draw into an index panic. Mutation goes through
/// [`new`](WatchModel::new) and [`update`](WatchModel::update); the draw reads the accessors below.
#[derive(Debug)]
pub struct WatchModel {
    /// The flat selectable rows. State-sorted (blocked → working → idle → unknown, then
    /// longest-in-state first); when [`grouped`](WatchModel::grouped), reordered into grouped display
    /// order (repo groups by urgency, rows state-sorted within a group). Header lines are never rows.
    rows: Vec<AgentRow>,
    sel: Selection,
    /// Whether the wide layouts group rows by repo (default true); the `g` key flips it, session-local
    /// like `pref`. Reorders `rows`; the draw interleaves the group headers.
    grouped: bool,
    /// Grouped display order, cached by `regroup` so the draw reads it instead of recomputing
    /// `group_rows` per frame. Empty when flat (`!grouped`) or when grouping yields no groups.
    groups: Vec<Group>,
    /// Whether any row resolved a non-empty branch: the list/table branch column shows iff true. A
    /// derived cache, recomputed by `recompute_columns` wherever the row set changes.
    show_branch: bool,
    /// Whether any row carries a non-empty `@agent_model`: the table model column shows iff true. A
    /// derived cache, recomputed by `recompute_columns` wherever the row set changes.
    show_model: bool,
    pref: WidePref,
    /// The last width seen (from a Resize); `last_layout` is derived from it plus `pref`.
    width: u16,
    /// The last height seen (from a Resize). With `width` it is the frame the mouse hit-test and
    /// the scroll window are computed against — the same frame the draw lays out.
    height: u16,
    /// The layout the current `width`/`pref` derive; a change drops the preview cache.
    /// Invariant: `last_layout == watch_layout(width, pref)`, re-derived only by `relayout_and_preview`.
    last_layout: WatchLayout,
    /// Scroll offset + hover + click timing, in draw-line space.
    view: View,
    /// The refresh deadline and preview cache both surfaces share.
    common: Common,
}

impl WatchModel {
    /// Seed from the first (stamp) rows; `pref` comes from `--table`, `now` arms the refresh gate.
    /// `width` starts at 0 ([`ListOnly`](WatchLayout::ListOnly)); the shell's initial Resize sets the
    /// real width before the first draw.
    pub fn new(rows: Vec<AgentRow>, pref: WidePref, now: u64) -> WatchModel {
        let mut m = WatchModel {
            rows: sorted(rows),
            sel: Selection::default(),
            grouped: true,
            groups: Vec::new(),
            show_branch: false,
            show_model: false,
            pref,
            width: 0,
            height: 0,
            last_layout: watch_layout(0, pref),
            view: View::default(),
            common: Common::new(now),
        };
        m.regroup();
        m.recompute_columns();
        m
    }

    /// Fold one event into the model, returning the effects the shell executes.
    pub fn update(&mut self, ev: Event, now: u64, _res: &mut ()) -> Vec<Effect> {
        if let Some(fx) = self.common.update(&ev, now) {
            return fx;
        }
        match ev {
            Event::Key(k) => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m, now),
            // Width is model state: record it, then re-derive the layout through the shared
            // threshold-cross path (also the `p` toggle's path), which drops the cache on a change.
            // Height rides along for the scroll window and the hit-test.
            Event::Resize { width, height } => {
                self.width = width;
                self.height = height;
                self.relayout_and_preview()
            }
            Event::RowsRefreshed(rows) => {
                self.set_rows(rows);
                // Force a re-capture against the (reanchored) selection on every successful tick, as
                // the watch preview is live; the capture gate skips it when narrow.
                self.common.forget_target();
                self.preview_effect()
            }
            // Handled by `Common::update` above; spelled out so a new variant must be routed
            // deliberately rather than falling into a silent no-op.
            Event::Tick | Event::Nudge | Event::RefreshFailed | Event::PreviewCaptured { .. } => {
                vec![]
            }
        }
    }

    fn on_key(&mut self, k: Key) -> Vec<Effect> {
        match k {
            // q/Esc/Ctrl-C quit; a plain `Quit` batch, nothing to defer (no jump rides with it).
            Key::Char('q') | Key::Esc | Key::CtrlC => vec![Effect::Quit],
            // Enter jumps but keeps the sidebar open (persistent, non-modal): `[Focus, ClearAttention]`
            // with NO `Quit`, so the runner runs them inline and the loop continues (watch.rs:207-215).
            Key::Enter => match self.selected_row() {
                Some(r) => vec![
                    Effect::Focus(Box::new(r.clone())),
                    Effect::ClearAttention {
                        pane: r.pane_id.clone(),
                    },
                ],
                None => vec![],
            },
            // `a` opens the action menu on the HIGHLIGHTED pane, not the pane the sidebar lives in:
            // triaging N blocked agents from here is the point, and jumping to each first is not.
            // The menu is tmux's own overlay, so the sidebar neither draws it nor closes for it.
            Key::Char('a') => match self.selected_row() {
                Some(r) => vec![Effect::ActMenu {
                    pane: r.pane_id.clone(),
                }],
                None => vec![],
            },
            // k/j arrive as `Char`; move, then re-capture iff the wide preview is showing.
            Key::Up | Key::Char('k') => {
                self.move_by(-1);
                self.preview_effect()
            }
            Key::Down | Key::Char('j') => {
                self.move_by(1);
                self.preview_effect()
            }
            // `p` flips the wide-mode body; it is a layout change, so it routes through the same
            // threshold-cross path as Resize (one drop-and-recapture function, not two copies).
            Key::Char('p') => {
                self.pref = self.pref.toggled();
                self.relayout_and_preview()
            }
            // `g` flips grouped ↔ flat: reorder `rows` and reanchor the selection to its pane. The
            // selected pane is unchanged, so the preview (keyed by pane id) stays valid — no capture.
            Key::Char('g') => {
                self.grouped = !self.grouped;
                self.reorder_rows();
                vec![]
            }
            _ => vec![],
        }
    }

    /// Fold one mouse report. Hover just moves a highlight (no effects, so a pointer crossing the
    /// sidebar costs nothing but a redraw); a press selects the row it landed on, and a second
    /// press on that same row jumps, the mouse spelling of "highlight, then Enter". The wheel moves
    /// the selection rather than the window, so the highlight can never scroll out of sight.
    fn on_mouse(&mut self, m: Mouse, now: u64) -> Vec<Effect> {
        let line = self.line_at(m.col, m.row);
        match m.kind {
            MouseKind::Moved => {
                // Only a *row* highlights: group headers and the empty space below the list are
                // not selectable, so hovering them clears rather than highlights.
                let hovered = line.filter(|&l| self.row_at_line(l).is_some());
                self.view.set_hover(hovered);
                vec![]
            }
            MouseKind::Down => {
                let Some(row) = line.and_then(|l| self.row_at_line(l)) else {
                    return vec![]; // a header, the border, the preview, the footer
                };
                let click = self.view.click(line.unwrap_or(row), now);
                self.sel.index = row;
                self.sync_view();
                match click {
                    // The sidebar is non-modal, so a jump keeps it open — exactly what Enter does.
                    Click::Double => match self.selected_row() {
                        Some(r) => vec![
                            Effect::Focus(Box::new(r.clone())),
                            Effect::ClearAttention {
                                pane: r.pane_id.clone(),
                            },
                        ],
                        None => vec![],
                    },
                    Click::Single => self.preview_effect(),
                }
            }
            MouseKind::ScrollUp => {
                self.step(-WHEEL_STEP);
                self.preview_effect()
            }
            MouseKind::ScrollDown => {
                self.step(WHEEL_STEP);
                self.preview_effect()
            }
        }
    }

    /// The draw line under a point, `None` when the point is outside the list.
    fn line_at(&self, col: u16, row: u16) -> Option<usize> {
        watch_geom(self.area(), self.last_layout)
            .list
            .index_at(self.view.scroll(), col, row)
            .filter(|&l| l < self.draw_len())
    }

    /// The row a draw line holds: itself in the flat narrow arm, the header-aware mapping in the
    /// grouped wide arms, and `None` for a header line.
    fn row_at_line(&self, line: usize) -> Option<usize> {
        if self.uses_headers() {
            row_at_display(&self.groups, self.rows.len(), line)
        } else {
            (line < self.rows.len()).then_some(line)
        }
    }

    /// Whether the current arm interleaves `▸ repo` header lines. The narrow arm never does — it
    /// draws the flat rows — so its draw space is the row space.
    fn uses_headers(&self) -> bool {
        self.last_layout != WatchLayout::ListOnly && !self.groups.is_empty()
    }

    /// Move the selection without wrapping (the wheel's semantics; `j`/`k` still wrap).
    fn step(&mut self, delta: i32) {
        self.sel.step(self.rows.len(), delta);
        self.sync_view();
    }

    /// Re-derive the scroll window from the current selection, list length, and viewport. Every
    /// selection or layout change runs through here so the highlight is always on screen.
    fn sync_view(&mut self) {
        let viewport = watch_geom(self.area(), self.last_layout).list.viewport();
        self.view
            .sync(self.draw_len(), viewport, self.draw_selection());
    }

    /// Re-derive the layout from `width`/`pref`; on a change drop the stale preview cache, then
    /// request a capture iff the new layout shows the preview. The single home of the threshold-cross
    /// cache-drop, shared by Resize and the `p` toggle.
    fn relayout_and_preview(&mut self) -> Vec<Effect> {
        let layout = watch_layout(self.width, self.pref);
        if layout != self.last_layout {
            self.last_layout = layout;
            self.common.drop_preview();
        }
        // The arm and the frame both decide the scroll window, so re-derive it here too: a resize
        // or a `p` toggle can leave the highlight outside a window sized for the old one.
        self.sync_view();
        self.preview_effect()
    }

    /// Capture the highlighted pane's preview iff the wide preview is showing. Narrow/table layouts
    /// emit nothing (zero tmux calls, matching the MVP).
    fn preview_effect(&mut self) -> Vec<Effect> {
        if self.last_layout != WatchLayout::ListAndPreview {
            return vec![];
        }
        let sel_pane = self.selected_row().map(|r| r.pane_id.clone());
        self.common.capture(sel_pane)
    }

    /// Replace the rows (a refresh), preserving the highlighted pane by id when it survives the reorder.
    fn set_rows(&mut self, rows: Vec<AgentRow>) {
        let anchor = self.selected_row().map(|r| r.pane_id.clone());
        self.rows = sorted(rows);
        self.regroup();
        self.recompute_columns();
        let ids: Vec<&str> = self.rows.iter().map(|r| r.pane_id.as_str()).collect();
        self.sel.reanchor(&ids, anchor.as_deref());
        self.sync_view();
    }

    /// Recompute the `show_branch`/`show_model` column caches from the current row set. Both predicates
    /// scan the flat `rows`, so grouping and order never affect them; called wherever the rows change.
    fn recompute_columns(&mut self) {
        self.show_branch = self
            .rows
            .iter()
            .any(|r| r.branch().is_some_and(|b| !b.is_empty()));
        self.show_model = self
            .rows
            .iter()
            .any(|r| r.model.as_deref().is_some_and(|m| !m.is_empty()));
    }

    /// Re-sort `rows` into flat state order then, when grouped, into grouped display order, and
    /// reanchor the selection to its pane. Used by the `g` toggle (the rows are already sorted within
    /// their current arrangement, so a plain re-sort restores flat order before regrouping).
    fn reorder_rows(&mut self) {
        let anchor = self.selected_row().map(|r| r.pane_id.clone());
        self.rows = sorted(std::mem::take(&mut self.rows));
        self.regroup();
        let ids: Vec<&str> = self.rows.iter().map(|r| r.pane_id.as_str()).collect();
        self.sel.reanchor(&ids, anchor.as_deref());
        self.sync_view();
    }

    /// When grouped, reorder the (already state-sorted) `rows` into grouped display order: groups by
    /// urgency, members in their state-sorted order. A no-op when flat. `rows` stays the flat
    /// selectable vec; group headers live only in the draw.
    fn regroup(&mut self) {
        if !self.grouped {
            self.groups = Vec::new();
            return;
        }
        // Reorder `rows` into grouped display order and cache each group with its now-contiguous
        // member range, so the draw reads `groups()` instead of recomputing `group_rows` per frame.
        let groups = group_rows(&self.rows);
        let mut rows = Vec::with_capacity(self.rows.len());
        let mut cached = Vec::with_capacity(groups.len());
        for g in groups {
            let start = rows.len();
            for i in g.members {
                rows.push(self.rows[i].clone());
            }
            cached.push(Group {
                name: g.name,
                members: (start..rows.len()).collect(),
            });
        }
        self.rows = rows;
        self.groups = cached;
    }

    /// The highlighted row, or `None` when the list is empty.
    pub fn selected_row(&self) -> Option<&AgentRow> {
        self.rows.get(self.sel.index)
    }

    fn move_by(&mut self, delta: i32) {
        self.sel.move_by(self.rows.len(), delta);
        self.sync_view();
    }

    // --- draw accessors -------------------------------------------------------------------------

    /// The body layout the current width and preference derive; the draw picks its arm from it.
    pub fn layout(&self) -> WatchLayout {
        self.last_layout
    }

    /// Whether the wide arms group rows by repo (the `g` toggle); the footer names the flip.
    pub fn grouped(&self) -> bool {
        self.grouped
    }

    /// Whether any row resolved a branch, so the list/table spends a column on it.
    pub fn show_branch(&self) -> bool {
        self.show_branch
    }

    /// Whether any row carries `@agent_model`, so the table spends a column on it.
    pub fn show_model(&self) -> bool {
        self.show_model
    }

    /// The selectable row count: the list title's `agents (N)`, and zero means no highlight.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The flat selectable rows in display order (the narrow arm, which renders no headers).
    pub fn rows(&self) -> impl ExactSizeIterator<Item = &AgentRow> {
        self.rows.iter()
    }

    /// The highlighted row's index among [`rows`](Self::rows) — the narrow arm's list index.
    pub fn selected_index(&self) -> usize {
        self.sel.index
    }

    /// The frame the fold last saw (from `Resize`) — what the draw lays out and the mouse hit-test
    /// measures against.
    pub fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// The first visible draw line: handed to the list widget so the window the draw paints is the
    /// window the hit-test assumed.
    pub fn scroll(&self) -> usize {
        self.view.scroll()
    }

    /// The draw line the pointer is over, `None` when it is elsewhere. Rendered dimmer than the
    /// selection: it says "this is what a click would take", not "this is current".
    pub fn hover(&self) -> Option<usize> {
        self.view.hover()
    }

    /// The highlighted line in the current arm's draw space: the flat row index in the narrow arm,
    /// the header-shifted index in the grouped wide arms.
    pub fn draw_selection(&self) -> usize {
        if self.uses_headers() {
            display_index(&self.groups, self.sel.index)
        } else {
            self.sel.index
        }
    }

    /// How many lines the current arm draws (rows, plus a header per group where they show).
    pub fn draw_len(&self) -> usize {
        if self.uses_headers() {
            display_len(&self.groups, self.rows.len())
        } else {
            self.rows.len()
        }
    }

    /// The wide arms' display order: when grouped, a header above each group's rows; when flat, the
    /// rows alone. Yields borrowed rows, so the draw never indexes across `rows` and `groups`.
    pub fn display_items(&self) -> impl Iterator<Item = DisplayItem<'_>> {
        let flat: &[AgentRow] = if self.grouped { &[] } else { &self.rows };
        let groups: &[Group] = if self.grouped { &self.groups } else { &[] };
        flat.iter()
            .map(DisplayItem::Row)
            .chain(groups.iter().flat_map(move |g| {
                std::iter::once(DisplayItem::Header(&g.name))
                    .chain(g.members.iter().map(|&i| DisplayItem::Row(&self.rows[i])))
            }))
    }

    /// The cached preview text for the highlighted pane (empty until a capture lands).
    pub fn preview_text(&self) -> &Text<'static> {
        self.common.preview_text()
    }

    /// The pane the cached preview was captured for; the fold tests read the recapture gate here.
    #[cfg(test)]
    fn preview_target(&self) -> Option<&str> {
        self.common.preview_target()
    }
}

/// The table header, column labels padded to the same widths as [`table_row`] so they line up. The
/// leading two blanks stand in for the glyph column. The `branch` label sits next to `where` (both
/// answer "where the pane is"), before the free-width title; it appears only when `show_branch`.
pub fn table_header(show_model: bool, show_branch: bool) -> Line<'static> {
    let mut cols = format!("  {:<aw$} ", truncate("agent", AGENT_W), aw = AGENT_W);
    if show_model {
        cols.push_str(&format!(
            "{:<mw$} ",
            truncate("model", MODEL_W),
            mw = MODEL_W
        ));
    }
    cols.push_str(&format!(
        "{:<sw$} {:>cw$} {:>tw$} {:<lw$} ",
        truncate("state", STATE_W),
        "ctx",
        "time",
        truncate("where", LOCATOR_W),
        sw = STATE_W,
        cw = CONTEXT_W,
        tw = TIME_W,
        lw = LOCATOR_W,
    ));
    if show_branch {
        cols.push_str(&format!(
            "{:<bw$} ",
            truncate("branch", BRANCH_W),
            bw = BRANCH_W
        ));
    }
    cols.push_str("title");
    Line::from(Span::styled(cols, Style::default().fg(Color::DarkGray)))
}

/// Width the title column gets after the fixed columns: the body width minus every leading cell
/// (each padded field carries a trailing space). Floors at zero so a too-narrow frame just clips.
pub fn table_title_width(width: u16, show_model: bool, show_branch: bool) -> usize {
    // glyph(2) + agent + state + ctx + time + where, each `+1` for the trailing space.
    let mut fixed =
        2 + (AGENT_W + 1) + (STATE_W + 1) + (CONTEXT_W + 1) + (TIME_W + 1) + (LOCATOR_W + 1);
    if show_model {
        fixed += MODEL_W + 1;
    }
    if show_branch {
        fixed += BRANCH_W + 1;
    }
    (width as usize).saturating_sub(fixed)
}

/// One full-width table row: glyph, agent, (model), state-with-detail, context gauge, time, locator,
/// (branch), title. Every column but the title is fixed-width so they align down the table; the title
/// clips to `title_w`. A `blocked` pane with a `permission` detail renders `blocked(permission)`; an
/// absent context gauge is a blank cell, a stale one is grey; an unresolved branch is a blank cell.
pub fn table_row(
    palette: &RowPalette,
    r: &AgentRow,
    now: u64,
    show_model: bool,
    show_branch: bool,
    title_w: usize,
) -> Line<'static> {
    let (glyph, color) = row_style(palette, r);
    let time = fmt_since(now, r.since);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{:<w$} ", truncate(&r.agent, AGENT_W), w = AGENT_W),
            Style::default().fg(Color::Cyan),
        ),
    ];
    if show_model {
        let model = r.model.as_deref().unwrap_or("");
        spans.push(Span::raw(format!(
            "{:<w$} ",
            truncate(model, MODEL_W),
            w = MODEL_W
        )));
    }
    spans.push(Span::styled(
        format!("{:<w$} ", truncate(&state_label(r), STATE_W), w = STATE_W),
        Style::default().fg(color),
    ));
    spans.push(context_span(r, now));
    spans.push(Span::styled(
        format!("{time:>w$} ", w = TIME_W),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::raw(format!(
        "{:<w$} ",
        truncate_locator(&r.locator(), LOCATOR_W),
        w = LOCATOR_W
    )));
    if show_branch {
        let branch = r.branch().unwrap_or("");
        spans.push(Span::styled(
            format!("{:<w$} ", truncate(branch, BRANCH_W), w = BRANCH_W),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(truncate(&r.title, title_w)));
    Line::from(spans)
}

/// The state cell text: the state token, plus `(detail)` when a detail is present
/// (`blocked` + `permission` → `blocked(permission)`).
fn state_label(r: &AgentRow) -> String {
    match &r.detail {
        Some(d) => format!("{}({})", r.state.token(), d),
        None => r.state.token().to_string(),
    }
}

/// The context-gauge cell, right-aligned in [`CONTEXT_W`] with a trailing space: `78%` when covered
/// (grey once [`CONTEXT_STALE_MS`] stale), a blank cell when the pane carries no gauge.
fn context_span(r: &AgentRow, now: u64) -> Span<'static> {
    match r.context_pct {
        None => Span::raw(format!("{:>w$} ", "", w = CONTEXT_W)),
        Some(pct) => {
            let stale = r
                .context_at
                .is_some_and(|at| now.saturating_sub(at) >= CONTEXT_STALE_MS);
            let style = if stale {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Span::styled(format!("{:>w$} ", format!("{pct}%"), w = CONTEXT_W), style)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PREVIEW_MIN_WIDTH;
    use tma_core::{AgentState, RepoLabel};

    fn row(session: &str, w: u32, p: u32, state: AgentState, since: u64) -> AgentRow {
        AgentRow {
            pane_id: format!("%{w}{p}"),
            agent: "claude".to_string(),
            state,
            detail: None,
            since,
            session: session.to_string(),
            window_index: w,
            pane_index: p,
            title: "t".to_string(),
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

    /// A row with a resolved repo/branch, for the grouping tests.
    fn repo_row(
        w: u32,
        p: u32,
        repo: &str,
        branch: &str,
        state: AgentState,
        since: u64,
    ) -> AgentRow {
        AgentRow {
            repo: Some(RepoLabel {
                name: repo.to_string(),
                branch: branch.to_string(),
                worktree: false,
            }),
            ..row("s", w, p, state, since)
        }
    }

    /// Two rows (blocked first after the sort), for the selection/preview scripts.
    fn two_rows() -> Vec<AgentRow> {
        vec![
            row("a", 0, 0, AgentState::Blocked, 10),
            row("a", 0, 1, AgentState::Working, 10),
        ]
    }

    // --- mouse --------------------------------------------------------------------------------

    /// Six rows, so the list outgrows a short frame and the scroll window matters.
    fn six_rows() -> Vec<AgentRow> {
        (0..6)
            .map(|p| row("a", 0, p, AgentState::Working, 10 + p as u64))
            .collect()
    }

    /// A mouse event at a point, folded like the runner would.
    fn mouse(m: &mut WatchModel, kind: MouseKind, col: u16, row: u16, now: u64) -> Vec<Effect> {
        m.update(Event::Mouse(Mouse { kind, col, row }), now, &mut ())
    }

    /// The narrow sidebar at 32x10: a bordered list whose first row is screen row 1.
    fn narrow(rows: Vec<AgentRow>) -> WatchModel {
        let mut m = WatchModel::new(rows, WidePref::Preview, 0);
        m.update(
            Event::Resize {
                width: 32,
                height: 10,
            },
            0,
            &mut (),
        );
        m
    }

    #[test]
    fn a_click_selects_the_row_under_the_pointer() {
        let mut m = narrow(six_rows());
        assert_eq!(m.selected_index(), 0);
        // Screen row 3 is the third list row (row 0 is the border).
        let fx = mouse(&mut m, MouseKind::Down, 5, 3, 1_000);
        assert_eq!(m.selected_index(), 2);
        assert!(fx.is_empty(), "the narrow arm captures no preview: {fx:?}");
        // The border, and the empty space past the last row, select nothing.
        mouse(&mut m, MouseKind::Down, 5, 0, 2_000);
        assert_eq!(m.selected_index(), 2, "the top border is not a row");
        mouse(&mut m, MouseKind::Down, 5, 8, 3_000);
        assert_eq!(m.selected_index(), 2, "past the last row is not a row");
    }

    #[test]
    fn a_second_click_on_the_selected_row_jumps_without_closing() {
        let mut m = narrow(six_rows());
        mouse(&mut m, MouseKind::Down, 5, 2, 1_000);
        let pane = m.selected_row().unwrap().pane_id.clone();
        let fx = mouse(&mut m, MouseKind::Down, 5, 2, 1_200);
        assert!(
            matches!(
                fx.as_slice(),
                [Effect::Focus(r), Effect::ClearAttention { pane: p }]
                    if r.pane_id == pane && *p == pane
            ),
            "the double-click jumps like Enter, got {fx:?}"
        );
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Quit)),
            "the sidebar is non-modal: a jump leaves it open"
        );
    }

    #[test]
    fn hover_tracks_rows_and_clears_off_them() {
        let mut m = narrow(six_rows());
        assert_eq!(m.hover(), None, "nothing hovered before the pointer moves");
        mouse(&mut m, MouseKind::Moved, 5, 2, 0);
        assert_eq!(m.hover(), Some(1));
        mouse(&mut m, MouseKind::Moved, 5, 9, 0);
        assert_eq!(m.hover(), None, "the footer is not a row");
        // Hover never selects: that is what a click is for.
        assert_eq!(m.selected_index(), 0);
    }

    #[test]
    fn the_wheel_moves_the_selection_and_stops_at_the_ends() {
        let mut m = narrow(six_rows());
        mouse(&mut m, MouseKind::ScrollDown, 5, 3, 0);
        assert_eq!(m.selected_index(), 3, "one notch is three rows");
        mouse(&mut m, MouseKind::ScrollDown, 5, 3, 0);
        assert_eq!(m.selected_index(), 5, "the last row holds — no wrap");
        mouse(&mut m, MouseKind::ScrollUp, 5, 3, 0);
        assert_eq!(m.selected_index(), 2);
        mouse(&mut m, MouseKind::ScrollUp, 5, 3, 0);
        mouse(&mut m, MouseKind::ScrollUp, 5, 3, 0);
        assert_eq!(m.selected_index(), 0, "and so does the first");
    }

    /// The scroll window is the fold's, so a click after scrolling resolves through the same offset
    /// the draw painted with — the bug this test exists to prevent is an off-by-`scroll` selection.
    #[test]
    fn a_click_after_scrolling_reads_through_the_same_offset() {
        // 32x7: one footer line, then a bordered list whose interior is four rows, over six rows.
        let mut m = WatchModel::new(six_rows(), WidePref::Preview, 0);
        m.update(
            Event::Resize {
                width: 32,
                height: 7,
            },
            0,
            &mut (),
        );
        assert_eq!(m.scroll(), 0);
        mouse(&mut m, MouseKind::ScrollDown, 5, 2, 0);
        assert_eq!(m.selected_index(), 3);
        assert_eq!(m.scroll(), 0, "row 3 is the last one already visible");
        mouse(&mut m, MouseKind::ScrollDown, 5, 2, 0);
        assert_eq!(
            (m.selected_index(), m.scroll()),
            (5, 2),
            "the window follows the selection to the end of the list"
        );
        // Screen row 1 is now the third row of the list, because the window starts at index 2.
        mouse(&mut m, MouseKind::Down, 5, 1, 1_000);
        assert_eq!(m.selected_index(), 2);
    }

    #[test]
    fn a_click_on_a_group_header_selects_nothing() {
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        m.update(
            Event::Resize {
                width: 120,
                height: 20,
            },
            0,
            &mut (),
        );
        // The table arm is borderless under a one-line column header: draw line 0 is screen row 1,
        // and it is the `▸ app` group header.
        mouse(&mut m, MouseKind::Down, 5, 1, 1_000);
        assert_eq!(m.selected_index(), 0, "a header click changes nothing");
        assert_eq!(m.hover(), None);
        mouse(&mut m, MouseKind::Moved, 5, 1, 1_000);
        assert_eq!(m.hover(), None, "and it does not highlight either");
        // The line under it is the group's first row.
        mouse(&mut m, MouseKind::Down, 5, 2, 2_000);
        assert_eq!(m.selected_row().unwrap().pane_id, "%00");
        // Two headers and two app rows precede the lib row: draw line 4, screen row 5.
        mouse(&mut m, MouseKind::Down, 5, 5, 3_000);
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
        assert_eq!(m.draw_selection(), 4);
    }

    // --- new update coverage (width as model state) ---------------------------------------------

    #[test]
    fn preview_cache_drop_on_layout_threshold_cross() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        // Widen into the preview: one capture for the highlighted pane, which we let land.
        let fx = m.update(
            Event::Resize {
                width: 100,
                height: 40,
            },
            0,
            &mut (),
        );
        let pane = m.selected_row().unwrap().pane_id.clone();
        assert!(
            matches!(fx.as_slice(), [Effect::CapturePreview { pane: p }] if *p == pane),
            "widening into the preview captures, got {fx:?}"
        );
        m.update(
            Event::PreviewCaptured {
                pane: pane.clone(),
                ansi: "hello".to_string(),
            },
            0,
            &mut (),
        );
        assert_eq!(m.preview_target(), Some(pane.as_str()));
        assert!(!m.preview_text().lines.is_empty(), "cache populated");
        // Cross back below the threshold: the cache drops and nothing is captured (narrow).
        let fx = m.update(
            Event::Resize {
                width: 40,
                height: 40,
            },
            0,
            &mut (),
        );
        assert_eq!(m.layout(), WatchLayout::ListOnly);
        assert!(
            m.preview_target().is_none(),
            "threshold cross drops the target"
        );
        assert!(
            m.preview_text().lines.is_empty(),
            "threshold cross drops the text"
        );
        assert!(fx.is_empty(), "narrow emits no capture");
    }

    #[test]
    fn resize_below_threshold_forces_listonly() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        let fx = m.update(
            Event::Resize {
                width: PREVIEW_MIN_WIDTH - 1,
                height: 40,
            },
            0,
            &mut (),
        );
        assert_eq!(m.layout(), WatchLayout::ListOnly);
        assert!(fx.is_empty(), "a sub-threshold resize captures nothing");
    }

    #[test]
    fn wide_pref_toggle_flips_and_rederives() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        m.update(
            Event::Resize {
                width: 100,
                height: 40,
            },
            0,
            &mut (),
        );
        let pane = m.selected_row().unwrap().pane_id.clone();
        m.update(
            Event::PreviewCaptured {
                pane: pane.clone(),
                ansi: String::new(),
            },
            0,
            &mut (),
        );
        // `p` flips to the table: layout changes, cache drops, no capture (table has no preview).
        let fx = m.update(Event::Key(Key::Char('p')), 0, &mut ());
        assert_eq!(m.pref, WidePref::Table);
        assert_eq!(m.layout(), WatchLayout::Table);
        assert!(m.preview_target().is_none(), "flip drops the cache");
        assert!(fx.is_empty(), "the table emits no capture");
        // `p` flips back onto the preview: it re-captures for the current selection.
        let fx = m.update(Event::Key(Key::Char('p')), 0, &mut ());
        assert_eq!(m.pref, WidePref::Preview);
        assert_eq!(m.layout(), WatchLayout::ListAndPreview);
        assert!(
            matches!(fx.as_slice(), [Effect::CapturePreview { pane: p }] if *p == pane),
            "landing back on the preview re-captures, got {fx:?}"
        );
    }

    #[test]
    fn watch_enter_focuses_inline_no_quit() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        let pane = m.selected_row().unwrap().pane_id.clone();
        let fx = m.update(Event::Key(Key::Enter), 0, &mut ());
        assert!(
            matches!(
                fx.as_slice(),
                [Effect::Focus(_), Effect::ClearAttention { pane: cp }] if *cp == pane
            ),
            "Enter yields [Focus, ClearAttention], got {fx:?}"
        );
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Quit)),
            "no Quit: the sidebar stays open (non-modal)"
        );
    }

    #[test]
    fn watch_a_opens_the_action_menu_on_the_selected_pane() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        m.sel.index = 1;
        let pane = m.selected_row().unwrap().pane_id.clone();
        let fx = m.update(Event::Key(Key::Char('a')), 0, &mut ());
        assert!(
            matches!(fx.as_slice(), [Effect::ActMenu { pane: p }] if *p == pane),
            "`a` yields one ActMenu for the highlighted pane, got {fx:?}"
        );
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Quit)),
            "the sidebar stays open behind the menu"
        );
        // An empty list has nothing to act on: no effect, no panic.
        let mut empty = WatchModel::new(vec![], WidePref::Preview, 0);
        assert!(empty
            .update(Event::Key(Key::Char('a')), 0, &mut ())
            .is_empty());
    }

    #[test]
    fn watch_recaptures_preview_after_refresh() {
        let mut m = WatchModel::new(two_rows(), WidePref::Preview, 0);
        m.update(
            Event::Resize {
                width: 100,
                height: 40,
            },
            0,
            &mut (),
        );
        let sel = m.selected_row().unwrap().pane_id.clone();
        m.update(
            Event::PreviewCaptured {
                pane: sel.clone(),
                ansi: String::new(),
            },
            0,
            &mut (),
        );
        assert_eq!(m.preview_target(), Some(sel.as_str()));
        // A successful refresh (rows reordered) reanchors the selection to the same pane and forces a
        // fresh capture against it, even though the highlighted pane never changed (watch.rs:241-244).
        let fx = m.update(
            Event::RowsRefreshed(vec![
                row("a", 0, 1, AgentState::Working, 10),
                row("a", 0, 0, AgentState::Blocked, 10),
            ]),
            0,
            &mut (),
        );
        assert_eq!(
            m.selected_row().unwrap().pane_id,
            sel,
            "the selection follows its pane across the reorder"
        );
        assert!(
            matches!(fx.as_slice(), [Effect::CapturePreview { pane: p }] if *p == sel),
            "a refresh re-captures the live preview, got {fx:?}"
        );
    }

    // --- ports of the watch_layout suite --------------------------------------------------------

    #[test]
    fn watch_layout_pins_the_preview_threshold() {
        // One column below the threshold stays the single-list MVP; exactly at it, the wide body
        // appears (the `>=` edge). The default preference is the preview.
        let p = WidePref::Preview;
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH - 1, p),
            WatchLayout::ListOnly
        );
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH, p),
            WatchLayout::ListAndPreview
        );
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH + 40, p),
            WatchLayout::ListAndPreview
        );
        // A degenerate zero-width frame never tries to split.
        assert_eq!(watch_layout(0, p), WatchLayout::ListOnly);
    }

    #[test]
    fn watch_layout_wide_pref_picks_table_narrow_always_list() {
        // At or above the threshold the preference decides preview vs table.
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH, WidePref::Table),
            WatchLayout::Table
        );
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH, WidePref::Preview),
            WatchLayout::ListAndPreview
        );
        // Below the threshold the narrow fallback wins regardless of preference.
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH - 1, WidePref::Table),
            WatchLayout::ListOnly
        );
        assert_eq!(
            watch_layout(PREVIEW_MIN_WIDTH - 1, WidePref::Preview),
            WatchLayout::ListOnly
        );
    }

    #[test]
    fn wide_pref_toggle_flips_preview_and_table() {
        assert_eq!(WidePref::Preview.toggled(), WidePref::Table);
        assert_eq!(WidePref::Table.toggled(), WidePref::Preview);
    }

    // --- table row builder, ported -------------------------------------------------------------

    /// Concatenated text of a rendered line.
    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Char width of every column but the trailing title, for the alignment assertions.
    fn prefix_width(l: &Line) -> usize {
        l.spans[..l.spans.len() - 1]
            .iter()
            .map(|s| s.content.chars().count())
            .sum()
    }

    #[test]
    fn table_columns_align_and_render_detail_and_context() {
        let styles = RowPalette::default();
        let now = 1_000_000;
        let mut blocked = row("a", 0, 0, AgentState::Blocked, now - 5_000);
        blocked.detail = Some("permission".to_string());
        blocked.context_pct = Some(78);
        blocked.context_at = Some(now - 1_000);
        // A long session name and no context/detail: the fixed columns must still line up.
        let idle = row("verylongsessionname", 1, 2, AgentState::Idle, now - 90_000);

        let b = table_row(&styles, &blocked, now, false, false, 10);
        let i = table_row(&styles, &idle, now, false, false, 10);
        assert_eq!(
            prefix_width(&b),
            prefix_width(&i),
            "fixed columns align across mixed rows"
        );

        let bt = line_text(&b);
        assert!(
            bt.contains("blocked(permission)"),
            "state cell shows the detail"
        );
        assert!(bt.contains("78%"), "context gauge shows the percent");
        assert!(
            !line_text(&i).contains('%'),
            "an uncovered pane leaves a blank context cell"
        );
    }

    #[test]
    fn table_model_column_appears_only_when_shown() {
        let styles = RowPalette::default();
        let now = 1_000_000;
        let mut r = row("a", 0, 0, AgentState::Working, now - 1_000);
        r.model = Some("gpt-5.6".to_string());

        let without = table_row(&styles, &r, now, false, false, 10);
        let with = table_row(&styles, &r, now, true, false, 10);
        assert!(
            !line_text(&without).contains("gpt-5.6"),
            "hidden model column is absent"
        );
        assert!(
            line_text(&with).contains("gpt-5.6"),
            "shown model column carries the label"
        );
        assert_eq!(
            prefix_width(&with) - prefix_width(&without),
            MODEL_W + 1,
            "the model column adds exactly its fixed width"
        );
    }

    #[test]
    fn context_span_absent_present_and_stale() {
        let now = 1_000_000_000;
        let mut r = row("a", 0, 0, AgentState::Idle, now);
        // Absent gauge: a blank, fixed-width cell with no percent.
        assert!(!context_span(&r, now).content.contains('%'));
        // Fresh gauge: rendered, not greyed.
        r.context_pct = Some(42);
        r.context_at = Some(now - 1_000);
        let fresh = context_span(&r, now);
        assert!(fresh.content.contains("42%"));
        assert_ne!(
            fresh.style.fg,
            Some(Color::DarkGray),
            "a fresh gauge is not greyed"
        );
        // Stale gauge: greyed.
        r.context_at = Some(now - CONTEXT_STALE_MS);
        assert_eq!(
            context_span(&r, now).style.fg,
            Some(Color::DarkGray),
            "a stale gauge greys"
        );
    }

    // --- grouping -------------------------------------------------------------------------------

    /// Three panes across two repos, urgency-ordered so the grouped and flat orders differ (the app
    /// group's idle pane jumps up next to its blocked pane under grouping).
    fn grouped_fixture() -> Vec<AgentRow> {
        vec![
            repo_row(0, 0, "app", "main", AgentState::Blocked, 5),
            repo_row(0, 1, "lib", "main", AgentState::Working, 8),
            repo_row(0, 2, "app", "wt", AgentState::Idle, 30),
        ]
    }

    /// The display order as flat tokens: `▸name` per header, the pane id per row.
    fn display_shape(m: &WatchModel) -> Vec<String> {
        m.display_items()
            .map(|it| match it {
                DisplayItem::Header(name) => format!("▸{name}"),
                DisplayItem::Row(r) => r.pane_id.clone(),
            })
            .collect()
    }

    #[test]
    fn new_seeds_grouped_display_order() {
        // Grouped by default: the blocked-led `app` group first (its two panes contiguous), `lib`
        // after. The flat sort would interleave them (blocked-app, working-lib, idle-app).
        let m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        assert!(m.grouped());
        let ids: Vec<&str> = m.rows().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["%00", "%02", "%01"]);
    }

    #[test]
    fn g_toggle_round_trips_preserving_the_selected_pane() {
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        // Select the lib pane (grouped index 2, but flat-sorted index 1): the round trip must keep it.
        m.sel.index = 2;
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
        // Flip to flat: pure state sort, selection follows its pane to index 1.
        m.update(Event::Key(Key::Char('g')), 0, &mut ());
        assert!(!m.grouped());
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
        assert_eq!(m.sel.index, 1, "flat sort places the working pane second");
        // Flip back: grouped order restored, selection still on the lib pane.
        m.update(Event::Key(Key::Char('g')), 0, &mut ());
        assert!(m.grouped());
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
        assert_eq!(m.row_count(), 3, "no rows lost across the toggle");
    }

    #[test]
    fn draw_selection_skips_headers_but_jump_still_targets_the_pane() {
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        // Wide enough for the table arm, which is the arm that draws the headers.
        m.update(
            Event::Resize {
                width: 120,
                height: 20,
            },
            0,
            &mut (),
        );
        // Grouped rows [%00, %02, %01]; groups app(0,1), lib(2). Select the lib row.
        m.sel.index = 2;
        // Two headers precede it (app's and lib's): draw index 2 + 2 = 4.
        assert_eq!(m.draw_selection(), 4);
        // Enter reads selected_row() by the flat index — the lib pane, unaffected by the headers.
        let fx = m.update(Event::Key(Key::Enter), 0, &mut ());
        assert!(
            matches!(
                fx.as_slice(),
                [Effect::Focus(r), Effect::ClearAttention { pane }]
                    if r.pane_id == "%01" && *pane == "%01"
            ),
            "Enter jumps to the selected pane, got {fx:?}"
        );
    }

    #[test]
    fn wrap_nav_traverses_the_grouped_order() {
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        // Grouped rows [%00 app-blocked, %02 app-idle, %01 lib-working].
        assert_eq!(m.selected_row().unwrap().pane_id, "%00");
        // Up from the top wraps to the last grouped row (lib), not the flat-sorted tail.
        m.update(Event::Key(Key::Up), 0, &mut ());
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
        // Down wraps to the first grouped row, then steps within the app group to its idle member.
        m.update(Event::Key(Key::Down), 0, &mut ());
        assert_eq!(m.selected_row().unwrap().pane_id, "%00");
        m.update(Event::Key(Key::Down), 0, &mut ());
        assert_eq!(m.selected_row().unwrap().pane_id, "%02");
    }

    #[test]
    fn refresh_keeps_grouping_and_the_selected_pane() {
        // A grouped model refreshed with reordered rows re-groups and keeps the selection on its pane.
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        m.sel.index = 2; // the lib pane
        m.update(Event::RowsRefreshed(grouped_fixture()), 0, &mut ());
        assert!(m.grouped());
        let ids: Vec<&str> = m.rows().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["%00", "%02", "%01"],
            "grouping survives the refresh"
        );
        assert_eq!(m.selected_row().unwrap().pane_id, "%01");
    }

    #[test]
    fn display_items_track_the_g_toggle() {
        // Grouped: a header above each group's rows, in display order (what the draw renders).
        let mut m = WatchModel::new(grouped_fixture(), WidePref::Table, 0);
        assert_eq!(
            display_shape(&m),
            vec!["▸app", "%00", "%02", "▸lib", "%01"],
            "each group's panes follow its header, contiguously"
        );
        // Flat: the same rows in state order, no headers.
        m.update(Event::Key(Key::Char('g')), 0, &mut ());
        assert_eq!(display_shape(&m), vec!["%00", "%01", "%02"]);
        // Back to grouped: the headers return.
        m.update(Event::Key(Key::Char('g')), 0, &mut ());
        assert_eq!(display_shape(&m), vec!["▸app", "%00", "%02", "▸lib", "%01"]);
    }

    #[test]
    fn display_items_hold_no_rows_when_empty() {
        // A grouped model over zero rows yields nothing to draw: no stray headers.
        let m = WatchModel::new(vec![], WidePref::Table, 0);
        assert!(m.grouped());
        assert!(display_shape(&m).is_empty());
        assert_eq!(m.draw_selection(), 0);
    }

    #[test]
    fn show_branch_and_model_track_the_row_set() {
        // No repo/model on any row: both columns stay hidden.
        let mut m = WatchModel::new(two_rows(), WidePref::Table, 0);
        assert!(!m.show_branch());
        assert!(!m.show_model());
        // A refresh bringing a resolved branch and a model flips both caches.
        let mut r = repo_row(0, 0, "app", "main", AgentState::Blocked, 5);
        r.model = Some("gpt-5.6".to_string());
        m.update(Event::RowsRefreshed(vec![r]), 0, &mut ());
        assert!(m.show_branch(), "a resolved branch shows the branch column");
        assert!(m.show_model(), "a stamped model shows the model column");
    }

    #[test]
    fn table_header_branch_label_only_when_shown() {
        assert!(!line_text(&table_header(false, false)).contains("branch"));
        assert!(line_text(&table_header(false, true)).contains("branch"));
    }

    // --- properties -----------------------------------------------------------------------------

    use proptest::prelude::*;

    fn arb_watch_state() -> impl Strategy<Value = AgentState> {
        prop_oneof![
            Just(AgentState::Blocked),
            Just(AgentState::Working),
            Just(AgentState::Idle),
            Just(AgentState::Unknown),
        ]
    }

    /// Rows with unique pane ids and varied repos (`None`, or one of a few names) so grouping has
    /// something to reorder; `i` seeds a distinct pane id per row.
    fn arb_watch_rows() -> impl Strategy<Value = Vec<AgentRow>> {
        let repo = prop_oneof![Just(None), "[a-c]".prop_map(Some)];
        prop::collection::vec((repo, arb_watch_state(), 0u64..100), 1..10).prop_map(|specs| {
            specs
                .into_iter()
                .enumerate()
                .map(|(i, (repo, state, since))| AgentRow {
                    repo: repo.map(|name: String| RepoLabel {
                        name,
                        branch: String::new(),
                        worktree: false,
                    }),
                    ..row("s", i as u32, 0, state, since)
                })
                .collect()
        })
    }

    /// Pane ids sorted — a multiset identity for the row set (pane ids are unique per row).
    fn pane_multiset(m: &WatchModel) -> Vec<String> {
        let mut ids: Vec<String> = m.rows().map(|r| r.pane_id.clone()).collect();
        ids.sort();
        ids
    }

    proptest! {
        /// Toggling grouped off then on preserves the selected pane (the reanchor invariant) and
        /// leaves the row multiset unchanged — the round trip only reorders.
        #[test]
        fn g_toggle_round_trip_preserves_selection_and_multiset(
            rows in arb_watch_rows(),
            sel in 0usize..30,
        ) {
            let mut m = WatchModel::new(rows, WidePref::Table, 0);
            m.sel.index = sel % m.row_count();
            let pane = m.selected_row().unwrap().pane_id.clone();
            let before = pane_multiset(&m);
            m.update(Event::Key(Key::Char('g')), 0, &mut ());
            m.update(Event::Key(Key::Char('g')), 0, &mut ());
            prop_assert_eq!(m.selected_row().unwrap().pane_id.clone(), pane);
            prop_assert_eq!(pane_multiset(&m), before);
        }
    }

    #[test]
    fn table_branch_column_aligns_and_hides_when_absent() {
        let styles = RowPalette::default();
        let now = 1_000_000;
        let mut resolved = row("s", 0, 0, AgentState::Working, now - 1_000);
        resolved.repo = Some(RepoLabel {
            name: "r".to_string(),
            branch: "main".to_string(),
            worktree: false,
        });
        let mut long = row("verylongsession", 1, 2, AgentState::Idle, now - 5_000);
        long.repo = Some(RepoLabel {
            name: "r".to_string(),
            branch: "feature/really-long".to_string(),
            worktree: false,
        });
        let none = row("s", 0, 3, AgentState::Idle, now - 1_000);

        // Shown: every row carries a fixed-width branch cell, so the fixed columns still align.
        let a = table_row(&styles, &resolved, now, false, true, 10);
        let b = table_row(&styles, &long, now, false, true, 10);
        let c = table_row(&styles, &none, now, false, true, 10);
        assert_eq!(prefix_width(&a), prefix_width(&b), "branch column aligns");
        assert_eq!(
            prefix_width(&a),
            prefix_width(&c),
            "an unresolved branch keeps its blank cell"
        );
        assert!(line_text(&a).contains("main"));
        assert!(line_text(&b).contains('…'), "a long branch truncates");

        // Hidden: the branch cell disappears, narrowing the prefix by exactly its column.
        let off = table_row(&styles, &resolved, now, false, false, 10);
        assert_eq!(prefix_width(&a) - prefix_width(&off), BRANCH_W + 1);
        assert!(!line_text(&off).contains("main"));
        assert_eq!(
            table_title_width(100, false, true) + BRANCH_W + 1,
            table_title_width(100, false, false),
            "the branch column costs the title exactly its width"
        );
    }
}
