//! The fold's single input variant: keys, time, signals, and effect results all arrive here.

use tma_core::AgentRow;

use crate::key::Key;

/// Every input to `update`; `now` arrives as a separate param, not a variant, so the core reads no
/// clock.
#[derive(Clone, Debug)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    Tick,  // time may have advanced; now is an update param
    Nudge, // SIGUSR1, drained by the shell
    Resize { width: u16, height: u16 },
    RowsRefreshed(Vec<AgentRow>),
    RefreshFailed, // dash::refresh returned None; keep stale rows
    PreviewCaptured { pane: String, ansi: String },
}

/// One mouse report, in 0-based terminal cell coordinates (crossterm's, which tmux has already
/// translated to the pane or popup the surface owns — a click outside it never arrives).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
}

/// The mouse gestures the folds act on. Drags, other buttons, and modifier chords are dropped by
/// the shell rather than carried here: the surfaces are a list and a preview, so press, hover, and
/// the wheel are the whole vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    /// Left button pressed.
    Down,
    /// Pointer moved with no button held (hover).
    Moved,
    ScrollUp,
    ScrollDown,
}
