//! The rendered preview and the pane it was captured for; recapture gating reads `for_pane`.

use ratatui::text::Text;

use crate::ansi::ansi_to_text;
use crate::effect::Effect;

/// The cached preview text plus its source pane; `None` `for_pane` means nothing is cached yet.
#[derive(Debug, Default)]
pub(crate) struct PreviewCache {
    pub(crate) text: Text<'static>,
    pub(crate) for_pane: Option<String>,
}

impl PreviewCache {
    /// The `PreviewCaptured` fold arm: store the rendered capture and mark it as the cached target.
    pub(crate) fn apply_captured(&mut self, pane: String, ansi: &str) {
        self.text = ansi_to_text(ansi);
        self.for_pane = Some(pane);
    }

    /// Capture the highlighted pane's preview iff it differs from the cached target; an empty
    /// selection clears the cache directly (nothing to capture).
    pub(crate) fn capture_effect(&mut self, sel_pane: Option<String>) -> Vec<Effect> {
        if sel_pane == self.for_pane {
            return vec![];
        }
        match sel_pane {
            Some(pane) => vec![Effect::CapturePreview { pane }],
            None => {
                self.text = Text::default();
                self.for_pane = None;
                vec![]
            }
        }
    }
}
