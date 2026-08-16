use tma_core::{ActionKind, ActionManifest, GateOutcome, RefusalReason};
use tma_tmux::tmux::TmuxError;

use super::{api_requires_refusal, BrokerIo, PaneFacts, Refusal};

// ---- dry-run -----------------------------------------------------------------------------------

/// The `--dry-run` resolution: the resolved context (each value with its age), the gate
/// verdict, and the would-be effect — computed with no side effects (no re-verify, no lock, no
/// spawn), so the loop is edit → dry-run → fire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DryRun {
    pub action: String,
    pub pane: String,
    pub agent: Option<String>,
    pub context: Vec<ContextValue>,
    pub gate: DryGate,
    pub effect: Effect,
}

/// One resolved context variable and its staleness. `age_ms` is `Some` for a value derived from the
/// pane stamp (aged against `@agent_stamped_at`), `None` for a live tmux format read now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextValue {
    pub name: &'static str,
    pub value: String,
    pub age_ms: Option<u64>,
}

/// The dry-run gate verdict. `Vanished` when the pane is gone; otherwise the gate outcome computed on
/// the *stored* stamp (dry-run never re-verifies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DryGate {
    Fireable,
    Refused(Refusal),
    Vanished,
}

/// The effect the fire would have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A `keys` sequence for the resolved agent.
    Keys(Vec<String>),
    /// An API-channel call: the resolved endpoint, operation, and reply verdict.
    Api {
        endpoint: String,
        op: &'static str,
        reply: &'static str,
    },
    /// The `exec` command string (passed to `sh -c` verbatim).
    Command(String),
    /// Nothing resolvable (the pane is gone, or a `keys` action does not cover the agent).
    None,
}

/// Resolve a dry-run against `pane_id` with no side effects. Built over [`BrokerIo`] so it shares the
/// real read path (and is mockable), but it calls only [`BrokerIo::read_pane`].
pub fn dry_run<T: BrokerIo>(io: &T, action: &ActionManifest, pane_id: &str) -> DryRun {
    let now = io.now_ms();
    let base = |gate, agent, context, effect| DryRun {
        action: action.name.clone(),
        pane: pane_id.to_string(),
        agent,
        context,
        gate,
        effect,
    };

    let facts = match io.read_pane(pane_id) {
        Ok(Some(f)) => f,
        Ok(None) => return base(DryGate::Vanished, None, Vec::new(), Effect::None),
        Err(_) => return base(DryGate::Vanished, None, Vec::new(), Effect::None),
    };

    let agent = facts.agent.clone();
    let context = resolve_context(action, &facts, pane_id, now);

    // Gate on the stored stamp (no re-verify): dry-run shows the verdict it *would* apply, and the
    // per-value ages let the author see staleness for themselves. The API-lane `requires` is
    // reflected too, so a dry-run over a pane with no request id / endpoint shows `requires-unmet`.
    // A held single-flight lock rides last, exactly as `list_fireability` orders it (a gate reason
    // is the more useful verdict), read-only: dry-run peeks at the lock, never takes it. A failed
    // peek leaves the verdict alone — the fire path is what decides, and it re-reads.
    let gate = match agent.as_deref().filter(|a| action.applies_to(a)) {
        None => DryGate::Refused(Refusal::Gate(RefusalReason::WrongAgent)),
        Some(a) => match action.evaluate_gate(&facts.gate_input(a)) {
            GateOutcome::Fireable => match api_requires_refusal(action, &facts, a) {
                Some(r) => DryGate::Refused(r),
                None => match io.lock_held(pane_id, now) {
                    Ok(true) => DryGate::Refused(Refusal::Locked),
                    _ => DryGate::Fireable,
                },
            },
            GateOutcome::Refused(r) => DryGate::Refused(Refusal::Gate(r)),
        },
    };

    let effect = match action.kind {
        ActionKind::Keys => match agent.as_deref() {
            Some(a) => match action.api_for(a) {
                Some(t) => Effect::Api {
                    endpoint: facts
                        .api_endpoint
                        .clone()
                        .unwrap_or_else(|| "(unresolved)".to_string()),
                    op: t.op.token(),
                    reply: t.reply.token(),
                },
                None => action
                    .keys_for(a)
                    .map(|seq| Effect::Keys(seq.to_vec()))
                    .unwrap_or(Effect::None),
            },
            None => Effect::None,
        },
        ActionKind::Exec => Effect::Command(action.command.clone().unwrap_or_default()),
    };
    base(gate, agent, context, effect)
}

