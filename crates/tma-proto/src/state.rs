//! The state pair a row carries, in the host's own tokens.
//!
//! Both vocabularies are copied from `tma-core`, not re-invented, and
//! `crates/tma/tests/proto_drift.rs` fails when the host grows a token this crate has no arm for.

vocabulary! {
    /// The closed, frozen published state vocabulary (`tma_core::AgentState`): whose move it is.
    /// Closed on purpose. An escape hatch here would invent a state a pane cannot be observed in,
    /// and the qualification a newer host wants to add belongs in [`Detail`], which is open.
    pub enum State {
        /// Prompt shown, nothing running.
        Idle = "idle",
        /// The ball is with the agent.
        Working = "working",
        /// The ball is with the human.
        Blocked = "blocked",
        /// Recognized agent, unreadable evidence.
        Unknown = "unknown",
    }
}

vocabulary! {
    /// The open detail dimension (`tma_core::Detail`): why the pane is in its state.
    ///
    /// An unrecognized token lands in `Other` and stays there, which is what stops an older device
    /// reading a newer host's dialog as a permission prompt (R21). The card builder sends `Other` to
    /// an informational card, so the degradation is "I cannot type this dialog", never a wrong
    /// affordance.
    pub open enum Detail {
        Permission = "permission",
        /// A plan-approval dialog: its affirmative option grants every following action.
        Plan = "plan",
        /// A workspace-trust gate: its affirmative option grants the whole folder.
        Trust = "trust",
        /// A question asked mid-turn. There is nothing to grant, so approve/deny do not resolve it.
        Question = "question",
        Error = "error",
        RateLimit = "rate_limit",
        Background = "background",
        Compacting = "compacting",
    }
}

vocabulary! {
    /// The state vocabulary a [`crate::Selector`] filters on: the four stored tokens plus `done`,
    /// which is idle with the attention flag still up. Matches `tma_core::StateToken`.
    pub enum StateFilter {
        Idle = "idle",
        Working = "working",
        Blocked = "blocked",
        Unknown = "unknown",
        Done = "done",
    }
}
