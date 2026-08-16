//! The output of the detection fold: a [`Verdict`] and the [`WritePlan`] that tells
//! the write adapter how to commit it.

use crate::evidence::Provenance;
use crate::state::{AgentState, Detail};

/// What the write adapter should do with a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAction {
    /// Commit the new state tuple (guarded).
    Publish,
    /// Freeze/suppress: refresh `@agent_stamped_at` and `@agent_hash` only, never
    /// state, never `@agent_evidence_at`.
    Hold,
}

/// How a verdict is to be written, precomputed by the pure fold so the adapter carries
/// no arbitration logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePlan {
    pub action: WriteAction,
    /// Whether this producer's evidence may override a stamped hook claim — feeds the
    /// write guard. Meaningless when `action` is `Hold`.
    pub may_override: bool,
    /// Set the attention flag: a noteworthy transition (blocked; working→idle completion).
    pub set_attention: bool,
    /// Pid-change episode boundary: reset `since` / `notified_at` / attention this cycle.
    pub episode_reset: bool,
}

/// The evidence that decided a verdict, named for `tma debug explain`. Works for both
/// fresh evidence and a hold-previous outcome (where the decider is the prior stamp).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinningEvidence {
    /// Coarse provenance bucket of the deciding evidence.
    pub source: Provenance,
    /// When that evidence was produced (epoch milliseconds, injected).
    pub at: u64,
    /// Rule id / hook name / matcher, or a sentinel like `hold-previous`.
    pub label: String,
}

/// The fold's decision for one pane in one cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub state: AgentState,
    pub detail: Option<Detail>,
    pub winning_evidence: WinningEvidence,
    pub writes: WritePlan,
}
