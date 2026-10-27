//! Frozen guarded transactions for program state and independently owned metadata.
//!
//! Bodies contain only plain set-option operations. Effective after-set-option hooks are
//! rejected at execution time because a yielding hook would permit another client to interleave.
//! A unique receipt and full readback of every edited option are required to report a win.
//! Injected epoch clocks are sampled at submission and, for live authority or effects, again
//! after readback. Callers also confirm the paired monotonic schedule before using a renewed
//! coverage epoch; an IO operation cannot detect a monotonic deadline by itself.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use tma_core::persistence::{
    opt, BlockedHistory, CollectorLease, CommitMarker, MetadataEvidence, MetadataOwner,
    MetadataUpdate, NotificationClaim, ObservationIdentity, PaneOwner, PersistedResult,
    ProgramMetadata, ProjectionRead, ResultHistory, Token, LEASE_DURATION_MS, METADATA_KEYS,
};
use tma_core::render::{self, Guard, Publish, StampCommand};
use tma_core::stamp::opt as public;
use tma_core::StampedState;

use crate::tmux::{Tmux, TmuxError};

const NO_HOOKS: &str = "#{==:#{after-set-option},}";
static UNSUPPORTED_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error("cannot obtain persistence identity: {0}")]
    Random(#[source] std::io::Error),
    #[error("invalid persistence transaction: {0}")]
    Invalid(&'static str),
    #[error("program authority disabled: conditional command groups are unavailable or after-set-option hooks are configured")]
    Unsupported,
}

pub fn fresh_token() -> Result<Token, PersistenceError> {
    let nonce = crate::lock::fresh_nonce().map_err(PersistenceError::Random)?;
    Token::parse(&nonce).ok_or(PersistenceError::Invalid("invalid random token"))
}

/// One coherent options read. A receipt is a write identity, never a clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneRead {
    pub pane_id: String,
    pub target: String,
    pub options: HashMap<String, String>,
    pub option_hooks: bool,
    pub program: ProgramMetadata,
}

impl PaneRead {
    /// Snapshot already obtained by list-panes, including the server lease. Hook support is
    /// checked at execution time by every complete transaction.
    pub fn from_options(pane_id: String, options: HashMap<String, String>) -> Self {
        let program = ProgramMetadata::from_options(&options);
        Self {
            target: pane_id.clone(),
            pane_id,
            options,
            program,
            option_hooks: false,
        }
    }

    pub fn owner(&self) -> Option<PaneOwner> {
        self.program.owner.clone()
    }
    pub fn result(&self) -> Option<PersistedResult> {
        self.program.result.clone()
    }
    pub fn projection(&self, now: u64) -> Result<ProjectionRead, tma_core::GrammarError> {
        self.program.projection(&self.options, now)
    }
    fn value(&self, key: &str) -> &str {
        self.options.get(key).map_or("", String::as_str)
    }
}

/// A loss includes the persisted winner so rows and effects can use stored truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Won(PaneRead),
    Lost(PaneRead),
}

impl CommitOutcome {
    pub fn winner(&self) -> &PaneRead {
        match self {
            Self::Won(read) | Self::Lost(read) => read,
        }
    }
    pub fn won(&self) -> bool {
        matches!(self, Self::Won(_))
    }
}

/// A literal option mutation. No report text is evaluated as a format or a shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionEdit {
    pub key: &'static str,
    pub value: Option<String>,
}

/// An advisory same-second cycle hint, never state or effect permission.
/// A later writer may replace the receipt before this caller confirms its readback.
#[derive(Clone, Copy, Debug)]
pub struct PollHint(u64);

impl PollHint {
    pub fn new(at: u64) -> Self {
        Self(at)
    }
}

impl OptionEdit {
    pub fn set(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: Some(value.into()),
        }
    }
    pub fn unset(key: &'static str) -> Self {
        Self { key, value: None }
    }
    fn command(&self, pane: &str) -> StampCommand {
        match &self.value {
            Some(value) => render::set_pane_option(pane, self.key, value),
            None => render::unset_pane_option(pane, self.key),
        }
    }
}

/// Convert the existing closed plan vocabulary to literal mutations for a guarded group.
/// The caller supplies the freshly read prior tuple for legacy write-once SINCE behavior.
/// This conversion carries no ownership decision or success prediction.
pub fn edits_for_plan(
    pane: &str,
    plan: &crate::stamp::StampPlan,
    previous: Option<&StampedState>,
) -> Result<Vec<OptionEdit>, PersistenceError> {
    let commands = match plan {
        crate::stamp::StampPlan::Publish(publish) => {
            if publish.pane_id != pane {
                return Err(PersistenceError::Invalid("plan pane mismatch"));
            }
            render::render_publish_advisory(
                publish,
                previous.map(|s| s.state),
                previous.map_or(0, |s| s.since),
            )
        }
        crate::stamp::StampPlan::Hold { stamped_at, hash } => {
            render::render_hold(pane, *stamped_at, *hash)
        }
        crate::stamp::StampPlan::Remove => render::render_remove(pane),
        crate::stamp::StampPlan::EndSession => render::render_end_session(pane),
    };
    commands
        .iter()
        .map(edit_from_plain)
        .filter(|edit| !edit.as_ref().is_ok_and(|e| METADATA_KEYS.contains(&e.key)))
        .collect()
}

/// All legacy guards compose with ownership, including nominally unconditional writers.
#[derive(Clone, Debug)]
pub enum Ownership {
    /// Fallback may replace only absent, expired or incomplete protocol authority.
    Fallback { now: u64 },
    /// The caller has freshly rechecked this complete process/stream binding.
    Program { owner: PaneOwner, now: u64 },
    /// Cleanup is exact-owner conditional, including after lease expiry.
    ExactOwner(PaneOwner),
    /// User acknowledgement and notification compare the complete observed result.
    ExactResult(PersistedResult),
    /// Hook/API metadata has an independent process/session lifetime. Binding a new owner
    /// additionally requires runtime's fresh process/session checks.
    Metadata { expected: Option<MetadataOwner> },
}

fn option(key: &str) -> String {
    format!("#{{{key}}}")
}
fn equal(key: &str, value: &str) -> String {
    format!(
        "#{{==:{},{}}}",
        option(key),
        render::literal_tmux_format(value)
    )
}
fn and(a: &str, b: &str) -> String {
    format!("#{{&&:{a},{b}}}")
}
fn conjunction(checks: &[String]) -> String {
    checks
        .iter()
        .rev()
        .fold("1".to_owned(), |rest, check| and(check, &rest))
}
fn not(a: &str) -> String {
    format!("#{{?{a},0,1}}")
}
// Bind a contextual validity decision to every input used by the typed projection reader.
// In particular a lease renewal does not advance the pane receipt. Comparing its exact value
// prevents a delayed writer from treating that renewal as an implausibly future stale lease.
pub(crate) const PROJECTION_KEYS: &[&str] = &[
    opt::ORIGIN,
    opt::OWNER,
    opt::COMMIT,
    opt::COLLECTOR,
    public::STATE,
    public::SOURCE,
    public::PID,
    public::EVIDENCE_AT,
    public::SINCE,
    public::STAMPED_AT,
    public::NOTIFIED_AT,
    public::TURN_AT,
    public::HASH,
];

fn projection_unchanged(observed: &PaneRead) -> String {
    // tmux rescans each nested operand. Keep the large OWNER and COMMIT comparisons near the
    // root to avoid repeatedly parsing a maximum process chain under its format time limit.
    conjunction(
        &PROJECTION_KEYS
            .iter()
            .map(|key| equal(key, observed.value(key)))
            .collect::<Vec<_>>(),
    )
}

fn live_projection(observed: &PaneRead, now: u64) -> String {
    if matches!(observed.projection(now), Ok(ProjectionRead::Live { .. })) {
        projection_unchanged(observed)
    } else {
        "0".into()
    }
}

