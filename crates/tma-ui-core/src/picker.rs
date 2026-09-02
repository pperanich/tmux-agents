//! The picker surface's pure fold: the filter/selection/refresh/preview state and the `update` that
//! folds an `Event` into it, returning the `Effect`s the shell executes. No terminal, no tmux
//! handle: the fuzzy `Matcher` is the associated resource (`Res`), passed in per call.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};
use ratatui::layout::Rect;
use ratatui::text::Text;
use tma_core::{sort_rank, AgentRow};

use crate::common::{preview_fits, Common};
use crate::effect::Effect;
use crate::event::{Event, Mouse, MouseKind};
use crate::key::Key;
use crate::layout::picker_geom;
use crate::selection::Selection;
use crate::view::{Click, View};

/// Rows one wheel notch moves the selection, matching `tma watch`'s.
const WHEEL_STEP: i32 = 3;

/// The picker's model: rows, the fuzzy filter/scope, selection, the refresh gate, and the preview
/// cache. Derives `Debug` so an event-script test can assert a model projection.
///
/// Every field is private: `visible` holds indices into `all`, so an assignment to either from
/// outside could turn a draw into an index panic. Mutation goes through [`new`](PickerModel::new)
/// and [`update`](PickerModel::update); the draw reads the accessors below.
#[derive(Debug)]
pub struct PickerModel {
    /// All agent rows, state-sorted (blocked → working → idle → unknown, then longest-in-state first).
    all: Vec<AgentRow>,
    query: String,
    /// True when the ctrl-s session scope is active (scoped to `current_session`).
    scoped: bool,
    /// The invoking client's session, seeded by the shell; the ctrl-s scope filters `all` down to it.
    current_session: String,
    sel: Selection,
    /// Indices into `all`, after scope + fuzzy filter, in display order.
    visible: Vec<usize>,
    /// Whether any visible row resolved a non-empty branch: the branch column shows iff true. A
    /// derived cache, recomputed by `recompute` wherever `visible` changes.
    show_branch: bool,
    /// The last body width seen (from a Resize), which gates the preview pane; 0 until the shell's
    /// initial Resize lands, so nothing is captured before the first real frame.
    width: u16,
    /// The last height seen (from a Resize). With `width` it is the frame the mouse hit-test and
    /// the scroll window are measured against — the same frame the draw lays out.
    height: u16,
    /// Scroll offset + hover + click timing, in draw-line space (the picker never groups, so a
    /// draw line is a visible row).
    view: View,
    /// The refresh deadline and preview cache both surfaces share.
    common: Common,
}

impl PickerModel {
    /// Seed from the first (stamp) rows; `now` arms the refresh gate, `current_session` backs ctrl-s.
    pub fn new(
        rows: Vec<AgentRow>,
        current_session: String,
        now: u64,
        matcher: &mut Matcher,
    ) -> PickerModel {
        let mut m = PickerModel {
            all: sorted(rows),
            query: String::new(),
            scoped: false,
            current_session,
            sel: Selection::default(),
            visible: Vec::new(),
            show_branch: false,
            width: 0,
            height: 0,
            view: View::default(),
            common: Common::new(now),
        };
        m.recompute(matcher);
        m
    }

    /// Fold one event into the model, returning the effects the shell executes.
    pub fn update(&mut self, ev: Event, now: u64, matcher: &mut Matcher) -> Vec<Effect> {
        if let Some(fx) = self.common.update(&ev, now) {
            return fx;
        }
        match ev {
            Event::Key(k) => self.on_key(k, matcher),
            Event::Mouse(m) => self.on_mouse(m, now),
            // Width is model state: a popup narrower than the gate carries no preview, so crossing
            // the gate either way drops what is cached before the capture decision below. Height
            // rides along for the scroll window and the hit-test.
            Event::Resize { width, height } => {
                if preview_fits(width) != self.preview_visible() {
                    self.common.drop_preview();
                }
                self.width = width;
                self.height = height;
                self.sync_view();
                self.selection_preview_effect()
            }
            Event::RowsRefreshed(rows) => {
                self.set_rows(rows, matcher);
                // Reset the preview target on a successful refresh: the follow-up capture re-syncs
                // it to the reanchored selection.
                self.common.forget_target();
                self.selection_preview_effect()
            }
            // Handled by `Common::update` above; spelled out so a new variant must be routed
            // deliberately rather than falling into a silent no-op.
            Event::Tick | Event::Nudge | Event::RefreshFailed | Event::PreviewCaptured { .. } => {
                vec![]
            }
        }
    }

