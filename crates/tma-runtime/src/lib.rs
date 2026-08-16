//! `tma-runtime` — the tier-2 detection pipeline.
//!
//! Everything above the tmux I/O choke point ([`tma_tmux`]) and below the binary's dispatch: config
//! and manifest loading, pane identity, the poll cycle, on-demand capture, the hook-event bridge
//! ([`event`]), the daemon wire protocol ([`ipc`]), the surface subscribe stream ([`subscribe`]),
//! the notify primitive ([`notify`]), the SIGUSR1 sidebar nudge ([`nudge`]), the sidebar toggle
//! ([`sidebar`]), and the debug surfaces.
//! Pure domain logic lives in [`tma_core`].
//! Dep edges (acyclic): `tma-core ← tma-tmux ← tma-runtime ← tma-daemon`; `tma event` works with no
//! daemon, and only [`ipc::DaemonSink`] speaks the wire protocol.
#![deny(rustdoc::broken_intra_doc_links)]

// --- detection pipeline ----------------------------------------------------------
// What makes a pane an agent pane, what it is doing, and the inputs that decide: the poll
// cycle and its capture fallback, the hook-event bridge, and the per-pane metadata they read.
pub mod capture;
pub mod config;
pub mod cycle;
pub mod event;
pub mod identity;
pub mod manifests;
pub mod origin;
pub mod repo;
// The cycle is `rollout`'s only in-crate caller, but `tests/rollout_integration.rs` drives the tail
// directly, so the module stays public.
pub mod rollout;

// --- protocol and state ----------------------------------------------------------
// The daemon wire and the two streams riding it: surface subscriptions and transition history.
pub mod ipc;
pub mod subscribe;
pub mod transitions;

// --- surfaces and helpers --------------------------------------------------------
// What the bin and `tma-ui` call once state exists: acting on a pane, notifying, the sidebar
// toggle, the debug tools, and the shared primitives (`http` is internal to the broker).
pub mod actions;
pub mod broker;
pub mod debug;
mod http;
pub mod json;
pub mod notify;
pub mod nudge;
pub mod sidebar;
pub mod ui;

/// The tmux I/O handle and its error, re-exported so consumers can name the type runtime's public
/// API already requires (`cycle::run_cycle(&Tmux)`) without a direct [`tma_tmux`] dependency.
pub use tma_tmux::tmux::{escape_menu_label, MenuItem, Server, Tmux, TmuxError};

/// Wall-clock epoch in milliseconds (0 before the epoch). The one home for every epoch-ms read in
/// the workspace: the stamp grammar's unit, so the poll cycle, the hook bridge, capture, the
/// daemon's notify dispatch, and the picker/`watch` display all read the clock through here.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