fn ownership_predicate(
    ownership: &Ownership,
    observed: &PaneRead,
) -> Result<String, PersistenceError> {
    let predicate = match ownership {
        Ownership::Fallback { now } => {
            if matches!(observed.projection(*now), Ok(ProjectionRead::Live { .. })) {
                "0".into()
            } else {
                projection_unchanged(observed)
            }
        }
        Ownership::Program { owner, now } => {
            let encoded = owner.encode();
            if PaneOwner::parse(&encoded).as_ref() != Some(owner) {
                return Err(PersistenceError::Invalid("invalid program owner"));
            }
            match &observed.program.lease {
                Some(lease) if lease.token == owner.collector && lease.live_at(*now) => and(
                    &equal(opt::COLLECTOR, &lease.encode()),
                    &projection_unchanged(observed),
                ),
                _ => "0".into(),
            }
        }
        Ownership::ExactOwner(owner) => {
            if PaneOwner::parse(&owner.encode()).as_ref() != Some(owner) {
                return Err(PersistenceError::Invalid("invalid exact owner"));
            }
            equal(opt::OWNER, &owner.encode())
        }
        Ownership::ExactResult(result) => {
            if PersistedResult::parse(&result.encode()).as_ref() != Some(result) {
                return Err(PersistenceError::Invalid("invalid exact result"));
            }
            and(
                &equal(opt::OWNER, &result.owner.encode()),
                &equal(opt::RESULT, &result.encode()),
            )
        }
        Ownership::Metadata { expected } => equal(
            opt::METADATA_OWNER,
            &expected
                .as_ref()
                .map_or_else(String::new, MetadataOwner::encode),
        ),
    };
    Ok(predicate)
}

fn effective_guard(guard: Guard, ownership: &Ownership) -> String {
    let suppress = guard.suppress_expr();
    // An expired protocol projection's compatibility hook token is stale evidence.
    let suppress = if matches!(ownership, Ownership::Fallback { .. })
        && !matches!(guard, Guard::RefreshClaim { .. })
    {
        let absent_program = and(
            &equal(opt::ORIGIN, ""),
            &and(&equal(opt::OWNER, ""), &equal(opt::COMMIT, "")),
        );
        and(&absent_program, &suppress)
    } else {
        suppress
    };
    not(&suppress)
}

/// Targeted coherent read, with all edited fields included. A control separator in option text
/// makes the read fail closed rather than silently returning a different tuple.
pub fn read_pane(tmux: &Tmux, pane: &str) -> Result<PaneRead, PersistenceError> {
    read_pane_with(tmux, pane, &[])
}

fn read_pane_with(
    tmux: &Tmux,
    pane: &str,
    edits: &[OptionEdit],
) -> Result<PaneRead, PersistenceError> {
    let mut keys: Vec<_> = crate::tmux::pane_option_keys().collect();
    for edit in edits {
        if !keys.contains(&edit.key) {
            keys.push(edit.key);
        }
    }
    // Terminal sentinel preserves trailing newlines in the last option value.
    let format = keys
        .iter()
        .map(|key| option(key))
        .chain(["#{after-set-option}".into(), "end".into()])
        .collect::<Vec<_>>()
        .join("\u{1f}");
    let text = tmux.pane_format(pane, &format!("#{{pane_id}}\u{1f}{format}"))?;
    let mut all_fields = text.split('\u{1f}');
    let pane_id = all_fields
        .next()
        .ok_or(PersistenceError::Invalid("missing pane id"))?;
    if tma_core::program_status::PaneId::parse(pane_id).is_err() {
        if let Ok(requested) = tma_core::program_status::PaneId::parse(pane) {
            if pane == format!("%{}", requested.get()) {
                let membership = tmux
                    .list_panes()?
                    .into_iter()
                    .map(|row| tma_core::program_status::PaneId::parse(&row.pane_id))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| PersistenceError::Invalid("invalid fresh pane membership"))?;
                if !membership.contains(&requested) {
                    return Err(TmuxError::PaneGone { pane: requested }.into());
                }
            }
        }
        return Err(PersistenceError::Invalid("invalid pane id"));
    }
    let fields: Vec<_> = all_fields.collect();
    if fields.len() != keys.len() + 2 || fields.last() != Some(&"end") {
        return Err(PersistenceError::Invalid("ambiguous options readback"));
    }
    let option_hooks = !fields[keys.len()].is_empty();
    let options = keys
        .into_iter()
        .zip(fields)
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    let program = ProgramMetadata::from_options(&options);
    Ok(PaneRead {
        pane_id: pane_id.to_owned(),
        target: pane.to_owned(),
        options,
        option_hooks,
        program,
    })
}

