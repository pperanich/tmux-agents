#![deny(rustdoc::broken_intra_doc_links)]

//! tmux-agents display layer: the impure shell for the two TEA surfaces plus the single-shot render
//! verbs. `runner` owns the generic draw/input loop and the shared effect executor (the crossterm
//! poll mapped to core events, the SIGUSR1 nudge drain, and the tmux I/O each `Effect` names);
//! `picker`/`watch` are thin seams that seed from stamps, wire the nudge, and hold each surface's
//! `draw` fn, folding through [`tma_ui_core`]. `term`'s `TerminalGuard` owns raw mode, the alternate
//! screen, and watch's `@tma_watch_pid` advertisement, and the runner drops it before a deferred
//! jump. The non-loop verbs (`jump`, `menu`, `surfaces` for `ls`/`status`) stay here unchanged. No
//! `tma-tmux` dep: `Tmux`/`TmuxError` come through runtime's re-export; the only tmux side effects go
//! through [`tma_runtime::ui`] (preview capture, focus).

mod dash;
pub mod jump;
pub mod menu;
pub mod picker;
mod runner;
pub mod surfaces;
mod term;
#[cfg(test)]
mod test_render;
pub mod watch;
