//! The list's scroll window, the pointer's hover, and click timing: the view state a mouse adds.
//!
//! Indices here are DRAW indices — positions in the line list the draw builds, group headers
//! included — not row indices. That is the space a click arrives in, and the space the scroll
//! offset is handed to ratatui in.

/// How long after a click a second one on the same line counts as a double-click (ms). Long enough
/// for a deliberate double tap, short enough that two considered clicks on the same row are two
/// selections and not a jump.
const DOUBLE_CLICK_MS: u64 = 400;

/// What a press turned out to be once the previous one is taken into account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Click {
    Single,
    Double,
}

/// Scroll offset + hover, kept together because both are pure view state the fold owns so the draw
/// and the hit-test read the same numbers.
#[derive(Debug, Default)]
pub(crate) struct View {
    /// First visible draw index. The fold owns it (rather than letting the list widget pick one per
    /// frame) precisely so a click can be resolved back to a line.
    scroll: usize,
    /// The line the pointer is over, `None` when it is elsewhere or has not moved yet.
    hover: Option<usize>,
    /// The last press: line and when, for double-click detection.
    last_click: Option<(usize, u64)>,
}

impl View {
    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    pub(crate) fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Point the hover at `line` (or nowhere). Returns whether it changed, so a fold can skip work
    /// for the many motion events that stay on one row.
    pub(crate) fn set_hover(&mut self, line: Option<usize>) -> bool {
        let changed = self.hover != line;
        self.hover = line;
        changed
    }

    /// Scroll so `selected` is visible in a `viewport`-line window over `len` lines, then clamp the
    /// offset so the list never scrolls past its own end. Called after anything that moves the
    /// selection, changes the row set, or resizes the frame.
    pub(crate) fn sync(&mut self, len: usize, viewport: usize, selected: usize) {
        if viewport == 0 || len == 0 {
            self.scroll = 0;
            return;
        }
        if selected < self.scroll {
            self.scroll = selected;
        } else if selected >= self.scroll + viewport {
            self.scroll = selected + 1 - viewport;
        }
        self.scroll = self.scroll.min(len.saturating_sub(viewport));
        // A hover past the end of the list is stale (rows went away under the pointer).
        if self.hover.is_some_and(|h| h >= len) {
            self.hover = None;
        }
    }

    /// Record a press on `line` and classify it against the previous one.
    pub(crate) fn click(&mut self, line: usize, now: u64) -> Click {
        let double = self
            .last_click
            .is_some_and(|(prev, at)| prev == line && now.saturating_sub(at) <= DOUBLE_CLICK_MS);
        // A double-click closes the pair: a third press starts over rather than firing again.
        self.last_click = if double { None } else { Some((line, now)) };
        if double {
            Click::Double
        } else {
            Click::Single
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_scrolls_only_far_enough_to_show_the_selection() {
        let mut v = View::default();
        // 20 lines, 5 visible. Selecting line 7 scrolls just enough to put it at the bottom.
        v.sync(20, 5, 7);
        assert_eq!(v.scroll(), 3);
        // Moving up inside the window does not scroll.
        v.sync(20, 5, 4);
        assert_eq!(v.scroll(), 3);
        // Above the window scrolls to put it on top.
        v.sync(20, 5, 2);
        assert_eq!(v.scroll(), 2);
        // The end of the list is the floor: no scrolling into empty space.
        v.sync(20, 5, 19);
        assert_eq!(v.scroll(), 15);
        // A list that shrinks under the offset pulls it back.
        v.sync(6, 5, 0);
        assert_eq!(v.scroll(), 0);
    }

    #[test]
    fn sync_handles_a_viewport_or_list_of_nothing() {
        let mut v = View::default();
        v.sync(0, 5, 0);
        assert_eq!(v.scroll(), 0);
        v.sync(20, 0, 9);
        assert_eq!(v.scroll(), 0, "no rows visible ⇒ nothing to scroll to");
    }

    #[test]
    fn a_hover_past_the_end_is_dropped_on_sync() {
        let mut v = View::default();
        assert!(v.set_hover(Some(8)));
        assert!(!v.set_hover(Some(8)), "the same line is not a change");
        v.sync(9, 5, 0);
        assert_eq!(v.hover(), Some(8), "still a real line");
        v.sync(4, 5, 0);
        assert_eq!(v.hover(), None, "the rows under the pointer went away");
    }

    #[test]
    fn a_second_press_on_the_same_line_inside_the_window_is_a_double() {
        let mut v = View::default();
        assert_eq!(v.click(3, 1_000), Click::Single);
        assert_eq!(v.click(3, 1_300), Click::Double);
        // The pair is closed: the next press starts a new one.
        assert_eq!(v.click(3, 1_400), Click::Single);
        // Too slow, or a different line: single either way.
        assert_eq!(v.click(3, 2_000), Click::Single);
        assert_eq!(v.click(4, 2_100), Click::Single);
    }
}