fn validate_edits(edits: &[OptionEdit]) -> Result<(), PersistenceError> {
    if edits.iter().any(|e| {
        !e.key.starts_with('@')
            || !e.key[1..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || e.value.as_ref().is_some_and(|v| v.contains('\0'))
    }) {
        return Err(PersistenceError::Invalid("invalid option key/value"));
    }
    Ok(())
}

fn transaction(
    tmux: &Tmux,
    observed: &PaneRead,
    ownership: &Ownership,
    guard: Guard,
    extra_predicate: &str,
    edits: &[OptionEdit],
) -> Result<CommitOutcome, PersistenceError> {
    transaction_with_hint(
        tmux,
        observed,
        ownership,
        guard,
        extra_predicate,
        edits,
        None,
    )
}

fn transaction_with_hint(
    tmux: &Tmux,
    observed: &PaneRead,
    ownership: &Ownership,
    guard: Guard,
    extra_predicate: &str,
    edits: &[OptionEdit],
    hint: Option<PollHint>,
) -> Result<CommitOutcome, PersistenceError> {
    validate_edits(edits)?;
    let write = fresh_token()?;
    let predicate = and(
        NO_HOOKS,
        &and(
            &ownership_predicate(ownership, observed)?,
            &and(
                &effective_guard(guard, ownership),
                &and(
                    extra_predicate,
                    &equal(opt::RECEIPT, observed.value(opt::RECEIPT)),
                ),
            ),
        ),
    );
    let mut body: Vec<_> = edits
        .iter()
        .map(|edit| edit.command(&observed.target))
        .collect();
    body.push(OptionEdit::set(opt::RECEIPT, write.as_str()).command(&observed.target));
    let mut commands = vec![render::conditional_group(
        &observed.target,
        &predicate,
        &body,
    )];
    if let Some(PollHint(at)) = hint {
        let command = StampCommand {
            argv: vec![
                "set-option".into(),
                "-s".into(),
                "-t".into(),
                observed.target.clone(),
                public::LAST_POLL.into(),
                at.to_string(),
            ],
        };
        commands.push(render::conditional_group(
            &observed.target,
            &and(NO_HOOKS, &equal(opt::RECEIPT, write.as_str())),
            &[command],
        ));
    }
    tmux.apply_persistence(&observed.target, &commands)?;
    let persisted = read_pane_with(tmux, &observed.target, edits)?;
    if persisted.option_hooks {
        return Err(PersistenceError::Unsupported);
    }
    // Do not infer success from tmux's exit status or receipt alone. Last edit wins on duplicate
    // keys, just as it does in tmux. A partial body can never satisfy all final assertions.
    let expected: HashMap<_, _> = edits
        .iter()
        .map(|e| (e.key, e.value.as_deref().unwrap_or("")))
        .collect();
    let won = persisted.value(opt::RECEIPT) == write.as_str()
        && expected
            .into_iter()
            .all(|(key, value)| persisted.value(key) == value);
    Ok(if won {
        CommitOutcome::Won(persisted)
    } else {
        CommitOutcome::Lost(persisted)
    })
}

/// Apply a complete state transaction. `edits` must include every state-dependent companion
/// (requests, attention, turn, PID/name/hash/freshness) that the producer intends to change.
/// Independent hook metadata uses `write_metadata` instead.
/// `clock` must sample the current epoch time, rather than return a previously captured stamp.
pub fn write_fallback(
    tmux: &Tmux,
    observed: &PaneRead,
    guard: Guard,
    clock: impl FnMut() -> u64,
    edits: &[OptionEdit],
) -> Result<CommitOutcome, PersistenceError> {
    write_fallback_with_hint(tmux, observed, guard, clock, edits, None)
}

/// Publish the normal fallback tuple and optionally batch one exact-receipt poll hint.
/// The hint certifies complete server execution before client readback, not a later Won.
pub fn write_fallback_with_hint(
    tmux: &Tmux,
    observed: &PaneRead,
    guard: Guard,
    mut clock: impl FnMut() -> u64,
    edits: &[OptionEdit],
    hint: Option<PollHint>,
) -> Result<CommitOutcome, PersistenceError> {
    reject_private_edits(edits)?;
    let mut body = vec![OptionEdit::unset(opt::COMMIT)];
    body.extend_from_slice(edits);
    add_episode(observed, edits, &mut body)?;
    body.push(OptionEdit::unset(opt::OWNER));
    if edits.iter().any(|edit| edit.key == public::STATE) {
        body.push(OptionEdit::unset(opt::ORIGIN));
    }
    transaction_with_hint(
        tmux,
        observed,
        &Ownership::Fallback { now: clock() },
        guard,
        "1",
        &body,
        hint,
    )
}

/// Legacy continuation when complete nonyielding groups are unavailable. Each invocation
/// contains exactly one conditional mutation, and checks current live ownership immediately
/// before that mutation. `clock` is sampled again after each possibly yielding hook. A newly
/// acquired protocol owner therefore suppresses the remaining advisory suffix.
///
/// This returns persisted truth only: partial advisory progress is possible and MUST NOT be
/// interpreted as a complete transaction win or permission to emit a result effect.
pub fn advisory_fallback(
    tmux: &Tmux,
    initial: &PaneRead,
    guard: Guard,
    edits: &[OptionEdit],
    mut clock: impl FnMut() -> u64,
) -> Result<PaneRead, PersistenceError> {
    let pane = &initial.target;
    reject_private_edits(edits)?;
    validate_edits(edits)?;
    let mut body = vec![OptionEdit::unset(opt::COMMIT)];
    body.extend_from_slice(edits);
    add_episode(initial, edits, &mut body)?;
    body.push(OptionEdit::unset(opt::OWNER));
    if edits.iter().any(|edit| edit.key == public::STATE) {
        body.push(OptionEdit::unset(opt::ORIGIN));
    }
    let write = fresh_token()?;
    let initial_ownership = Ownership::Fallback { now: clock() };
    let admission = and(
        &ownership_predicate(&initial_ownership, initial)?,
        &and(
            &effective_guard(guard, &initial_ownership),
            &equal(opt::RECEIPT, initial.value(opt::RECEIPT)),
        ),
    );
    tmux.apply_persistence(
        pane,
        &[render::conditional_group(
            pane,
            &admission,
            &[OptionEdit::set(opt::RECEIPT, write.as_str()).command(pane)],
        )],
    )?;
    for edit in &body {
        let observed = read_pane(tmux, pane)?;
        let now = clock();
        let ownership = Ownership::Fallback { now };
        let predicate = and(
            &ownership_predicate(&ownership, &observed)?,
            &and(
                &and(
                    &equal(opt::RECEIPT, write.as_str()),
                    &equal(opt::METADATA_OWNER, initial.value(opt::METADATA_OWNER)),
                ),
                &effective_guard(guard, &ownership),
            ),
        );
        tmux.apply_persistence(
            pane,
            &[render::conditional_group(
                pane,
                &predicate,
                &[edit.command(pane)],
            )],
        )?;
    }
    read_pane_with(tmux, pane, &body)
}

/// Independently owned metadata continuation under yielding hooks. Every individual edit checks
/// the same expected process/session owner. No complete transaction success is claimed.
pub fn advisory_metadata(
    tmux: &Tmux,
    observed: &PaneRead,
    expected: Option<&MetadataOwner>,
    next: Option<&MetadataOwner>,
    edits: &[OptionEdit],
) -> Result<PaneRead, PersistenceError> {
    advisory_metadata_observation(tmux, observed, expected, next, edits, None)
}

fn metadata_admission(observed: &PaneRead, evidence: Option<MetadataEvidence>) -> String {
    let mut predicate = equal(opt::METADATA_UPDATE, observed.value(opt::METADATA_UPDATE));
    if let Some(evidence) = evidence {
        let pending = MetadataUpdate::parse(observed.value(opt::METADATA_UPDATE))
            .and_then(|update| update.evidence);
        let newer_pending = pending
            .is_some_and(|pending| pending.key() == evidence.key() && pending.at() > evidence.at());
        let previous = observed.value(evidence.key()).parse::<u64>().unwrap_or(0);
        if newer_pending || previous > evidence.at() {
            return "0".into();
        }
        predicate = and(
            &predicate,
            &equal(evidence.key(), observed.value(evidence.key())),
        );
    }
    predicate
}

/// Establish a unique update claim, then bind the next owner, then apply the guarded suffix.
/// A newer same-owner update replaces the claim and stops the old suffix at its next mutation.
pub fn advisory_metadata_observation(
    tmux: &Tmux,
    observed: &PaneRead,
    expected: Option<&MetadataOwner>,
    next: Option<&MetadataOwner>,
    edits: &[OptionEdit],
    evidence: Option<MetadataEvidence>,
) -> Result<PaneRead, PersistenceError> {
    validate_metadata(expected, next, edits)?;
    validate_edits(edits)?;
    let update = MetadataUpdate {
        token: fresh_token()?,
        evidence,
    }
    .encode();
    let admission = and(
        &metadata_admission(observed, evidence),
        &and(
            &equal(
                opt::METADATA_OWNER,
                &expected.map_or_else(String::new, MetadataOwner::encode),
            ),
            &equal(opt::RECEIPT, observed.value(opt::RECEIPT)),
        ),
    );
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &admission,
            &[OptionEdit::set(opt::METADATA_UPDATE, &update).command(&observed.target)],
        )],
    )?;
    let claim = and(
        &equal(opt::METADATA_UPDATE, &update),
        &equal(opt::RECEIPT, observed.value(opt::RECEIPT)),
    );
    let bind = and(
        &claim,
        &equal(
            opt::METADATA_OWNER,
            &expected.map_or_else(String::new, MetadataOwner::encode),
        ),
    );
    let next_value = next.map_or_else(String::new, MetadataOwner::encode);
    let owner_edit = if next.is_some() {
        OptionEdit::set(opt::METADATA_OWNER, &next_value)
    } else {
        OptionEdit::unset(opt::METADATA_OWNER)
    };
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &bind,
            &[owner_edit.command(&observed.target)],
        )],
    )?;
    for edit in edits {
        let mut predicate = and(&claim, &equal(opt::METADATA_OWNER, &next_value));
        if let Some(evidence) = evidence {
            predicate = and(
                &predicate,
                &format!(
                    "#{{||:#{{==:{},}},#{{e|<=:{},{}}}}}",
                    option(evidence.key()),
                    option(evidence.key()),
                    evidence.at()
                ),
            );
        }
        tmux.apply_persistence(
            &observed.target,
            &[render::conditional_group(
                &observed.target,
                &predicate,
                &[edit.command(&observed.target)],
            )],
        )?;
    }
    read_pane_with(tmux, &observed.target, edits)
}

/// An old focus/input clear cannot consume a newer result even when an option hook yields.
/// The ACK marker is historical and exact; this API never authorizes a notification effect.
pub fn advisory_acknowledge(
    tmux: &Tmux,
    observed: &PaneRead,
    result: &PersistedResult,
) -> Result<PaneRead, PersistenceError> {
    let predicate = and(
        &not(&equal(opt::INVALIDATED, &result.encode())),
        &and(
            &equal(public::STATE, "idle"),
            &ownership_predicate(&Ownership::ExactResult(result.clone()), observed)?,
        ),
    );
    let edits = [
        OptionEdit::set(opt::ACK, result.encode()),
        OptionEdit::unset(public::ATTENTION),
    ];
    for (index, edit) in edits.iter().enumerate() {
        // Durable acknowledgement precedes its visible clear. An unpresented/deferred result
        // has no attention and cannot acquire an ACK. A delayed clear must still match both
        // the result and its durable acknowledgement after a yielding hook.
        let condition = and(
            &predicate,
            &if index == 0 {
                equal(public::ATTENTION, "1")
            } else {
                equal(opt::ACK, &result.encode())
            },
        );
        tmux.apply_persistence(
            &observed.target,
            &[render::conditional_group(
                &observed.target,
                &condition,
                &[edit.command(&observed.target)],
            )],
        )?;
    }
    read_pane_with(tmux, &observed.target, &edits)
}

fn reject_private_edits(edits: &[OptionEdit]) -> Result<(), PersistenceError> {
    if edits
        .iter()
        .any(|e| opt::PANE_KEYS.contains(&e.key) || e.key == opt::COLLECTOR)
    {
        return Err(PersistenceError::Invalid(
            "private marker edits require their ownership API",
        ));
    }
    Ok(())
}

