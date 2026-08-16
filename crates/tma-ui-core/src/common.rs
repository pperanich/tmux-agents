//! The fold state and arms both surfaces share: the 1 s refresh deadline and the preview cache,
//! plus the `Event` arms whose meaning does not depend on which surface is folding.

use ratatui::text::Text;

use crate::effect::{refresh_batch, Effect};
use crate::event::Event;
use crate::preview::PreviewCache;
use crate::refresh_gate::{RefreshGate, REFRESH_INTERVAL_MS};

/// Below this body width a surface shows no preview: `watch` stays the single-list MVP and the
/// picker gives the list the whole popup. 34 (fixed list pane, border included) plus 2 (preview
/// border) plus 40 (readable preview text) = 76, the narrowest width that fits both. The compare is
/// `>=`, so at 76 the preview gets its 40.
pub const PREVIEW_MIN_WIDTH: u16 = 76;

/// Whether a body `width` columns wide has room for a preview beside the list. One source of truth
/// for both surfaces' draw split and their capture gate, so neither can paint a pane it is not
/// capturing for (or capture for a pane it is not painting).
pub(crate) fn preview_fits(width: u16) -> bool {
    width >= PREVIEW_MIN_WIDTH
}

/// The picker's and `watch`'s shared fold state. Each model owns one and hands its events to
/// [`update`](Common::update) first; only what comes back `None` is the surface's own business.
#[derive(Debug)]
pub(crate) struct Common {
    gate: RefreshGate,
    preview: PreviewCache,
}

impl Common {
    /// A refresh gate armed at `now` over an empty preview cache.
    pub(crate) fn new(now: u64) -> Common {
        Common {
            gate: RefreshGate::new(now, REFRESH_INTERVAL_MS),
            preview: PreviewCache::default(),
        }
    }

    /// Fold the arms that mean the same thing on either surface: the guarded tick, the nudge, the
    /// failed refresh (stale rows kept, nothing emitted), and a landed capture. `None` hands `ev`
    /// back for the caller's surface-specific arms.
    pub(crate) fn update(&mut self, ev: &Event, now: u64) -> Option<Vec<Effect>> {
        match ev {
            Event::Tick => Some(refresh_batch(self.gate.on_tick(now))),
            Event::Nudge => Some(refresh_batch(self.gate.on_nudge(now))),
            Event::RefreshFailed => Some(vec![]),
            Event::PreviewCaptured { pane, ansi } => {
                self.preview.apply_captured(pane.clone(), ansi);
                Some(vec![])
            }
            _ => None,
        }
    }

    /// Capture `sel_pane`'s preview iff it differs from the cached target.
    pub(crate) fn capture(&mut self, sel_pane: Option<String>) -> Vec<Effect> {
        self.preview.capture_effect(sel_pane)
    }

    /// Forget the cached target but keep the text on screen: a refresh reanchors the selection, and
    /// the follow-up capture re-syncs the two.
    pub(crate) fn forget_target(&mut self) {
        self.preview.for_pane = None;
    }

    /// Drop text and target both: whatever is cached is stale once the preview stops being shown.
    pub(crate) fn drop_preview(&mut self) {
        self.preview.text = Text::default();
        self.preview.for_pane = None;
    }

    /// The cached preview text, for the draw.
    pub(crate) fn preview_text(&self) -> &Text<'static> {
        &self.preview.text
    }

    /// The pane the cached preview was captured for; the fold tests read the recapture gate here.
    #[cfg(test)]
    pub(crate) fn preview_target(&self) -> Option<&str> {
        self.preview.for_pane.as_deref()
    }
}