    fn on_key(&mut self, k: Key, matcher: &mut Matcher) -> Vec<Effect> {
        // Reaching for the keyboard ends the pointer's claim: otherwise the hover mark sits there
        // beside the selection, and two marked rows read as two selections. The next pointer move
        // brings it back.
        self.view.set_hover(None);
        match k {
            Key::Esc | Key::CtrlC => vec![Effect::Quit],
            Key::Enter => self.focus_batch(),
            Key::CtrlS => {
                self.scoped = !self.scoped;
                self.recompute(matcher);
                self.selection_preview_effect()
            }
            Key::Up => {
                self.move_by(-1);
                self.selection_preview_effect()
            }
            Key::Down => {
                self.move_by(1);
                self.selection_preview_effect()
            }
            Key::Backspace => {
                self.query.pop();
                self.recompute(matcher);
                self.selection_preview_effect()
            }
            // Tab opens the action menu on the highlighted pane. Not a printable character, which
            // is the whole point: every one of those belongs to the query, so an agent called
            // `auth` (or `1password`) can actually be searched for.
            Key::Tab => match self.selected_row() {
                Some(r) => vec![Effect::ActMenu {
                    pane: r.pane_id.clone(),
                }],
                None => vec![],
            },
            Key::Char(c) => {
                self.query.push(c);
                self.recompute(matcher);
                self.selection_preview_effect()
            }
        }
    }

    /// Fold one mouse report. The picker is modal and exists to pick one row, so a press selects
    /// and a second press on that row is the jump — the mouse spelling of "highlight, then Enter",
    /// deliberately two clicks so a stray one cannot teleport the client. Hover only highlights.
    fn on_mouse(&mut self, m: Mouse, now: u64) -> Vec<Effect> {
        let line = self.line_at(m.col, m.row);
        match m.kind {
            MouseKind::Moved => {
                self.view.set_hover(line);
                vec![]
            }
            MouseKind::Down => {
                let Some(row) = line else {
                    return vec![]; // the border, the preview, the query line
                };
                let click = self.view.click(row, now);
                self.sel.index = row;
                self.sync_view();
                match click {
                    Click::Double => self.focus_batch(),
                    Click::Single => self.selection_preview_effect(),
                }
            }
            MouseKind::ScrollUp => {
                self.step(-WHEEL_STEP);
                self.selection_preview_effect()
            }
            MouseKind::ScrollDown => {
                self.step(WHEEL_STEP);
                self.selection_preview_effect()
            }
        }
    }

    /// The visible-row index under a point, `None` outside the list (or past its last row).
    fn line_at(&self, col: u16, row: u16) -> Option<usize> {
        picker_geom(self.area(), self.preview_visible())
            .list
            .index_at(self.view.scroll(), col, row)
            .filter(|&l| l < self.visible.len())
    }

    /// Move the selection without wrapping (the wheel's semantics; the arrow keys still wrap).
    fn step(&mut self, delta: i32) {
        self.sel.step(self.visible.len(), delta);
        self.sync_view();
    }

    /// Re-derive the scroll window from the current selection, row count, and viewport, so the
    /// highlight is always on screen and the hit-test always reads the window the draw painted.
    fn sync_view(&mut self) {
        let viewport = picker_geom(self.area(), self.preview_visible())
            .list
            .viewport();
        self.view.sync(self.visible.len(), viewport, self.sel.index);
    }