/// Publish a freshly rechecked program selection. Compatibility source is forced to hook here,
/// and the birth-bound agent PID must match the normalized owner. Result identity is independent
/// of state equality, so idle-to-idle results can advance turn/attention through the supplied tuple.
/// A lease that expires during IO returns Lost even if the complete tuple reached the server.
pub fn publish_program(
    tmux: &Tmux,
    observed: &PaneRead,
    owner: &PaneOwner,
    mut clock: impl FnMut() -> u64,
    publish: &Publish,
    result: Option<&PersistedResult>,
    companions: &[OptionEdit],
) -> Result<CommitOutcome, PersistenceError> {
    publish_program_with_history(
        tmux,
        observed,
        owner,
        &mut clock,
        publish,
        result,
        companions,
        &ResultHistory::Retain,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn publish_program_with_history(
    tmux: &Tmux,
    observed: &PaneRead,
    owner: &PaneOwner,
    mut clock: impl FnMut() -> u64,
    publish: &Publish,
    result: Option<&PersistedResult>,
    companions: &[OptionEdit],
    history: &ResultHistory,
    blocked_record: Option<&tma_core::program_status::RecordId>,
) -> Result<CommitOutcome, PersistenceError> {
    if publish.pane_id != observed.target
        || publish.pid != owner.processes.agent().pid
        || result.is_some_and(|r| {
            &r.owner != owner
                || r.identity.owner != owner.episode
                || PersistedResult::parse(&r.encode()).as_ref() != Some(r)
                || (!matches!(history, ResultHistory::Rebind { .. })
                    && publish.state != tma_core::AgentState::Idle)
                || (!matches!(history, ResultHistory::Rebind { .. })
                    && r.kind == tma_core::program_status::ResultKind::Failure
                    && publish.detail.as_ref().map(tma_core::Detail::as_str)
                        != Some(tma_core::Detail::ERROR))
        })
    {
        return Err(PersistenceError::Invalid("publication owner/pane mismatch"));
    }
    reject_private_edits(companions)?;
    if companions.iter().any(|e| {
        ![
            public::ATTENTION,
            public::TURN_AT,
            public::NOTIFIED_AT,
            public::PERMISSION_REQUEST,
            public::QUESTION_REQUEST,
            public::PENDING_CALL,
            public::PENDING_TOOL,
            public::PENDING_SUMMARY,
        ]
        .contains(&e.key)
    }) {
        return Err(PersistenceError::Invalid(
            "independent or tuple field in program companions",
        ));
    }
    let mut publish = publish.clone();
    publish.source = tma_core::Provenance::Hook;
    publish.guard = Guard::Unconditional;
    let previous = StampedState::from_options(&observed.options)
        .ok()
        .flatten()
        .map(|r| r.into_inner());
    let commands = render::render_publish_advisory(
        &publish,
        previous.as_ref().map(|p| p.state),
        previous.as_ref().map_or(0, |p| p.since),
    );
    let mut edits = vec![
        OptionEdit::unset(opt::COMMIT),
        OptionEdit::set(opt::ORIGIN, "1"),
        OptionEdit::set(opt::OWNER, owner.encode()),
    ];
    for command in commands {
        let edit = edit_from_plain(&command)?;
        if !METADATA_KEYS.contains(&edit.key) {
            edits.push(edit);
        }
    }
    edits.extend_from_slice(companions);
    let tuple_edits = edits.clone();
    add_episode(observed, &tuple_edits, &mut edits)?;
    if publish.state == tma_core::AgentState::Blocked {
        let old = BlockedHistory::parse(observed.value(opt::BLOCKED_HISTORY));
        let same = old.as_ref().filter(|history| {
            history.processes == owner.processes
                && observed.value(public::STATE) == "blocked"
                && observed.value(opt::ATTENTION_EPISODE) == history.episode.as_str()
        });
        let episode = if let Some(history) = same {
            history.episode.clone()
        } else {
            fresh_token()?
        };
        let attention = tuple_edits
            .iter()
            .rev()
            .find(|edit| edit.key == public::ATTENTION)
            .map_or(observed.value(public::ATTENTION) == "1", |edit| {
                edit.value.as_deref() == Some("1")
            });
        let acknowledged = same.is_some_and(|history| history.acknowledged) || !attention;
        let blocked = BlockedHistory {
            processes: owner.processes.clone(),
            episode: episode.clone(),
            acknowledged,
            record: blocked_record
                .cloned()
                .unwrap_or_else(tma_core::program_status::RecordId::root),
        };
        edits.push(OptionEdit::set(opt::ATTENTION_EPISODE, episode.as_str()));
        edits.push(if acknowledged {
            OptionEdit::unset(public::ATTENTION)
        } else {
            OptionEdit::set(public::ATTENTION, "1")
        });
        edits.push(OptionEdit::set(opt::BLOCKED_HISTORY, blocked.encode()));
    } else {
        edits.push(OptionEdit::unset(opt::BLOCKED_HISTORY));
    }
    if matches!(history, ResultHistory::Reset) || publish.episode_reset {
        edits.extend([
            OptionEdit::unset(opt::RESULT),
            OptionEdit::unset(opt::ACK),
            OptionEdit::unset(opt::NOTIFIED),
        ]);
    }
    if let Some(result) = result {
        edits.push(OptionEdit::set(opt::RESULT, result.encode()));
        edits.push(OptionEdit::unset(opt::INVALIDATED));
    }
    if let ResultHistory::Rebind {
        acknowledged,
        notified,
    } = history
    {
        let result = result.ok_or(PersistenceError::Invalid(
            "history rebind needs normalized result",
        ))?;
        edits.push(if *acknowledged {
            OptionEdit::set(opt::ACK, result.encode())
        } else {
            OptionEdit::unset(opt::ACK)
        });
        edits.push(if *notified {
            OptionEdit::set(
                opt::NOTIFIED,
                NotificationClaim {
                    token: fresh_token()?,
                    result: result.clone(),
                }
                .encode(),
            )
        } else {
            OptionEdit::unset(opt::NOTIFIED)
        });
    }
    if result.is_some_and(|result| observed.program.acknowledged.as_ref() == Some(result)) {
        edits.push(OptionEdit::unset(public::ATTENTION));
    }
    let commit = CommitMarker {
        write: fresh_token()?,
        owner: owner.clone(),
    };
    edits.push(OptionEdit::set(opt::COMMIT, commit.encode()));
    let outcome = transaction(
        tmux,
        observed,
        &Ownership::Program {
            owner: owner.clone(),
            now: clock(),
        },
        Guard::Unconditional,
        &[
            opt::ACK,
            opt::RESULT,
            opt::INVALIDATED,
            opt::NOTIFIED,
            opt::BLOCKED_HISTORY,
            opt::ATTENTION_EPISODE,
            opt::EPISODE_NOTIFIED,
            public::ATTENTION,
        ]
        .iter()
        .fold("1".to_owned(), |predicate, key| {
            and(&predicate, &equal(key, observed.value(key)))
        }),
        &edits,
    )?;
    if outcome.won()
        && !matches!(outcome.winner().projection(clock()),
        Ok(ProjectionRead::Live { owner: persisted, .. }) if &persisted == owner)
    {
        return Ok(CommitOutcome::Lost(outcome.winner().clone()));
    }
    Ok(outcome)
}

pub(crate) fn edit_from_plain(command: &StampCommand) -> Result<OptionEdit, PersistenceError> {
    // Renderer emits set-option -p [-u] -t pane key [value]. Recover only registered literal
    // options, so future renderer changes fail explicitly rather than losing readback coverage.
    let args = &command.argv;
    let unset = args.get(2).is_some_and(|s| s == "-u");
    let key_at = if unset { 5 } else { 4 };
    if args.first().map(String::as_str) != Some("set-option")
        || args.get(1).map(String::as_str) != Some("-p")
        || args.get(key_at - 2).map(String::as_str) != Some("-t")
        || args.len() != key_at + if unset { 1 } else { 2 }
    {
        return Err(PersistenceError::Invalid("nonliteral renderer command"));
    }
    let key = args
        .get(key_at)
        .ok_or(PersistenceError::Invalid("unexpected renderer command"))?;
    let key = crate::tmux::pane_option_keys()
        .find(|known| *known == key)
        .ok_or(PersistenceError::Invalid("unregistered renderer option"))?;
    Ok(OptionEdit {
        key,
        value: if unset {
            None
        } else {
            Some(
                args.get(key_at + 1)
                    .ok_or(PersistenceError::Invalid("missing renderer value"))?
                    .clone(),
            )
        },
    })
}

/// Capture at the read used to decide acknowledgement or notification. Verification may read
/// again, but must never replace this identity with the newer result found by that read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionObservation {
    pub read: PaneRead,
    pub identity: ObservationIdentity,
    pub episode_at: u64,
}

impl AttentionObservation {
    pub fn from_read(read: PaneRead, now: u64) -> Option<Self> {
        let (stamp, identity) = read.program.attention_observation(&read.options, now)?;
        Some(Self {
            episode_at: stamp.episode_at(),
            read,
            identity,
        })
    }
}

fn notification_observation_current(
    observation: &AttentionObservation,
    current: &PaneRead,
    now: u64,
) -> bool {
    if observation.read.program.program_origin
        && !matches!(
            observation.read.projection(now),
            Ok(ProjectionRead::Live { .. })
        )
    {
        return false;
    }
    if let ObservationIdentity::Result(result) = &observation.identity {
        if current.program.acknowledged.as_ref() == Some(result)
            || current.program.invalidated.as_ref() == Some(result)
        {
            return false;
        }
    }
    current.value(opt::METADATA_OWNER) == observation.read.value(opt::METADATA_OWNER)
        && current.value(public::ATTENTION) == observation.read.value(public::ATTENTION)
        && AttentionObservation::from_read(current.clone(), now)
            .is_some_and(|current| current.identity == observation.identity)
}

fn observation_predicate(observation: &AttentionObservation) -> Result<String, PersistenceError> {
    let observed = &observation.read;
    Ok(match &observation.identity {
        ObservationIdentity::Result(result) => {
            ownership_predicate(&Ownership::ExactResult(result.clone()), observed)?
        }
        ObservationIdentity::Blocked { owner, episode } => and(
            &equal(opt::OWNER, &owner.encode()),
            &and(
                &equal(public::STATE, "blocked"),
                &equal(opt::ATTENTION_EPISODE, episode.as_str()),
            ),
        ),
        ObservationIdentity::Legacy { .. } => {
            let mut predicate = and(
                &equal(opt::ORIGIN, ""),
                &and(&equal(opt::OWNER, ""), &equal(opt::COMMIT, "")),
            );
            for key in [
                public::STATE,
                public::SOURCE,
                public::PID,
                public::SINCE,
                public::TURN_AT,
                opt::ATTENTION_EPISODE,
            ] {
                predicate = and(&predicate, &equal(key, observed.value(key)));
            }
            predicate
        }
    })
}

/// Guard legacy/blocked clears by the observed stable episode. Terminal ACKs retain their exact
/// result marker. A missing attention bit never consumes a deferred terminal result.
pub fn acknowledge_observation(
    tmux: &Tmux,
    observation: &AttentionObservation,
) -> Result<CommitOutcome, PersistenceError> {
    if let ObservationIdentity::Result(result) = &observation.identity {
        return match acknowledge_result(tmux, &observation.read, result) {
            Err(PersistenceError::Unsupported) => {
                advisory_acknowledge(tmux, &observation.read, result).map(CommitOutcome::Lost)
            }
            outcome => outcome,
        };
    }
    let base_predicate = and(
        &observation_predicate(observation)?,
        &equal(public::ATTENTION, "1"),
    );
    let predicate = and(
        &base_predicate,
        &equal(
            opt::BLOCKED_HISTORY,
            observation.read.value(opt::BLOCKED_HISTORY),
        ),
    );
    let mut edits = vec![OptionEdit::unset(public::ATTENTION)];
    if let Some(mut history) = BlockedHistory::parse(observation.read.value(opt::BLOCKED_HISTORY))
        .filter(|history| {
            observation.read.value(opt::ATTENTION_EPISODE) == history.episode.as_str()
        })
    {
        history.acknowledged = true;
        edits.insert(0, OptionEdit::set(opt::BLOCKED_HISTORY, history.encode()));
    }
    match transaction(
        tmux,
        &observation.read,
        &Ownership::Metadata {
            expected: metadata_owner(&observation.read),
        },
        Guard::Unconditional,
        &predicate,
        &edits,
    ) {
        Err(PersistenceError::Unsupported) => {
            for (index, edit) in edits.iter().enumerate() {
                let expected_history = if index == 0 {
                    observation.read.value(opt::BLOCKED_HISTORY)
                } else {
                    edits[0].value.as_deref().unwrap_or("")
                };
                let predicate = and(
                    &base_predicate,
                    &equal(opt::BLOCKED_HISTORY, expected_history),
                );
                tmux.apply_persistence(
                    &observation.read.target,
                    &[render::conditional_group(
                        &observation.read.target,
                        &predicate,
                        &[edit.command(&observation.read.target)],
                    )],
                )?;
            }
            Ok(CommitOutcome::Lost(read_pane(
                tmux,
                &observation.read.target,
            )?))
        }
        outcome => outcome,
    }
}

pub fn metadata_owner(read: &PaneRead) -> Option<MetadataOwner> {
    read.options
        .get(opt::METADATA_OWNER)
        .and_then(|value| MetadataOwner::parse(value))
}

/// Notification permission is distinct from a complete pane transaction. `grouped` indicates
/// complete transaction confirmation, which a later receipt refresh can defeat while an exact
/// unique claim still grants permission. Program claim writes require supported guarded groups;
/// legacy compatibility also permits one exact mutation under yielding hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationOutcome {
    Claimed { read: PaneRead, grouped: bool },
    Lost(PaneRead),
}
impl NotificationOutcome {
    pub fn claimed(&self) -> bool {
        matches!(self, Self::Claimed { .. })
    }
    pub fn winner(&self) -> &PaneRead {
        match self {
            Self::Claimed { read, .. } | Self::Lost(read) => read,
        }
    }
    fn grouped(outcome: CommitOutcome) -> Self {
        match outcome {
            CommitOutcome::Won(read) => Self::Claimed {
                read,
                grouped: true,
            },
            CommitOutcome::Lost(read) => Self::Lost(read),
        }
    }
}

