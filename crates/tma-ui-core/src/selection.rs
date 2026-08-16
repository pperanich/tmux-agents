//! The highlighted-row index plus the wrap-nav/clamp/anchor-preserve logic both surfaces share.

/// The highlighted-row index plus the wrap-nav/clamp/anchor-preserve logic both surfaces share.
/// Kept separate from row storage (the picker filters via `visible`, `watch` indexes rows directly).
#[derive(Debug, Default)]
pub(crate) struct Selection {
    pub(crate) index: usize,
}

impl Selection {
    /// Move the selection by `delta`, wrapping at both ends. A no-op on an empty list.
    pub(crate) fn move_by(&mut self, len: usize, delta: i32) {
        if len == 0 {
            return;
        }
        let len = len as i32;
        self.index = (((self.index as i32 + delta) % len + len) % len) as usize;
    }

    /// Keep the index inside `[0, len)` (an empty list pins it to 0).
    pub(crate) fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.index = 0;
        } else if self.index >= len {
            self.index = len - 1;
        }
    }

    /// After a refresh: clamp, then re-seek the previously-highlighted pane (`anchor`) in the new
    /// display order (`ids`, pane ids) so the selection follows the same pane across a reorder.
    pub(crate) fn reanchor(&mut self, ids: &[&str], anchor: Option<&str>) {
        self.clamp(ids.len());
        if let Some(a) = anchor {
            if let Some(pos) = ids.iter().position(|&id| id == a) {
                self.index = pos;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_by_wraps_and_noops_on_empty() {
        let mut s = Selection::default();
        s.move_by(2, -1);
        assert_eq!(s.index, 1, "up from the top wraps to the bottom");
        s.move_by(2, 1);
        assert_eq!(s.index, 0);
        s.move_by(0, 1);
        assert_eq!(s.index, 0, "empty list is a no-op");
    }

    #[test]
    fn clamp_pins_into_range() {
        let mut s = Selection { index: 5 };
        s.clamp(3);
        assert_eq!(s.index, 2);
        s.clamp(0);
        assert_eq!(s.index, 0);
    }

    #[test]
    fn reanchor_across_refresh() {
        // The pane highlighted at index 1 is first in the refreshed order; the selection follows it.
        let mut s = Selection { index: 1 };
        s.reanchor(&["%01", "%00"], Some("%01"));
        assert_eq!(
            s.index, 0,
            "selection followed the pane to its new position"
        );
    }

    #[test]
    fn reanchor_clamps_when_pane_gone() {
        // The previously-highlighted pane vanished and the list shrank; reanchor clamps into range.
        let mut s = Selection { index: 1 };
        s.reanchor(&["%01"], Some("%00"));
        assert_eq!(s.index, 0);
    }

    // --- properties -----------------------------------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// After `reanchor`: a surviving anchor pins the selection to that pane's (first) position;
        /// otherwise the index is in-bounds (or 0 on an empty list, the clamp fallback).
        #[test]
        fn reanchor_seeks_anchor_else_stays_in_bounds(
            ids in prop::collection::vec("%[0-9]{1,3}", 0..12),
            start in 0usize..25,
            anchor in prop::option::of("%[0-9]{1,3}"),
        ) {
            let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let mut s = Selection { index: start };
            s.reanchor(&id_refs, anchor.as_deref());
            if ids.is_empty() {
                prop_assert_eq!(s.index, 0);
            } else {
                prop_assert!(s.index < ids.len());
                if let Some(pos) = anchor.as_ref().and_then(|a| ids.iter().position(|id| id == a)) {
                    prop_assert_eq!(s.index, pos);
                }
            }
        }

        /// `move_by` keeps the index in-bounds on any non-empty list across an arbitrary move
        /// sequence, and is a no-op (index unchanged) on the empty list.
        #[test]
        fn move_by_stays_in_bounds_over_sequences(
            len in 0usize..15,
            start in 0usize..30,
            deltas in prop::collection::vec(-5i32..6, 0..20),
        ) {
            let mut s = Selection { index: start };
            for d in deltas {
                let before = s.index;
                s.move_by(len, d);
                if len == 0 {
                    prop_assert_eq!(s.index, before);
                } else {
                    prop_assert!(s.index < len);
                }
            }
        }
    }
}