    /// The Enter/quick-select jump batch: focus the highlighted agent, clear its attention, quit.
    /// `Quit` is present, so the shell defers the whole batch until the terminal is restored.
    fn focus_batch(&self) -> Vec<Effect> {
        match self.selected_row() {
            Some(r) => vec![
                Effect::Focus(Box::new(r.clone())),
                Effect::ClearAttention {
                    pane: r.pane_id.clone(),
                },
                Effect::Quit,
            ],
            None => vec![Effect::Quit],
        }
    }

    /// Capture the highlighted pane's preview iff the preview is showing and the pane differs from
    /// the cached target. A popup below the gate captures nothing: one `capture-pane` per selection
    /// move buys a preview too narrow to read.
    fn selection_preview_effect(&mut self) -> Vec<Effect> {
        if !self.preview_visible() {
            return vec![];
        }
        let sel_pane = self.selected_row().map(|r| r.pane_id.clone());
        self.common.capture(sel_pane)
    }

    /// Replace the rows (a refresh), preserving the highlighted pane by id when it survives the reorder.
    fn set_rows(&mut self, rows: Vec<AgentRow>, matcher: &mut Matcher) {
        let anchor = self.selected_row().map(|r| r.pane_id.clone());
        self.all = sorted(rows);
        self.recompute(matcher);
        let ids: Vec<&str> = self
            .visible
            .iter()
            .map(|&i| self.all[i].pane_id.as_str())
            .collect();
        self.sel.reanchor(&ids, anchor.as_deref());
    }

    fn recompute(&mut self, matcher: &mut Matcher) {
        let scope = self.scoped.then_some(self.current_session.as_str());
        self.visible = compute_visible(&self.all, &self.query, scope, matcher);
        self.sel.clamp(self.visible.len());
        self.sync_view();
        // The branch column keys off the visible rows (not `all`), so recompute it here, wherever
        // scope + fuzzy filter change `visible`.
        self.show_branch = self
            .visible
            .iter()
            .any(|&i| self.all[i].branch().is_some_and(|b| !b.is_empty()));
    }

    /// The highlighted row, or `None` when the visible list is empty.
    pub fn selected_row(&self) -> Option<&AgentRow> {
        self.visible.get(self.sel.index).map(|&i| &self.all[i])
    }

    fn move_by(&mut self, delta: i32) {
        self.sel.move_by(self.visible.len(), delta);
        self.sync_view();
    }

    // --- draw accessors -------------------------------------------------------------------------

    /// The fuzzy query as typed; the prompt echoes it verbatim. Every printable key reaches it,
    /// with no key held back for a shortcut.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether the ctrl-s session scope is active; the prompt names the current scope.
    pub fn scoped(&self) -> bool {
        self.scoped
    }

    /// Whether any visible row resolved a branch, so the list spends a column on it.
    pub fn show_branch(&self) -> bool {
        self.show_branch
    }

    /// The visible row count: the list title's `agents (N)`, and zero means no highlight.
    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// The rows left by the scope and fuzzy filter, in display order. Yields borrowed rows, so the
    /// draw never indexes `all` through `visible` itself; `enumerate` gives the quick-select digit.
    pub fn visible_rows(&self) -> impl ExactSizeIterator<Item = &AgentRow> {
        self.visible.iter().map(|&i| &self.all[i])
    }

    /// The highlighted row's index among [`visible_rows`](Self::visible_rows).
    pub fn selected_index(&self) -> usize {
        self.sel.index
    }