/// Claim an exact legacy or blocked episode. Stable dedup identity excludes receipt and commit.
/// A surviving unique claim may prove notification permission after a same-episode refresh,
/// without promoting that refresh's losing full transaction to a grouped win.
pub fn claim_observation(
    tmux: &Tmux,
    observation: &AttentionObservation,
    mut clock: impl FnMut() -> u64,
) -> Result<NotificationOutcome, PersistenceError> {
    if let ObservationIdentity::Result(result) = &observation.identity {
        let (outcome, claim) =
            claim_notification_with_token(tmux, &observation.read, result, &mut clock)?;
        let current = outcome.winner();
        if !outcome.won()
            && current.program.notified.as_ref() == Some(&claim)
            && notification_observation_current(observation, current, clock())
        {
            return Ok(NotificationOutcome::Claimed {
                read: current.clone(),
                grouped: false,
            });
        }
        return Ok(NotificationOutcome::grouped(outcome));
    }
    let now = clock();
    let identity = observation.identity.encode();
    let already = format!("#{{m:*|{},{}}}", identity, option(opt::EPISODE_NOTIFIED));
    let mut predicate = and(&observation_predicate(observation)?, &not(&already));
    if matches!(observation.identity, ObservationIdentity::Legacy { .. }) {
        predicate = and(
            &predicate,
            &and(
                &equal(
                    opt::METADATA_OWNER,
                    observation.read.value(opt::METADATA_OWNER),
                ),
                &equal(opt::RECEIPT, observation.read.value(opt::RECEIPT)),
            ),
        );
    }
    if matches!(observation.identity, ObservationIdentity::Blocked { .. }) {
        predicate = and(&predicate, &live_projection(&observation.read, now));
    } else {
        // Preserve cold-start behavior for tuples published before this private claim existed.
        let stamp = StampedState::from_options(&observation.read.options)
            .ok()
            .flatten()
            .map(|s| s.into_inner());
        if observation.read.value(opt::ATTENTION_EPISODE).is_empty()
            && stamp.is_some_and(|s| s.notified_at.is_some_and(|n| n >= s.episode_at()))
        {
            return Ok(NotificationOutcome::Lost(read_pane(
                tmux,
                &observation.read.target,
            )?));
        }
    }
    let claim = format!("{}|{identity}", fresh_token()?.as_str());
    let outcome = transaction(
        tmux,
        &observation.read,
        &Ownership::Metadata {
            expected: metadata_owner(&observation.read),
        },
        Guard::Unconditional,
        &predicate,
        &[
            OptionEdit::set(opt::EPISODE_NOTIFIED, &claim),
            OptionEdit::set(
                public::NOTIFIED_AT,
                now.max(observation.episode_at).to_string(),
            ),
        ],
    );
    let outcome = match outcome {
        Err(PersistenceError::Unsupported)
            if matches!(observation.identity, ObservationIdentity::Legacy { .. }) =>
        {
            // One guarded mutation is the claim. Public numeric compatibility is a separate,
            // exact-claim conditional update; neither is represented as grouped success.
            tmux.apply_persistence(
                &observation.read.target,
                &[render::conditional_group(
                    &observation.read.target,
                    &predicate,
                    &[OptionEdit::set(opt::EPISODE_NOTIFIED, &claim)
                        .command(&observation.read.target)],
                )],
            )?;
            let claimed = and(
                &observation_predicate(observation)?,
                &equal(opt::EPISODE_NOTIFIED, &claim),
            );
            tmux.apply_persistence(
                &observation.read.target,
                &[render::conditional_group(
                    &observation.read.target,
                    &claimed,
                    &[OptionEdit::set(
                        public::NOTIFIED_AT,
                        now.max(observation.episode_at).to_string(),
                    )
                    .command(&observation.read.target)],
                )],
            )?;
            let current = read_pane(tmux, &observation.read.target)?;
            let same = AttentionObservation::from_read(current.clone(), clock())
                .is_some_and(|current| current.identity == observation.identity);
            return Ok(if same && current.value(opt::EPISODE_NOTIFIED) == claim {
                NotificationOutcome::Claimed {
                    read: current,
                    grouped: false,
                }
            } else {
                NotificationOutcome::Lost(current)
            });
        }
        outcome => outcome?,
    };
    if outcome.won()
        && matches!(observation.identity, ObservationIdentity::Blocked { .. })
        && !matches!(
            outcome.winner().projection(clock()),
            Ok(ProjectionRead::Live { .. })
        )
    {
        return Ok(NotificationOutcome::Lost(outcome.winner().clone()));
    }
    let current = outcome.winner();
    if !outcome.won()
        && current.value(opt::EPISODE_NOTIFIED) == claim
        && notification_observation_current(observation, current, clock())
    {
        return Ok(NotificationOutcome::Claimed {
            read: current.clone(),
            grouped: false,
        });
    }
    Ok(NotificationOutcome::grouped(outcome))
}

