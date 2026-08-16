//! The fold's single input variant: keys, time, signals, and effect results all arrive here.

use tma_core::AgentRow;

use crate::key::Key;

/// Every input to `update`; `now` arrives as a separate param, not a variant, so the core reads no
/// clock.
#[derive(Clone, Debug)]
pub enum Event {
    Key(Key),
    Tick,  // time may have advanced; now is an update param
    Nudge, // SIGUSR1, drained by the shell
    Resize { width: u16, height: u16 },
    RowsRefreshed(Vec<AgentRow>),
    RefreshFailed, // dash::refresh returned None; keep stale rows
    PreviewCaptured { pane: String, ansi: String },
}
