//! Pure detection core for tmux-agents.
//!
//! **Purity rule:** this crate is a pure function of its inputs — no tmux calls, no clock reads,
//! and (outside the optional `fixtures` feature) no I/O; every timestamp is injected as `u64`
//! epoch milliseconds, so the full manifest corpus is testable offline. The one exception is the
//! `fixtures` loader (`fixture::Fixture::load` / `load_dir`), test support never compiled into
//! consumer builds. Any change tempting a tmux call, a non-fixture file read, or a
//! `SystemTime::now` into this crate is mis-scoped and belongs in the `tma` binary.
#![deny(rustdoc::broken_intra_doc_links)]

pub mod action;
pub mod edge;
pub mod engine;
pub mod evidence;
#[cfg(feature = "fixtures")]
pub mod fixture;
pub mod fold;
pub mod manifest;
pub mod render;
pub mod row;
pub mod seen;
pub mod snapshot;
pub mod stamp;
pub mod state;
pub mod telemetry;
pub mod verdict;

pub use action::{
    ActionError, ActionKind, ActionManifest, ApiOp, ApiReply, ContextKeys, GateInput, GateOutcome,
    RefusalReason, Requirement, When,
};
pub use edge::{diff_rows, Edge};
pub use engine::{EngineError, Evaluation, RuleEngine, RuleReport};
pub use evidence::{Claim, Evidence, Lifecycle, Provenance, Source, StateClaim};
pub use fold::{verdict, FoldConfig, SnapshotFacts};
pub use manifest::{Channel, Manifest, ManifestError, Telemetry};
pub use render::{
    render_context, render_context_advisory, render_hold, render_publish, render_quota,
    render_quota_advisory, render_remove, render_summary, set_pane_option, summary_string,
    unset_pane_option, Guard, Publish, QuotaStamp, StampCommand, SummaryScope,
};
pub use row::{is_done, sort_rank, AgentRow, PendingCall, QuotaLabel, RepoLabel, Selector, StateToken};
pub use seen::{seen_by_input, ClientView};
pub use snapshot::{PaneSnapshot, ProcInfo};
pub use stamp::{ReadResult, StampedState};
pub use state::{AgentState, Detail, GrammarError};
pub use telemetry::{
    claude_statusline_model, codex_rollout_model, format_cost_usd, hook_payload_model,
    parse_context, parse_usage, ContextReport, QuotaReport, QuotaWindow, QuotaWindowReading,
    UsageReport,
};
pub use verdict::{Verdict, WinningEvidence, WriteAction, WritePlan};