/// Context and stall markers use a unique claim receipt, never equality of epoch timestamps.
/// State and metric inputs come from the caller's decision read and remain unchanged at commit.
pub fn update_notify_marker(
    tmux: &Tmux,
    observed: &PaneRead,
    key: &'static str,
    arm_at: Option<u64>,
) -> Result<CommitOutcome, PersistenceError> {
    if ![public::CONTEXT_NOTIFIED_AT, public::STALL_NOTIFIED_AT].contains(&key) {
        return Err(PersistenceError::Invalid("invalid notification marker"));
    }
    let mut checks: Vec<_> = [
        key,
        public::CONTEXT_PCT,
        public::CONTEXT_AT,
        opt::METADATA_OWNER,
    ]
    .iter()
    .map(|field| equal(field, observed.value(field)))
    .collect();
    if arm_at.is_some() {
        checks.push(equal(key, ""));
    }
    let predicate = and(&conjunction(&checks), &projection_unchanged(observed));
    transaction(
        tmux,
        observed,
        &Ownership::Metadata {
            expected: metadata_owner(observed),
        },
        Guard::Unconditional,
        &predicate,
        &[
            match arm_at {
                Some(at) => OptionEdit::set(key, at.to_string()),
                None => OptionEdit::unset(key),
            },
            OptionEdit::set(opt::MARKER_CLAIM, fresh_token()?.as_str()),
        ],
    )
}

/// Compatibility marker claim under yielding hooks. The private single mutation is the claim;
/// its guarded numeric companion is public compatibility, never grouped success. Program stall
/// effects remain unavailable without grouped authority; context belongs to independent metadata.
pub fn advisory_notify_marker(
    tmux: &Tmux,
    observed: &PaneRead,
    key: &'static str,
    arm_at: Option<u64>,
) -> Result<NotificationOutcome, PersistenceError> {
    if ![public::CONTEXT_NOTIFIED_AT, public::STALL_NOTIFIED_AT].contains(&key) {
        return Err(PersistenceError::Invalid("invalid notification marker"));
    }
    if key == public::STALL_NOTIFIED_AT && observed.program.program_origin {
        return Err(PersistenceError::Unsupported);
    }
    let claim = fresh_token()?;
    let mut checks: Vec<_> = [
        key,
        opt::METADATA_OWNER,
        opt::METADATA_UPDATE,
        opt::RECEIPT,
        public::CONTEXT_PCT,
        public::CONTEXT_AT,
    ]
    .iter()
    .map(|field| equal(field, observed.value(field)))
    .collect();
    if arm_at.is_some() {
        checks.push(equal(key, ""));
    }
    let projection = projection_unchanged(observed);
    let guarded_by = |claim: &str| {
        let mut claimed = checks.clone();
        claimed.push(equal(opt::MARKER_CLAIM, claim));
        and(&conjunction(&claimed), &projection)
    };
    let admission = guarded_by(observed.value(opt::MARKER_CLAIM));
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &admission,
            &[OptionEdit::set(opt::MARKER_CLAIM, claim.as_str()).command(&observed.target)],
        )],
    )?;
    let predicate = guarded_by(claim.as_str());
    let edit = match arm_at {
        Some(at) => OptionEdit::set(key, at.to_string()),
        None => OptionEdit::unset(key),
    };
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &predicate,
            &[edit.command(&observed.target)],
        )],
    )?;
    let current = read_pane(tmux, &observed.target)?;
    let same = PROJECTION_KEYS
        .iter()
        .chain(
            [
                opt::METADATA_OWNER,
                opt::METADATA_UPDATE,
                opt::RECEIPT,
                public::CONTEXT_PCT,
                public::CONTEXT_AT,
            ]
            .iter(),
        )
        .all(|field| current.value(field) == observed.value(field));
    Ok(
        if same
            && current.value(opt::MARKER_CLAIM) == claim.as_str()
            && current.value(key) == edit.value.as_deref().unwrap_or("")
        {
            NotificationOutcome::Claimed {
                read: current,
                grouped: false,
            }
        } else {
            NotificationOutcome::Lost(current)
        },
    )
}

fn add_episode(
    observed: &PaneRead,
    edits: &[OptionEdit],
    body: &mut Vec<OptionEdit>,
) -> Result<(), PersistenceError> {
    let final_value = |key| {
        edits.iter().rev().find(|edit| edit.key == key).map_or_else(
            || observed.value(key),
            |edit| edit.value.as_deref().unwrap_or(""),
        )
    };
    // The closed episode-reset renderer clears both clocks while publishing a present state.
    let reset = edits
        .iter()
        .any(|edit| edit.key == public::STATE && edit.value.is_some())
        && [public::TURN_AT, public::NOTIFIED_AT].iter().all(|key| {
            edits
                .iter()
                .any(|edit| edit.key == *key && edit.value.is_none())
        });
    if reset
        || edits
            .iter()
            .any(|edit| edit.key == public::STATE && edit.value.as_deref() == Some("working"))
    {
        for key in [opt::RESULT, opt::ACK, opt::NOTIFIED, opt::INVALIDATED] {
            body.push(OptionEdit::unset(key));
        }
    }
    let removed = edits
        .iter()
        .any(|edit| edit.key == public::STATE && edit.value.is_none());
    if removed {
        body.push(OptionEdit::unset(opt::ATTENTION_EPISODE));
        body.push(OptionEdit::unset(opt::EPISODE_NOTIFIED));
        body.push(OptionEdit::unset(opt::BLOCKED_HISTORY));
        for key in [
            opt::RESULT,
            opt::ACK,
            opt::NOTIFIED,
            opt::INVALIDATED,
            opt::MARKER_CLAIM,
        ] {
            body.push(OptionEdit::unset(key));
        }
        return Ok(());
    }
    let state_changed = reset
        || final_value(public::STATE) != observed.value(public::STATE)
        || final_value(public::PID) != observed.value(public::PID);
    if reset || state_changed && final_value(public::STATE) != "blocked" {
        body.push(OptionEdit::unset(opt::BLOCKED_HISTORY));
    }
    if !state_changed
        && final_value(public::STATE) == "blocked"
        && observed.value(public::ATTENTION) != "1"
    {
        body.push(OptionEdit::unset(public::ATTENTION));
    }
    let explicit_turn = edits
        .iter()
        .any(|edit| edit.key == public::TURN_AT && edit.value.is_some())
        && observed.value(public::ATTENTION) != "1";
    if !final_value(public::STATE).is_empty()
        && (state_changed
            || explicit_turn
            || Token::parse(observed.value(opt::ATTENTION_EPISODE)).is_none())
    {
        let episode = fresh_token()?;
        body.push(OptionEdit::set(opt::ATTENTION_EPISODE, episode.as_str()));
        // Initial migration of an unchanged legacy run preserves an already-spent numeric
        // marker. Provenance changes cannot mint a semantic episode or rearm that claim.
        if !state_changed && !explicit_turn && !observed.program.program_origin {
            let stamp = StampedState::from_options(&observed.options)
                .ok()
                .flatten()
                .map(|stamp| stamp.into_inner());
            if stamp.is_some_and(|stamp| {
                stamp
                    .notified_at
                    .is_some_and(|notified| notified >= stamp.episode_at())
            }) {
                body.push(OptionEdit::set(
                    opt::EPISODE_NOTIFIED,
                    format!("{}|legacy|{}", fresh_token()?.as_str(), episode.as_str()),
                ));
            }
        }
    }
    Ok(())
}