    /// Whether the popup is wide enough to carry the preview pane beside the list. The draw splits
    /// the body iff this holds, matching the fold's capture gate.
    pub fn preview_visible(&self) -> bool {
        preview_fits(self.width)
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

    /// The first visible row: handed to the list widget so the window the draw paints is the window
    /// the hit-test assumed.
    pub fn scroll(&self) -> usize {
        self.view.scroll()
    }

    /// The row the pointer is over, `None` when it is elsewhere. Drawn dimmer than the selection:
    /// it says what a click would take, not what is current.
    pub fn hover(&self) -> Option<usize> {
        self.view.hover()
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

/// Sort rows by state priority: blocked → working → idle → unknown, then longest-in-state first
/// (the smallest `@agent_since`). Shared with `tma watch` (same order); crate-internal.
pub(crate) fn sorted(mut rows: Vec<AgentRow>) -> Vec<AgentRow> {
    rows.sort_by(|a, b| {
        sort_rank(a.state)
            .cmp(&sort_rank(b.state))
            .then_with(|| a.since.cmp(&b.since))
    });
    rows
}

/// Filter `all` by session scope and the fuzzy query. An empty query keeps the scoped rows in
/// state-priority order; a query ranks them by fuzzy score (a stable sort keeps that order on ties).
/// Crate-internal: the shell drives it only through `PickerModel::update`.
pub(crate) fn compute_visible(
    all: &[AgentRow],
    query: &str,
    scope: Option<&str>,
    matcher: &mut Matcher,
) -> Vec<usize> {
    let scoped: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, r)| scope.is_none_or(|s| r.session == s))
        .map(|(i, _)| i)
        .collect();
    if query.is_empty() {
        return scoped;
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = scoped
        .into_iter()
        .filter_map(|i| {
            let hay = haystack(&all[i]);
            let score = pattern.score(Utf32Str::new(&hay, &mut buf), matcher)?;
            Some((i, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(i, _)| i).collect()
}

fn haystack(r: &AgentRow) -> String {
    format!("{} {} {}", r.agent, r.locator(), r.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PREVIEW_MIN_WIDTH;
    use tma_core::AgentState;

    fn row(session: &str, w: u32, p: u32, agent: &str, state: AgentState, since: u64) -> AgentRow {
        AgentRow {
            pane_id: format!("%{w}{p}"),
            agent: agent.to_string(),
            state,
            detail: None,
            since,
            turn_at: 0,
            session: session.to_string(),
            window_index: w,
            pane_index: p,
            title: format!("{agent} task"),
            attention: false,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
            pending: None,
        }
    }

    fn matcher() -> Matcher {
        Matcher::new(nucleo::Config::DEFAULT)
    }

    /// A model over `rows`, seeded at `now = 0` with the given current session and the shell's
    /// initial Resize at a popup width wide enough for the preview.
    fn model(rows: Vec<AgentRow>, session: &str, m: &mut Matcher) -> PickerModel {
        let mut p = PickerModel::new(rows, session.to_string(), 0, m);
        p.update(resize(100), 0, m);
        p
    }

    /// The shell's Resize at `width`; the height sizes the popup's scroll window.
    fn resize(width: u16) -> Event {
        Event::Resize { width, height: 40 }
    }

    /// A mouse event at a point, folded like the runner would.
    fn mouse(
        p: &mut PickerModel,
        kind: MouseKind,
        col: u16,
        row: u16,
        now: u64,
        m: &mut Matcher,
    ) -> Vec<Effect> {
        p.update(Event::Mouse(Mouse { kind, col, row }), now, m)
    }

    #[test]
    fn a_click_selects_and_a_second_one_jumps_and_closes() {
        let mut mtch = matcher();
        let rows = vec![
            row("s", 0, 0, "alpha", AgentState::Blocked, 10),
            row("s", 0, 1, "bravo", AgentState::Working, 10),
            row("s", 0, 2, "charlie", AgentState::Idle, 10),
        ];
        let mut p = model(rows, "s", &mut mtch);
        // Screen row 2 is the second list row (row 0 is the popup's top border).
        mouse(&mut p, MouseKind::Down, 4, 2, 1_000, &mut mtch);
        assert_eq!(p.selected_row().unwrap().agent, "bravo");
        // A press elsewhere in the popup (the preview half) leaves the selection alone.
        mouse(&mut p, MouseKind::Down, 80, 2, 1_100, &mut mtch);
        assert_eq!(p.selected_row().unwrap().agent, "bravo");

        let fx = mouse(&mut p, MouseKind::Down, 4, 2, 1_300, &mut mtch);
        assert!(
            matches!(
                fx.as_slice(),
                [Effect::Focus(r), Effect::ClearAttention { .. }, Effect::Quit]
                    if r.agent == "bravo"
            ),
            "the picker is modal: its double-click jumps and closes, got {fx:?}"
        );
    }

    #[test]
    fn hover_highlights_without_selecting_and_the_wheel_moves_the_selection() {
        let mut mtch = matcher();
        let rows = (0..5)
            .map(|p| row("s", 0, p, "claude", AgentState::Working, 10 + p as u64))
            .collect();
        let mut p = model(rows, "s", &mut mtch);

        mouse(&mut p, MouseKind::Moved, 4, 3, 0, &mut mtch);
        assert_eq!(p.hover(), Some(2));
        assert_eq!(p.selected_index(), 0, "hover never moves the selection");
        mouse(&mut p, MouseKind::Moved, 4, 0, 0, &mut mtch);
        assert_eq!(p.hover(), None, "the border is not a row");

        // Back over a row, then the wheel: the pointer has not left, so the hover stays put while
        // the selection moves under it.
        mouse(&mut p, MouseKind::Moved, 4, 3, 0, &mut mtch);
        mouse(&mut p, MouseKind::ScrollDown, 4, 3, 0, &mut mtch);
        assert_eq!(p.selected_index(), 3);
        mouse(&mut p, MouseKind::ScrollDown, 4, 3, 0, &mut mtch);
        assert_eq!(p.selected_index(), 4, "the last row holds — no wrap");
        assert_eq!(
            p.hover(),
            Some(2),
            "the wheel is the pointer, so hover stays"
        );

        // Typing takes the list back: two marked rows would read as two selections.
        p.update(Event::Key(Key::Down), 0, &mut mtch);
        assert_eq!(p.hover(), None);
    }

    /// The fuzzy filter shortens the list under the pointer; a hover left pointing past the end is
    /// stale, and a click there must not select a row that is no longer drawn.
    #[test]
    fn a_filter_that_shortens_the_list_drops_a_stale_hover() {
        let mut mtch = matcher();
        let rows = vec![
            row("s", 0, 0, "alpha", AgentState::Blocked, 10),
            row("s", 0, 1, "bravo", AgentState::Working, 10),
            row("s", 0, 2, "charlie", AgentState::Idle, 10),
        ];
        let mut p = model(rows, "s", &mut mtch);
        mouse(&mut p, MouseKind::Moved, 4, 3, 0, &mut mtch);
        assert_eq!(p.hover(), Some(2));
        // "alpha" leaves one visible row.
        for c in "alpha".chars() {
            p.update(Event::Key(Key::Char(c)), 0, &mut mtch);
        }
        assert_eq!(p.visible_count(), 1);
        assert_eq!(p.hover(), None, "the hovered row is gone");
        mouse(&mut p, MouseKind::Down, 4, 3, 1_000, &mut mtch);
        assert_eq!(p.selected_index(), 0, "the empty space selects nothing");
    }

    // --- ports of the existing selection/filter unit tests --------------------------------------

    #[test]
    fn sort_blocked_first_then_longest_in_state() {
        let rows = vec![
            row("a", 0, 0, "c", AgentState::Idle, 500),
            row("a", 0, 1, "c", AgentState::Blocked, 300), // blocked, newer
            row("a", 0, 2, "c", AgentState::Blocked, 100), // blocked, longest
            row("a", 0, 3, "c", AgentState::Working, 200),
        ];
        let s = sorted(rows);
        assert_eq!(s[0].state, AgentState::Blocked);
        assert_eq!(s[0].since, 100, "longest-blocked first");
        assert_eq!(s[1].since, 300);
        assert_eq!(s[2].state, AgentState::Working);
        assert_eq!(s[3].state, AgentState::Idle);
    }

    #[test]
    fn done_row_keeps_idle_sort_rank() {
        // A done row (idle + attention) sorts as idle, not promoted (presentation only).
        let rows = vec![
            AgentRow {
                attention: true,
                ..row("a", 0, 0, "c", AgentState::Idle, 10)
            },
            row("a", 0, 1, "c", AgentState::Working, 10),
        ];
        let s = sorted(rows);
        assert_eq!(
            s[0].state,
            AgentState::Working,
            "working still ranks above done-idle"
        );
        assert!(s[1].attention);
    }

    #[test]
    fn empty_query_keeps_all_in_order() {
        let all = sorted(vec![
            row("a", 0, 0, "claude", AgentState::Idle, 10),
            row("b", 0, 0, "codex", AgentState::Blocked, 10),
        ]);
        let mut m = matcher();
        let vis = compute_visible(&all, "", None, &mut m);
        assert_eq!(vis.len(), 2);
        assert_eq!(
            all[vis[0]].state,
            AgentState::Blocked,
            "sort order preserved"
        );
    }

    #[test]
    fn fuzzy_query_filters_and_ranks() {
        let all = sorted(vec![
            row("proj", 0, 0, "claude", AgentState::Idle, 10),
            row("other", 0, 0, "codex", AgentState::Idle, 10),
        ]);
        let mut m = matcher();
        let vis = compute_visible(&all, "codex", None, &mut m);
        assert_eq!(vis.len(), 1);
        assert_eq!(all[vis[0]].agent, "codex");
    }

    #[test]
    fn session_scope_filters_by_session_preserving_query() {
        let all = sorted(vec![
            row("proj", 0, 0, "claude", AgentState::Idle, 10),
            row("other", 0, 0, "claude", AgentState::Idle, 10),
        ]);
        let mut m = matcher();
        let vis = compute_visible(&all, "claude", Some("proj"), &mut m);
        assert_eq!(vis.len(), 1);
        assert_eq!(all[vis[0]].session, "proj");
    }

    #[test]
    fn move_by_wraps() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "c", AgentState::Blocked, 10),
                row("a", 0, 1, "c", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        p.move_by(-1);
        assert_eq!(p.sel.index, 1, "up from the top wraps to the bottom");
        p.move_by(1);
        assert_eq!(p.sel.index, 0);
    }

    #[test]
    fn rows_refreshed_keeps_selection_on_same_pane() {
        // A refresh arrives as RowsRefreshed; the selection follows the highlighted pane across the
        // reorder (the old `set_rows` reanchor, now driven through `update`).
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "c", AgentState::Blocked, 10),
                row("a", 0, 1, "c", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        p.sel.index = 1;
        let pane = p.selected_row().unwrap().pane_id.clone();
        p.update(
            Event::RowsRefreshed(vec![
                row("a", 0, 1, "c", AgentState::Working, 10),
                row("a", 0, 0, "c", AgentState::Blocked, 10),
            ]),
            0,
            &mut m,
        );
        assert_eq!(p.selected_row().unwrap().pane_id, pane);
    }

    // --- shared behaviors (reused by the watch update) -------------------------------------------

    #[test]
    fn stale_rows_kept_on_refresh_failure() {
        let mut m = matcher();
        let mut p = model(
            vec![row("a", 0, 0, "c", AgentState::Blocked, 10)],
            "a",
            &mut m,
        );
        let before: Vec<String> = p.all.iter().map(|r| r.pane_id.clone()).collect();
        let effects = p.update(Event::RefreshFailed, 0, &mut m);
        assert!(effects.is_empty(), "a failed refresh emits nothing");
        let after: Vec<String> = p.all.iter().map(|r| r.pane_id.clone()).collect();
        assert_eq!(before, after, "the stale rows are left untouched");
    }

    #[test]
    fn preview_recapture_on_selection_change() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "c", AgentState::Blocked, 10),
                row("a", 0, 1, "c", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        // Establish a cached preview for the first row.
        let first = p.selected_row().unwrap().pane_id.clone();
        p.update(
            Event::PreviewCaptured {
                pane: first.clone(),
                ansi: String::new(),
            },
            0,
            &mut m,
        );
        assert_eq!(p.preview_target(), Some(first.as_str()));
        // Moving the selection emits exactly one capture for the newly highlighted pane.
        let effects = p.update(Event::Key(Key::Down), 0, &mut m);
        let next = p.selected_row().unwrap().pane_id.clone();
        assert!(
            matches!(effects.as_slice(), [Effect::CapturePreview { pane }] if *pane == next),
            "one CapturePreview for the new pane, got {effects:?}"
        );
    }

    // --- the width-gated preview -----------------------------------------------------------------

    #[test]
    fn narrow_popup_suppresses_the_preview_and_its_captures() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "c", AgentState::Blocked, 10),
                row("a", 0, 1, "c", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        // Wide (the seeded 100): the preview shows and a selection move captures for it.
        assert!(p.preview_visible());
        let pane = p.selected_row().unwrap().pane_id.clone();
        p.update(
            Event::PreviewCaptured {
                pane: pane.clone(),
                ansi: "hello".to_string(),
            },
            0,
            &mut m,
        );
        assert!(matches!(
            p.update(Event::Key(Key::Down), 0, &mut m).as_slice(),
            [Effect::CapturePreview { .. }]
        ));

        // One column below the gate: the preview goes away and the cache with it.
        let fx = p.update(resize(PREVIEW_MIN_WIDTH - 1), 0, &mut m);
        assert!(!p.preview_visible());
        assert!(fx.is_empty(), "a sub-gate resize captures nothing");
        assert!(p.preview_target().is_none(), "the stale target is dropped");
        assert!(
            p.preview_text().lines.is_empty(),
            "the stale text is dropped"
        );
        // Moving the selection in a narrow popup issues no capture-pane at all.
        assert!(p.update(Event::Key(Key::Up), 0, &mut m).is_empty());
        assert!(p.update(Event::Key(Key::Down), 0, &mut m).is_empty());

        // Back at the gate exactly: the preview returns and re-captures for the highlighted pane.
        let fx = p.update(resize(PREVIEW_MIN_WIDTH), 0, &mut m);
        assert!(p.preview_visible());
        let sel = p.selected_row().unwrap().pane_id.clone();
        assert!(
            matches!(fx.as_slice(), [Effect::CapturePreview { pane }] if *pane == sel),
            "widening back re-captures, got {fx:?}"
        );
    }

    #[test]
    fn no_capture_before_the_first_resize() {
        // The model is seeded from stamps before the shell knows the popup width; nothing is
        // captured until that first Resize says the preview fits.
        let mut m = matcher();
        let mut p = PickerModel::new(
            vec![row("a", 0, 0, "c", AgentState::Blocked, 10)],
            "a".to_string(),
            0,
            &mut m,
        );
        assert!(!p.preview_visible());
        assert!(p.update(Event::Key(Key::Down), 0, &mut m).is_empty());
    }

    // --- picker-specific update behaviors --------------------------------------------------------

    #[test]
    fn enter_emits_focus_clear_quit() {
        let mut m = matcher();
        let mut p = model(
            vec![row("a", 0, 0, "c", AgentState::Blocked, 10)],
            "a",
            &mut m,
        );
        let pane = p.selected_row().unwrap().pane_id.clone();
        let effects = p.update(Event::Key(Key::Enter), 0, &mut m);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Focus(_), Effect::ClearAttention { pane: cp }, Effect::Quit] if *cp == pane
            ),
            "Enter yields [Focus, ClearAttention, Quit], got {effects:?}"
        );
    }