/// The `TMA_*` context table with per-value ages, for `--dry-run`. Stamp-derived values are aged
/// against `@agent_stamped_at`; live formats (cwd, title, locator, pane) and the action name are
/// fresh (`age_ms == None`).
fn resolve_context(
    action: &ActionManifest,
    facts: &PaneFacts,
    pane_id: &str,
    now: u64,
) -> Vec<ContextValue> {
    let stamp_age =
        (facts.stamped_at != 0 && now >= facts.stamped_at).then(|| now - facts.stamped_at);
    let stamped = |name, value: String| ContextValue {
        name,
        value,
        age_ms: stamp_age,
    };
    let live = |name, value: String| ContextValue {
        name,
        value,
        age_ms: None,
    };
    vec![
        live("TMA_PANE", pane_id.to_string()),
        stamped("TMA_AGENT", facts.agent.clone().unwrap_or_default()),
        stamped("TMA_STATE", facts.state.token().to_string()),
        stamped("TMA_DETAIL", facts.detail.clone().unwrap_or_default()),
        stamped("TMA_SESSION_ID", facts.session.clone().unwrap_or_default()),
        live("TMA_CWD", facts.cwd.clone()),
        stamped("TMA_PID", facts.pid.clone().unwrap_or_default()),
        live("TMA_LOCATOR", facts.locator.clone()),
        live("TMA_TITLE", facts.title.clone()),
        live("TMA_ACTION", action.name.clone()),
    ]
}

// ---- fireability for `tma act --list --pane` ---------------------------------------------------

/// One action's fireability verdict for `tma act --list --pane`: `None` when fireable,
/// `Some(refusal)` with the reason token otherwise. The `locked` reason rides here too (it is a
/// broker-time verdict, not a gate outcome), so a surface can gray a busy pane distinctly.
pub type ListVerdict = Option<Refusal>;

/// Per-action fireability of `actions` against `pane_id`, reading the pane facts and the lock
/// once. `Ok(None)` means the pane vanished; otherwise the returned vector is aligned with
/// `actions`. The gate reason wins over `locked` (an already-gated action stays gated even under a
/// held lock, mirroring the fire path, which refuses the gate before it reaches the lock).
pub fn list_fireability<T: BrokerIo>(
    io: &T,
    actions: &[ActionManifest],
    pane_id: &str,
) -> Result<Option<Vec<ListVerdict>>, TmuxError> {
    let now = io.now_ms();
    let facts = match io.read_pane(pane_id)? {
        Some(f) => f,
        None => return Ok(None),
    };
    let held = io.lock_held(pane_id, now)?;
    let verdicts = actions
        .iter()
        .map(
            |action| match facts.agent.as_deref().filter(|a| action.applies_to(a)) {
                None => Some(Refusal::Gate(RefusalReason::WrongAgent)),
                Some(agent) => match action.evaluate_gate(&facts.gate_input(agent)) {
                    // Otherwise fireable: the API-lane `requires` can still refuse, then the
                    // lock; the gate/requires reason wins over `locked`, mirroring the fire path.
                    GateOutcome::Fireable => api_requires_refusal(action, &facts, agent)
                        .or_else(|| held.then_some(Refusal::Locked)),
                    GateOutcome::Refused(r) => Some(Refusal::Gate(r)),
                },
            },
        )
        .collect();
    Ok(Some(verdicts))
}