/// Retire the metadata update claim after complete removal and independent metadata release.
/// Keep the private pane receipt as a revision tombstone: deleting it would make an old empty
/// snapshot match again after another producer published and removed the pane (ABA).
pub(crate) fn finish_removal(
    tmux: &Tmux,
    observed: &PaneRead,
) -> Result<PaneRead, PersistenceError> {
    let predicate = [
        public::STATE,
        opt::ORIGIN,
        opt::OWNER,
        opt::COMMIT,
        opt::METADATA_OWNER,
    ]
    .iter()
    .fold(
        equal(opt::RECEIPT, observed.value(opt::RECEIPT)),
        |predicate, key| and(&predicate, &equal(key, "")),
    );
    let predicate = and(
        &predicate,
        &equal(opt::METADATA_UPDATE, observed.value(opt::METADATA_UPDATE)),
    );
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &predicate,
            &[OptionEdit::unset(opt::METADATA_UPDATE).command(&observed.target)],
        )],
    )?;
    read_pane(tmux, &observed.target)
}

/// Immediate single-mutation revocation remains safe when hooks were added after the probe.
/// History and public projection remain tagged program-origin and therefore decode stale.
pub fn revoke_program(
    tmux: &Tmux,
    pane: &str,
    owner: &PaneOwner,
) -> Result<bool, PersistenceError> {
    let current = revoke_program_readback(tmux, pane, owner)?;
    Ok(current.owner().as_ref() == Some(owner) && current.value(opt::COMMIT).is_empty())
}

/// Return the exact revocation's existing readback, including a replacement owner.
/// Callers can prove old authority absent without another independent read.
pub fn revoke_program_readback(
    tmux: &Tmux,
    pane: &str,
    owner: &PaneOwner,
) -> Result<PaneRead, PersistenceError> {
    let body = [render::unset_pane_option(pane, opt::COMMIT)];
    tmux.apply_persistence(
        pane,
        &[render::conditional_group(
            pane,
            &equal(opt::OWNER, &owner.encode()),
            &body,
        )],
    )?;
    read_pane(tmux, pane)
}

/// Record-clear/RIS invalidation is one exact-owner mutation, safe even with yielding hooks.
/// It fences historical reconciliation without granting authority or inventing another result.
pub fn invalidate_history(
    tmux: &Tmux,
    observed: &PaneRead,
    owner: &PaneOwner,
    invalidation: &tma_core::persistence::HistoryInvalidation,
) -> Result<(), PersistenceError> {
    if let Some(history) = BlockedHistory::parse(observed.value(opt::BLOCKED_HISTORY)) {
        let affected = match invalidation {
            tma_core::persistence::HistoryInvalidation::All => true,
            tma_core::persistence::HistoryInvalidation::Subtree(id) => {
                id.as_ref().contains(history.record.as_ref())
            }
        };
        if affected {
            let mut acknowledged = history.clone();
            acknowledged.acknowledged = !history.acknowledged;
            let predicate = and(
                &equal(opt::OWNER, &owner.encode()),
                &format!(
                    "#{{||:{},{}}}",
                    equal(opt::BLOCKED_HISTORY, &history.encode()),
                    equal(opt::BLOCKED_HISTORY, &acknowledged.encode())
                ),
            );
            tmux.apply_persistence(
                &observed.target,
                &[render::conditional_group(
                    &observed.target,
                    &predicate,
                    &[OptionEdit::unset(opt::BLOCKED_HISTORY).command(&observed.target)],
                )],
            )?;
        }
    }
    let Some(result) = observed
        .result()
        .filter(|result| invalidation.affects(result))
    else {
        return Ok(());
    };
    let predicate = and(
        &equal(opt::OWNER, &owner.encode()),
        &equal(opt::RESULT, &result.encode()),
    );
    tmux.apply_persistence(
        &observed.target,
        &[render::conditional_group(
            &observed.target,
            &predicate,
            &[OptionEdit::set(opt::INVALIDATED, result.encode()).command(&observed.target)],
        )],
    )
    .map_err(PersistenceError::from)
}

/// Exact cleanup keeps its initial ownership decision even after clearing OWNER within the body.
/// `edits` can include remove/EndSession/hold edits; old-token work cannot affect a replacement.
pub fn release_program(
    tmux: &Tmux,
    observed: &PaneRead,
    owner: &PaneOwner,
    edits: &[OptionEdit],
) -> Result<CommitOutcome, PersistenceError> {
    reject_private_edits(edits)?;
    let mut body = vec![
        OptionEdit::unset(opt::COMMIT),
        OptionEdit::unset(opt::OWNER),
    ];
    body.extend_from_slice(edits);
    transaction(
        tmux,
        observed,
        &Ownership::ExactOwner(owner.clone()),
        Guard::Unconditional,
        "1",
        &body,
    )
}

pub fn acknowledge_result(
    tmux: &Tmux,
    observed: &PaneRead,
    result: &PersistedResult,
) -> Result<CommitOutcome, PersistenceError> {
    transaction(
        tmux,
        observed,
        &Ownership::ExactResult(result.clone()),
        Guard::Unconditional,
        &and(
            &not(&equal(opt::INVALIDATED, &result.encode())),
            &and(
                &equal(public::STATE, "idle"),
                &equal(public::ATTENTION, "1"),
            ),
        ),
        &[
            OptionEdit::set(opt::ACK, result.encode()),
            OptionEdit::unset(public::ATTENTION),
        ],
    )
}

/// Claim BEFORE invoking any external notification effect. A fresh random token distinguishes
/// concurrent claims at equal clocks. Failure after a successful claim intentionally does not retry
/// the external effect automatically (at-most-once claim, not exactly-once delivery).
/// Revoked history and an exact durable ACK cannot authorize a claim, even with attention present.
pub fn claim_notification(
    tmux: &Tmux,
    observed: &PaneRead,
    result: &PersistedResult,
    clock: impl FnMut() -> u64,
) -> Result<CommitOutcome, PersistenceError> {
    claim_notification_with_token(tmux, observed, result, clock).map(|(outcome, _)| outcome)
}

fn claim_notification_with_token(
    tmux: &Tmux,
    observed: &PaneRead,
    result: &PersistedResult,
    mut clock: impl FnMut() -> u64,
) -> Result<(CommitOutcome, NotificationClaim), PersistenceError> {
    let already = format!("#{{m:*|{},{}}}", result.encode(), option(opt::NOTIFIED));
    let claim = NotificationClaim {
        token: fresh_token()?,
        result: result.clone(),
    };
    let now = clock();
    let outcome = transaction(
        tmux,
        observed,
        &Ownership::ExactResult(result.clone()),
        Guard::Unconditional,
        &and(
            &live_projection(observed, now),
            &and(
                &and(
                    &not(&equal(opt::INVALIDATED, &result.encode())),
                    &and(
                        &equal(public::STATE, "idle"),
                        &not(&equal(opt::ACK, &result.encode())),
                    ),
                ),
                &and(&equal(public::ATTENTION, "1"), &not(&already)),
            ),
        ),
        &[
            OptionEdit::set(opt::NOTIFIED, claim.encode()),
            OptionEdit::set(public::NOTIFIED_AT, now.to_string()),
        ],
    )?;
    if outcome.won()
        && !matches!(outcome.winner().projection(clock()),
        Ok(ProjectionRead::Live { owner, .. }) if owner == result.owner)
    {
        return Ok((CommitOutcome::Lost(outcome.winner().clone()), claim));
    }
    Ok((outcome, claim))
}

/// Explicit metadata binding/updates operate independently of a losing hook state transaction.
/// Runtime must validate the new process/session before binding or replacing the marker.
pub fn write_metadata(
    tmux: &Tmux,
    observed: &PaneRead,
    expected: Option<&MetadataOwner>,
    next: Option<&MetadataOwner>,
    edits: &[OptionEdit],
) -> Result<CommitOutcome, PersistenceError> {
    write_metadata_observation(tmux, observed, expected, next, edits, None)
}

pub fn write_metadata_observation(
    tmux: &Tmux,
    observed: &PaneRead,
    expected: Option<&MetadataOwner>,
    next: Option<&MetadataOwner>,
    edits: &[OptionEdit],
    evidence: Option<MetadataEvidence>,
) -> Result<CommitOutcome, PersistenceError> {
    validate_metadata(expected, next, edits)?;
    let mut body = edits.to_vec();
    body.push(match next {
        Some(owner) => OptionEdit::set(opt::METADATA_OWNER, owner.encode()),
        None => OptionEdit::unset(opt::METADATA_OWNER),
    });
    body.push(OptionEdit::set(
        opt::METADATA_UPDATE,
        MetadataUpdate {
            token: fresh_token()?,
            evidence,
        }
        .encode(),
    ));
    transaction(
        tmux,
        observed,
        &Ownership::Metadata {
            expected: expected.cloned(),
        },
        Guard::Unconditional,
        &metadata_admission(observed, evidence),
        &body,
    )
}