    /// Every printable key belongs to the query, with none held back for a shortcut: an agent
    /// called `auth` or a branch called `2fa` has to be reachable from an empty prompt.
    #[test]
    fn every_printable_key_types_including_a_and_the_digits() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "auth", AgentState::Blocked, 10),
                // Not `claude`: "au" is a subsequence of it, and this asserts the filter narrowed.
                row("a", 0, 1, "codex", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        for c in "au".chars() {
            let fx = p.update(Event::Key(Key::Char(c)), 0, &mut m);
            // A keystroke may re-capture the preview for a newly-highlighted row; what it must
            // never do is act or jump.
            assert!(
                !fx.iter()
                    .any(|e| matches!(e, Effect::ActMenu { .. } | Effect::Quit)),
                "typing neither acts nor jumps, got {fx:?}"
            );
        }
        assert_eq!(p.query(), "au");
        assert_eq!(p.visible_count(), 1, "and it filtered: {}", p.query());
        assert_eq!(p.selected_row().unwrap().agent, "auth");

        // Digits type too — there is no quick-select to intercept them.
        let mut p = model(
            vec![row("a", 0, 0, "claude", AgentState::Blocked, 10)],
            "a",
            &mut m,
        );
        let fx = p.update(Event::Key(Key::Char('2')), 0, &mut m);
        assert_eq!(p.query(), "2");
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Quit)),
            "a digit no longer jumps, got {fx:?}"
        );
    }

    #[test]
    fn tab_opens_the_action_menu_whatever_the_query_holds() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("a", 0, 0, "claude", AgentState::Blocked, 10),
                row("a", 0, 1, "claude", AgentState::Working, 10),
            ],
            "a",
            &mut m,
        );
        p.sel.index = 1;
        let pane = p.selected_row().unwrap().pane_id.clone();
        let fx = p.update(Event::Key(Key::Tab), 0, &mut m);
        assert!(
            matches!(fx.as_slice(), [Effect::ActMenu { pane: q }] if *q == pane),
            "tab yields one ActMenu for the highlighted pane, got {fx:?}"
        );
        assert_eq!(p.query(), "", "acting did not type into the query");

        // Mid-query it still acts: tab is not a character anyone can be searching for.
        p.update(Event::Key(Key::Char('c')), 0, &mut m);
        let fx = p.update(Event::Key(Key::Tab), 0, &mut m);
        assert_eq!(p.query(), "c", "tab did not touch the query");
        assert!(
            fx.iter().any(|e| matches!(e, Effect::ActMenu { .. })),
            "tab acts mid-query too, got {fx:?}"
        );
    }

    #[test]
    fn scope_toggle_recompute() {
        let mut m = matcher();
        let mut p = model(
            vec![
                row("proj", 0, 0, "c", AgentState::Blocked, 10),
                row("other", 0, 1, "c", AgentState::Working, 10),
            ],
            "proj",
            &mut m,
        );
        assert_eq!(p.visible_count(), 2, "both sessions visible unscoped");
        p.update(Event::Key(Key::CtrlS), 0, &mut m);
        assert!(p.scoped(), "ctrl-s scopes to the invoking session");
        assert_eq!(p.visible_count(), 1, "recompute drops the other session");
        assert_eq!(p.visible_rows().next().unwrap().session, "proj");
        // A second ctrl-s clears the scope.
        p.update(Event::Key(Key::CtrlS), 0, &mut m);
        assert!(!p.scoped());
        assert_eq!(p.visible_count(), 2);
    }

    #[test]
    fn tick_refreshes_only_on_the_deadline() {
        let mut m = matcher();
        let mut p = model(
            vec![row("a", 0, 0, "c", AgentState::Blocked, 10)],
            "a",
            &mut m,
        );
        assert!(
            p.update(Event::Tick, 500, &mut m).is_empty(),
            "before 1 s, a tick does not refresh"
        );
        assert!(
            matches!(
                p.update(Event::Tick, 1000, &mut m).as_slice(),
                [Effect::Refresh]
            ),
            "at the deadline, a tick refreshes"
        );
    }

    #[test]
    fn nudge_forces_an_immediate_refresh() {
        let mut m = matcher();
        let mut p = model(
            vec![row("a", 0, 0, "c", AgentState::Blocked, 10)],
            "a",
            &mut m,
        );
        // Well before the 1 s deadline, a nudge still refreshes at once.
        assert!(matches!(
            p.update(Event::Nudge, 200, &mut m).as_slice(),
            [Effect::Refresh]
        ));
    }
}
