//! `tma-daemon`: the tier-3 event-hub daemon.
//!
//! Strictly additive: with no daemon running, `tma event` direct-stamps and every one-shot works
//! unchanged. This crate holds the parts that are genuinely tier 3, the serve loop ([`run_cli`])
//! and the notification *dispatch* state machine ([`notify::NotifyState`]), atop the tier-2
//! [`tma_runtime`] crate. The wire protocol ([`tma_runtime::ipc`]) and the single-fire primitive
//! ([`tma_runtime::notify::fire`]) live in runtime so tier-2 `tma event` reaches them without
//! depending on this crate; the daemon imports them for the server side.
//!
//! The binary depends on this crate for exactly one thing: dispatching the `tma daemon`
//! subcommand ([`run_cli`]).
#![deny(rustdoc::broken_intra_doc_links)]

mod daemon;
mod notify;

pub use daemon::{evict_older_daemon, run_cli, DaemonOpts};