fn validate_metadata(
    expected: Option<&MetadataOwner>,
    next: Option<&MetadataOwner>,
    edits: &[OptionEdit],
) -> Result<(), PersistenceError> {
    if edits.iter().any(|e| !METADATA_KEYS.contains(&e.key)) {
        return Err(PersistenceError::Invalid(
            "state field in independent metadata lane",
        ));
    }
    if expected
        .into_iter()
        .chain(next)
        .any(|owner| MetadataOwner::parse(&owner.encode()).as_ref() != Some(owner))
    {
        return Err(PersistenceError::Invalid("invalid metadata owner"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    Won(Option<CollectorLease>),
    Lost(Option<CollectorLease>),
}

fn lease_operation(
    tmux: &Tmux,
    pane: &str,
    predicate: &str,
    value: Option<&CollectorLease>,
    no_hooks: bool,
) -> Result<LeaseOutcome, PersistenceError> {
    let predicate = if no_hooks {
        and(NO_HOOKS, predicate)
    } else {
        predicate.to_owned()
    };
    let command = match value {
        Some(lease) => StampCommand {
            argv: vec![
                "set-option".into(),
                "-s".into(),
                "-t".into(),
                pane.into(),
                opt::COLLECTOR.into(),
                lease.encode(),
            ],
        },
        None => StampCommand {
            argv: vec![
                "set-option".into(),
                "-su".into(),
                "-t".into(),
                pane.into(),
                opt::COLLECTOR.into(),
            ],
        },
    };
    tmux.apply_persistence(
        pane,
        &[render::conditional_group(pane, &predicate, &[command])],
    )?;
    let stored = read_collector(tmux, pane)?;
    if no_hooks && tmux.pane_format(pane, NO_HOOKS)? != "1" {
        return Err(PersistenceError::Unsupported);
    }
    let parsed = stored.as_deref().and_then(CollectorLease::parse);
    let expected = value.map(CollectorLease::encode);
    Ok(if stored == expected {
        LeaseOutcome::Won(parsed)
    } else {
        LeaseOutcome::Lost(parsed)
    })
}

fn read_collector(tmux: &Tmux, pane: &str) -> Result<Option<String>, PersistenceError> {
    // The display sentinel preserves raw whitespace: the legacy option getter trims it,
    // which would turn a malformed lease into a different, apparently canonical value.
    let raw = tmux.pane_format(pane, &format!("{}\u{1f}end", option(opt::COLLECTOR)))?;
    let value = raw
        .strip_suffix("\u{1f}end")
        .ok_or(PersistenceError::Invalid("invalid lease readback"))?;
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

pub fn acquire_collector(
    tmux: &Tmux,
    pane: &str,
    token: &Token,
    mut clock: impl FnMut() -> u64,
) -> Result<LeaseOutcome, PersistenceError> {
    let current = read_collector(tmux, pane)?;
    let now = clock();
    let eligible = !current
        .as_deref()
        .and_then(CollectorLease::parse)
        .is_some_and(|lease| lease.live_at(now));
    let lease = CollectorLease {
        token: token.clone(),
        expires_at: now.saturating_add(LEASE_DURATION_MS),
    };
    let outcome = lease_operation(
        tmux,
        pane,
        &and(
            if eligible { "1" } else { "0" },
            &equal(opt::COLLECTOR, current.as_deref().unwrap_or("")),
        ),
        Some(&lease),
        true,
    )?;
    Ok(confirmed_live_lease(outcome, clock()))
}

/// The scheduler must revoke every old pane and use a fresh token after a missed deadline or
/// wall-clock discontinuity. This operation also rejects an already expired persisted lease.
/// Completion must precede the previous deadline as well as the new one. A Lost outcome can
/// therefore include a newly written future lease; never use it to bridge expired coverage.
pub fn renew_collector(
    tmux: &Tmux,
    pane: &str,
    previous: &CollectorLease,
    mut clock: impl FnMut() -> u64,
) -> Result<LeaseOutcome, PersistenceError> {
    let current = read_collector(tmux, pane)?;
    let now = clock();
    let lease = CollectorLease {
        token: previous.token.clone(),
        expires_at: now.saturating_add(LEASE_DURATION_MS),
    };
    let outcome = lease_operation(
        tmux,
        pane,
        &and(
            if previous.live_at(now) { "1" } else { "0" },
            &and(
                &equal(opt::COLLECTOR, &previous.encode()),
                &equal(opt::COLLECTOR, current.as_deref().unwrap_or("")),
            ),
        ),
        Some(&lease),
        true,
    )?;
    let completed_at = clock();
    Ok(if previous.live_at(completed_at) {
        confirmed_live_lease(outcome, completed_at)
    } else {
        match outcome {
            LeaseOutcome::Won(lease) | LeaseOutcome::Lost(lease) => LeaseOutcome::Lost(lease),
        }
    })
}

fn confirmed_live_lease(outcome: LeaseOutcome, now: u64) -> LeaseOutcome {
    match outcome {
        LeaseOutcome::Won(lease) if !lease.as_ref().is_some_and(|lease| lease.live_at(now)) => {
            LeaseOutcome::Lost(lease)
        }
        other => other,
    }
}

/// A single exact-token conditional mutation, safe even with yielding option hooks.
pub fn release_collector(
    tmux: &Tmux,
    pane: &str,
    token: &Token,
) -> Result<LeaseOutcome, PersistenceError> {
    // Only the token matters for revocation: a concurrent renewal of the same collector
    // must not defeat shutdown. The anchored machine grammar cannot match a newer token.
    let predicate = format!(
        "#{{m/r:^[0-9]+:{}$,{}}}",
        token.as_str(),
        option(opt::COLLECTOR),
    );
    let released = CollectorLease {
        token: token.clone(),
        expires_at: 0,
    };
    lease_operation(tmux, pane, &predicate, Some(&released), false)
}

/// Probe actual behavior on this server, using fresh private options and always cleaning them.
/// No cache is authoritative: every transaction independently rejects option hooks at write time.
pub fn probe_capability(tmux: &Tmux, pane: &str) -> Result<bool, PersistenceError> {
    if tmux.pane_format(pane, NO_HOOKS)? != "1" {
        return Ok(false);
    }
    let token = fresh_token()?;
    let key = format!("@__tma_group_probe_{}", token.as_str());
    let key2 = format!("@__tma_group_probe_tail_{}", token.as_str());
    let quote = format!(
        "{} ' \" \\ $HOME `false` ; #{{pane_id}} ,\n  # literal comment\n\\\n雪",
        token.as_str()
    );
    let result = (|| {
        let commands = [render::conditional_group(
            pane,
            NO_HOOKS,
            &[
                render::set_pane_option(pane, &key, "before"),
                render::conditional_group(
                    pane,
                    &equal(&key, "before"),
                    &[
                        render::unset_pane_option(pane, &key),
                        render::set_pane_option(pane, &key2, &quote),
                    ],
                ),
                render::conditional_group(
                    pane,
                    "0",
                    &[render::set_pane_option(pane, &key2, "wrong")],
                ),
            ],
        )];
        for streamed in [false, true] {
            if streamed {
                tmux.apply_persistence_script(pane, &commands)?;
            } else {
                tmux.apply_persistence(pane, &commands)?;
            }
            let actual = tmux.pane_format(pane, &format!("#{{{key}}}\u{1f}#{{{key2}}}"))?;
            if actual != format!("\u{1f}{quote}") {
                return Ok(false);
            }
        }
        // A runtime error must stop the suffix. A success marker after partial execution is
        // unacceptable regardless of tmux's exit status.
        let failure = StampCommand {
            argv: vec![
                "set-option".into(),
                "-p".into(),
                "-t".into(),
                pane.into(),
                "__tma_invalid_builtin_option".into(),
                "1".into(),
            ],
        };
        let _outcome = tmux.apply_persistence_script(
            pane,
            &[render::conditional_group(
                pane,
                NO_HOOKS,
                &[
                    render::set_pane_option(pane, &key, "partial"),
                    failure,
                    render::set_pane_option(pane, &key2, "bad-commit"),
                ],
            )],
        );
        let actual = tmux.pane_format(pane, &format!("#{{{key}}}\u{1f}#{{{key2}}}"))?;
        Ok(actual == format!("partial\u{1f}{quote}"))
    })();
    let _ = tmux.apply(&[
        render::unset_pane_option(pane, &key),
        render::unset_pane_option(pane, &key2),
    ]);
    result
}

pub fn require_capability(tmux: &Tmux, pane: &str) -> Result<(), PersistenceError> {
    match probe_capability(tmux, pane) {
        Ok(true) => Ok(()),
        _ => {
            if !UNSUPPORTED_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("tma: program authority disabled: safe conditional option groups unavailable (or after-set-option hooks configured); retaining fallback");
            }
            Err(PersistenceError::Unsupported)
        }
    }
}
