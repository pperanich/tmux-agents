//! The tmux I/O crate: the one crate that spawns `tmux`.
//!
//! - [`tmux`]: the read-only subprocess adapter (`list-panes`/`capture-pane` formats) plus
//!   `ps_all`, the other half of the read path.
//! - [`control`]: the daemon's per-session `tmux -C` control-mode client pool.
//! - [`stamp`]: the guarded write adapter that renders a verdict into a chained, server-side
//!   guarded `set-option` invocation and applies it.
//! - [`lock`]: the `@agent_action` single-flight lock, a server-side conditional acquire/reclaim
//!   with nonce read-back plus nonce-conditional clear/rewrite (the action broker's mutex).
//!
//! Everything above this crate talks to tmux through these modules, so the I/O choke point is one
//! crate the compiler can police.
#![deny(rustdoc::broken_intra_doc_links)]

pub mod control;
pub mod lock;
pub mod stamp;
pub mod tmux;
