//! The fold's only I/O request; the shell's executor runs each and may feed back one event.

use tma_core::AgentRow;

/// What the core asks the shell to do; the core performs none of it itself.
/// No `PartialEq`: `AgentRow` derives none, so tests assert effects by pattern, not equality.
///
/// `ActMenu` carries only a pane id: which actions exist there, and whether any is fireable right
/// now, is the broker's knowledge, not the fold's. The executor re-invokes `tma act --menu` for it.
#[derive(Clone, Debug)]
pub enum Effect {
    Refresh,                         // executor: dash::refresh -> RowsRefreshed | RefreshFailed
    CapturePreview { pane: String }, // executor: ui::capture_preview -> PreviewCaptured
    Focus(Box<AgentRow>),            // executor: jump::focus_agent (boxed, AgentRow is wide)
    ClearAttention { pane: String }, // executor: tmux.unset_pane_option(pane, ATTENTION)
    ActMenu { pane: String },        // executor: run-shell -b `tma act --menu --pane <id>`
    Quit,
}

/// The refresh batch a fold emits from a gate decision: one [`Refresh`](Effect::Refresh) when due,
/// nothing otherwise. Keeps [`RefreshGate`](crate::refresh_gate::RefreshGate) free of the `Effect`
/// vocabulary: the gate returns a bool and the fold lifts it here.
pub(crate) fn refresh_batch(due: bool) -> Vec<Effect> {
    if due {
        vec![Effect::Refresh]
    } else {
        vec![]
    }
}
