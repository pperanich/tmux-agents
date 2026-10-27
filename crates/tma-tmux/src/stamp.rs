//! Guarded stamping: the write half of the poll cycle. Renders a producer's verdict into a
//! owned pane transaction, verifies the stored winner, and reconciles summaries.
//!
//! The *rendering* is pure and lives in `tma-core` ([`tma_core::render`]); this module owns the
//! seam that touches tmux: mapping a [`Verdict`] to a guard, the capability probe, appending the
//! `@agent_summary` rollups (window and session), and the agent-exit removal.

use std::sync::atomic::{AtomicBool, Ordering};

use tma_core::render::{self, Guard, Publish, StampCommand, SummaryScope};
use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, Verdict, WriteAction};

use crate::persistence::{self, OptionEdit, PaneRead, PersistenceError};
use crate::tmux::{PaneRecord, Tmux, TmuxError};

fn clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub fn persistence_error(error: PersistenceError) -> TmuxError {
    match error {
        PersistenceError::Tmux(error) => error,
        error => TmuxError::Parse {
            cmd: "pane persistence".into(),
            reason: error.to_string(),
        },
    }
}

/// Actual stored winner, including partial advisory progress when grouped writes are unavailable.
#[derive(Clone, Debug)]
pub struct Applied {
    pub read: PaneRead,
    pub committed: bool,
}

/// Fires the one-time degrade warning at most once per process (so the picker's 1 Hz refresh
/// loop does not spam it every cycle).
static DEGRADE_WARNED: AtomicBool = AtomicBool::new(false);
static SUMMARY_BOUND_WARNED: AtomicBool = AtomicBool::new(false);

/// What to write for one pane this cycle.
pub enum StampPlan {
    /// Commit a new (guarded) state tuple.
    Publish(Publish),
    /// Writes-on-hold: refresh freshness + hash only.
    Hold { stamped_at: u64, hash: Option<u64> },
    /// The agent exited: remove its state and independently owned metadata, retaining a private revision tombstone.
    Remove,
    /// One session ended under a still-running agent: clear the session lane, keep the state tuple
    /// (`render::render_end_session`).
    EndSession,
}

/// Choose the write guard for a verdict's `Publish`. The guard re-checks `@agent_source` (and, for
/// the carve-out, `@agent_evidence_at`) server-side at write time, so a hook event landing inside
/// the read→write window still wins (TOCTOU-safe).
pub(crate) fn guard_for_verdict(v: &Verdict) -> Guard {
    if v.writes.episode_reset {
        // This bypasses legacy arbitration; shared program ownership still guards the tuple.
        return Guard::Unconditional;
    }
    if v.writes.may_override {
        return Guard::Unconditional;
    }
    match v.winning_evidence.source {
        // Corroboration of an existing hook claim: advance its evidence_at only while the
        // store still shows that state (a concurrent hook must not be clobbered).
        Provenance::Hook => Guard::RefreshClaim { state: v.state },
        // Blocker chrome overrides a working/idle hook claim iff its capture postdates the stamped
        // evidence (the carve-out); a working/idle capture write never clobbers a hook claim.
        _ if v.state == AgentState::Blocked => Guard::CarveOut {
            capture_at: v.winning_evidence.at,
        },
        _ => Guard::ProtectHook,
    }
}

/// Build the [`StampPlan`] for a verdict: `Publish` carries the guard, freshness, and identity;
/// `Hold` refreshes freshness + hash only. `now` is the cycle clock, `hash` the viewport hash.
pub fn plan_from_verdict(
    pane_id: &str,
    v: &Verdict,
    pid: u32,
    name: &str,
    hash: u64,
    now: u64,
) -> StampPlan {
    match v.writes.action {
        WriteAction::Hold => StampPlan::Hold {
            stamped_at: now,
            hash: Some(hash),
        },
        WriteAction::Publish => StampPlan::Publish(Publish {
            pane_id: pane_id.to_string(),
            state: v.state,
            detail: v.detail.clone(),
            source: v.winning_evidence.source,
            evidence_at: v.winning_evidence.at,
            since: v.winning_evidence.at,
            stamped_at: now,
            hash: Some(hash),
            pid,
            name: name.to_string(),
            set_attention: v.writes.set_attention,
            episode_reset: v.writes.episode_reset,
            guard: guard_for_verdict(v),
        }),
    }
}

/// Whether this server supports `set-option -F` conditional writes (the guards). Cached in
/// [`opt::SETPF_OK`] for the server's life; probes once against any pane. On `false`, producers fall
/// back to the legacy expansion posture, warned once. Ownership checks remain mandatory. If the probe
/// cannot run, default to guarded WITHOUT caching so a later cycle re-probes.
pub fn guarded_writes_supported(tmux: &Tmux, panes: &[PaneRecord]) -> bool {
    match tmux.get_server_option(opt::SETPF_OK) {
        Ok(Some(v)) => {
            let ok = v == "1";
            if !ok {
                warn_degrade_once();
            }
            return ok;
        }
        Ok(None) => {}         // not probed yet — fall through to probe
        Err(_) => return true, // server gone / read failed: assume modern, don't cache
    }

    let Some(pane) = panes.first() else {
        return true; // nothing to probe against; retry on a later cycle with panes
    };
    match probe_conditional_writes(tmux, &pane.pane_id) {
        Ok(ok) => {
            let _ = tmux.set_server_option(opt::SETPF_OK, if ok { "1" } else { "0" });
            if !ok {
                warn_degrade_once();
            }
            ok
        }
        Err(_) => true, // probe failed mid-run: default guarded, leave the cache unset
    }
}

fn warn_degrade_once() {
    if !DEGRADE_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("tma: this tmux lacks `set-option -F` expansion; legacy writes may be advisory.");
    }
}

/// Behaviour probe for `set-option -pF`: tests that the format actually *expands* server-side (a
/// pre-3.2 tmux stores the literal string on a bare success), else degrade to advisory writes.
pub fn probe_conditional_writes(tmux: &Tmux, pane_id: &str) -> Result<bool, TmuxError> {
    const KEY: &str = "@__tma_probe";
    let set = StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-F".into(),
            "-t".into(),
            pane_id.into(),
            KEY.into(),
            // Expands to `ok` only if `-F` evaluates the conditional.
            "#{?#{==:1,1},ok,no}".into(),
        ],
    };
    tmux.apply(&[set])?;
    let value = tmux.display(pane_id, &format!("#{{{KEY}}}"))?;
    let unset = StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-u".into(),
            "-t".into(),
            pane_id.into(),
            KEY.into(),
        ],
    };
    let _ = tmux.apply(&[unset]);
    Ok(value == "ok")
}

/// Apply an owned plan and return persisted truth. Window/session summaries are reconciled
/// separately through explicit-context CAS commands. `guarded` is the legacy capability hint;
/// every operation still checks ownership and execution-time hook support.
pub fn apply(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    plan: &StampPlan,
    guarded: bool,
) -> Result<Applied, TmuxError> {
    apply_with(tmux, all_panes, pane_id, plan, guarded, &[])
}

/// Apply with independent trailing commands after a verified complete commit.
/// State-dependent pane companions use `apply_with_edits` instead.
pub fn apply_with(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    plan: &StampPlan,
    guarded: bool,
    trailing: &[StampCommand],
) -> Result<Applied, TmuxError> {
    apply_with_edits(tmux, all_panes, pane_id, plan, guarded, trailing, &[])
}

/// State-dependent companions share the complete tuple's ownership decision and readback.
#[allow(clippy::too_many_arguments)]
pub fn apply_with_edits(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    plan: &StampPlan,
    _guarded: bool,
    trailing: &[StampCommand],
    companions: &[OptionEdit],
) -> Result<Applied, TmuxError> {
    let observed = observed_pane(tmux, all_panes, pane_id)?;
    apply_observed(tmux, observed, plan, trailing, companions, None)
}

/// A poll cycle may batch its advisory hint with the exact pane write. The hint
/// certifies server execution, even if a later receipt replacement defeats readback.
pub fn apply_cycle(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    plan: &StampPlan,
    _guarded: bool,
    hint: Option<persistence::PollHint>,
    companions: &[OptionEdit],
) -> Result<Applied, TmuxError> {
    let observed = observed_pane(tmux, all_panes, pane_id)?;
    apply_observed(tmux, observed, plan, &[], companions, hint)
}

fn observed_pane(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
) -> Result<PaneRead, TmuxError> {
    let actual = if tma_core::program_status::PaneId::parse(pane_id).is_ok() {
        pane_id.to_owned()
    } else {
        tmux.pane_format(pane_id, "#{pane_id}")?
    };
    let observed = if let Some(pane) = all_panes.iter().find(|pane| pane.pane_id == actual) {
        let mut read = PaneRead::from_options(pane.pane_id.clone(), pane.options.clone());
        read.target = pane_id.to_owned();
        read
    } else {
        persistence::read_pane(tmux, pane_id).map_err(persistence_error)?
    };
    Ok(observed)
}

/// Hook state is bound to the metadata session checked before its independent update.
pub fn apply_for_metadata(
    tmux: &Tmux,
    pane: &str,
    plan: &StampPlan,
    expected: Option<&tma_core::persistence::MetadataOwner>,
    companions: &[OptionEdit],
) -> Result<Applied, TmuxError> {
    let observed = persistence::read_pane(tmux, pane).map_err(persistence_error)?;
    if persistence::metadata_owner(&observed).as_ref() != expected {
        return Ok(Applied {
            committed: false,
            read: observed,
        });
    }
    apply_observed(tmux, observed, plan, &[], companions, None)
}

fn apply_observed(
    tmux: &Tmux,
    observed: PaneRead,
    plan: &StampPlan,
    trailing: &[StampCommand],
    companions: &[OptionEdit],
    hint: Option<persistence::PollHint>,
) -> Result<Applied, TmuxError> {
    let pane_id = &observed.target;
    if matches!(plan, StampPlan::EndSession) {
        let (expected, current) = metadata_binding(tmux, &observed)?;
        if current.is_none() {
            return Ok(Applied {
                committed: false,
                read: observed,
            });
        }
        let edits: Vec<_> = tma_core::persistence::METADATA_KEYS
            .iter()
            .map(|key| OptionEdit::unset(key))
            .collect();
        return match persistence::write_metadata(tmux, &observed, expected.as_ref(), None, &edits) {
            Ok(outcome) => Ok(Applied {
                committed: outcome.won(),
                read: outcome.winner().clone(),
            }),
            Err(PersistenceError::Unsupported) => Ok(Applied {
                committed: false,
                read: persistence::advisory_metadata(
                    tmux,
                    &observed,
                    expected.as_ref(),
                    None,
                    &edits,
                )
                .map_err(persistence_error)?,
            }),
            Err(error) => Err(persistence_error(error)),
        };
    }
    let checked_plan = if let StampPlan::Publish(publish) = plan {
        observed
            .options
            .get(tma_core::persistence::opt::BLOCKED_HISTORY)
            .and_then(|raw| tma_core::persistence::BlockedHistory::parse(raw))
            .map(|history| {
                let mut publish = publish.clone();
                let current = blocked_processes_current(tmux, &observed, &history.processes);
                // A stale projection supplies no fold evidence. Its independently checked
                // process can still identify a continuing hook fallback presentation.
                if publish.pid == 0
                    && publish.source == Provenance::Hook
                    && current
                    && persistence::metadata_owner(&observed)
                        .is_some_and(|owner| owner.process == history.processes.agent())
                    && observed
                        .options
                        .get(opt::PID)
                        .and_then(|raw| raw.parse::<u32>().ok())
                        == Some(history.processes.agent().pid)
                {
                    publish.pid = history.processes.agent().pid;
                }
                if !current || publish.pid != history.processes.agent().pid {
                    publish.episode_reset = true;
                    publish.set_attention |= publish.state == AgentState::Blocked;
                }
                StampPlan::Publish(publish)
            })
    } else {
        None
    };
    let plan = checked_plan.as_ref().unwrap_or(plan);
    let previous = tma_core::persistence::current_stamp(&observed.options, clock())
        .ok()
        .flatten()
        .map(|stamp| stamp.into_inner());
    // Duration is not authority. An unchanged same-process state keeps its public run start
    // through fallback, while the fold still receives no stale program evidence.
    let duration_previous = previous.clone().or_else(|| {
        let StampPlan::Publish(publish) = plan else {
            return None;
        };
        tma_core::StampedState::from_options(&observed.options)
            .ok()
            .flatten()
            .map(|stamp| stamp.into_inner())
            .filter(|stamp| stamp.pid != 0 && stamp.pid == publish.pid)
    });
    let mut edits = persistence::edits_for_plan(pane_id, plan, duration_previous.as_ref())
        .map_err(persistence_error)?;
    if observed.program.program_origin && previous.is_none() {
        if let StampPlan::Publish(publish) = plan {
            // Stale protocol attention is not fallback completion evidence. Keep historical turn
            // markers for exact reconciliation, but publish only this capture's attention intent.
            edits.push(if publish.set_attention {
                OptionEdit::set(opt::ATTENTION, "1")
            } else {
                OptionEdit::unset(opt::ATTENTION)
            });
        }
    }
    edits.extend_from_slice(companions);
    let guard = match plan {
        StampPlan::Publish(publish) => publish.guard,
        _ => Guard::Unconditional,
    };
    let mut result =
        match persistence::write_fallback_with_hint(tmux, &observed, guard, clock, &edits, hint) {
            Ok(outcome) => Applied {
                committed: outcome.won(),
                read: outcome.winner().clone(),
            },
            Err(PersistenceError::Unsupported) => Applied {
                read: persistence::advisory_fallback(tmux, &observed, guard, &edits, clock)
                    .map_err(persistence_error)?,
                committed: false,
            },
            Err(error) => return Err(persistence_error(error)),
        };
    // Summaries are convergent hints derived from persisted pane truth, outside the restricted
    // nonyielding transaction. Never substitute the producer's intended state for a lost write.
    if matches!(plan, StampPlan::Remove) && result.committed {
        let expected = persistence::metadata_owner(&observed);
        let edits: Vec<_> = tma_core::persistence::METADATA_KEYS
            .iter()
            .map(|key| OptionEdit::unset(key))
            .collect();
        match persistence::write_metadata(tmux, &result.read, expected.as_ref(), None, &edits) {
            Ok(outcome) => result.read = outcome.winner().clone(),
            Err(PersistenceError::Unsupported) => {
                result.read = persistence::advisory_metadata(
                    tmux,
                    &result.read,
                    expected.as_ref(),
                    None,
                    &edits,
                )
                .map_err(persistence_error)?;
            }
            Err(error) => return Err(persistence_error(error)),
        }
        result.read = persistence::finish_removal(tmux, &result.read).map_err(persistence_error)?;
    }
    reconcile_summaries(tmux)?;
    if result.committed {
        tmux.apply(trailing)?;
    }
    Ok(result)
}

fn blocked_processes_current(
    tmux: &Tmux,
    observed: &PaneRead,
    processes: &tma_core::persistence::ProcessChain,
) -> bool {
    let Some(root) = tmux
        .pane_format(&observed.target, "#{pane_pid}")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
    else {
        return false;
    };
    if Some(root) != processes.as_slice().first().map(|process| process.pid) {
        return false;
    }
    crate::tmux::sample_process_chain(root, processes.agent().pid)
        .is_ok_and(|current| current == processes.as_slice())
}

/// Prepare summary CAS commands from one coherent pane read. Each scope snapshot is verified
/// again in the server command. A delayed reconciler cannot overwrite a
/// newer writer's aggregate, including after pane membership or target context changes.
/// A scope snapshot above 64 KiB retains its previous hint and emits one bounded warning.
pub fn summary_commands(tmux: &Tmux) -> Result<Vec<StampCommand>, TmuxError> {
    let panes = tmux.list_panes()?;
    let mut seen = Vec::new();
    let mut commands = Vec::new();
    let mut keys = persistence::PROJECTION_KEYS.to_vec();
    keys.push(opt::DETAIL);
    let fields = keys
        .iter()
        .map(|key| format!("#{{{key}}}"))
        .collect::<Vec<_>>()
        .join("|");
    let pane_loop = format!("#{{P:#{{pane_id}}|{fields};}}");
    let now = clock();
    for scope in [SummaryScope::Window, SummaryScope::Session] {
        for member in &panes {
            let key = (
                scope,
                member.session.as_str(),
                (scope == SummaryScope::Window).then_some(member.window_index),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            // list-panes and the native pane/window loops use tmux's session/window/pane order.
            // Include membership and explicit target context, not just the aggregate state count.
            let members: Vec<_> = panes
                .iter()
                .filter(|pane| in_scope(scope, pane, member))
                .collect();
            let body = members
                .iter()
                .map(|pane| {
                    format!(
                        "{}|{};",
                        pane.pane_id,
                        keys.iter()
                            .map(|key| pane.options.get(*key).map_or("", String::as_str))
                            .collect::<Vec<_>>()
                            .join("|")
                    )
                })
                .collect::<String>();
            if body.len() > 64 * 1024 {
                if !SUMMARY_BOUND_WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "tma: summary reconciliation skipped: {:?} scope at {}:{}.{} has a {} byte snapshot (limit 65536); its previous summary is retained",
                        scope, member.session, member.window_index, member.pane_index, body.len(),
                    );
                }
                continue;
            }
            let snapshot = format!("{}:{}:{body}", member.session, member.window_index);
            let members_format = if scope == SummaryScope::Window {
                pane_loop.clone()
            } else {
                format!("#{{W:{pane_loop}}}")
            };
            let snapshot_format = format!("#{{session_name}}:#{{window_index}}:{members_format}");
            let states: Vec<_> = members
                .iter()
                .filter_map(|pane| {
                    tma_core::persistence::current_stamp(&pane.options, now)
                        .ok()
                        .flatten()
                })
                .map(|stamp| stamp.into_inner().state)
                .collect();
            let desired = render::summary_string(&states);
            let stored = match scope {
                SummaryScope::Window => member.window_summary.as_deref(),
                SummaryScope::Session => member.session_summary.as_deref(),
            };
            if desired.as_deref() == stored {
                continue;
            }
            let predicate = format!(
                "#{{==:{snapshot_format},{}}}",
                render::literal_tmux_format(&snapshot)
            );
            let target = format!("={}:.{}", member.session, member.pane_id);
            commands.push(render::conditional_group(
                &target,
                &predicate,
                &[render::render_summary(scope, &target, desired.as_deref())],
            ));
        }
    }
    Ok(commands)
}

pub fn reconcile_summaries(tmux: &Tmux) -> Result<(), TmuxError> {
    for command in summary_commands(tmux)? {
        let pane = command.argv.get(3).ok_or_else(|| TmuxError::Parse {
            cmd: "summary".into(),
            reason: "missing explicit target".into(),
        })?;
        tmux.apply_persistence(pane, std::slice::from_ref(&command))?;
    }
    Ok(())
}

/// Apply state-dependent housekeeping without an unguarded alternate write path.
pub fn apply_companions(
    tmux: &Tmux,
    observed: &PaneRead,
    edits: &[OptionEdit],
) -> Result<Applied, TmuxError> {
    match persistence::write_fallback(tmux, observed, Guard::Unconditional, clock, edits) {
        Ok(outcome) => Ok(Applied {
            committed: outcome.won(),
            read: outcome.winner().clone(),
        }),
        Err(PersistenceError::Unsupported) => Ok(Applied {
            committed: false,
            read: persistence::advisory_fallback(
                tmux,
                observed,
                Guard::Unconditional,
                edits,
                clock,
            )
            .map_err(persistence_error)?,
        }),
        Err(error) => Err(persistence_error(error)),
    }
}

/// Literal housekeeping commands, admitted through the shared state owner rather than directly.
pub fn apply_commands(
    tmux: &Tmux,
    observed: &PaneRead,
    commands: &[StampCommand],
) -> Result<Applied, TmuxError> {
    let edits: Result<Vec<_>, _> = commands.iter().map(persistence::edit_from_plain).collect();
    apply_companions(tmux, observed, &edits.map_err(persistence_error)?)
}

/// Apply a context-utilization observation to `pane_id`: the guarded `-F` evidence-time write
/// when the server supports it, else protected advisory mutations under the same metadata binding
/// and evidence-time guard. `pct = None` is a
/// null-clear, `tokens = None` clears the absolute count pair. The caller validates its payload
/// session; the original pane snapshot fences that attribution through the write.
pub fn apply_context(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    pct: Option<u8>,
    tokens: Option<u64>,
    evidence_at: u64,
    guarded: bool,
) -> Result<(), TmuxError> {
    let _ = guarded;
    apply_metadata_observation(
        tmux,
        all_panes,
        pane_id,
        opt::CONTEXT_AT,
        evidence_at,
        &render::render_context_advisory(pane_id, pct, tokens, evidence_at),
    )
}

/// Apply a quota/cost observation to `pane_id`, [`apply_context`]'s twin on the `@agent_quota_at`
/// marker: the guarded `-F` evidence-time write when the server supports it, else the advisory
/// degrade. One chained invocation, `None` fields clearing their option. The ownership filter is
/// the caller's, as on the context path; this is the store.
pub fn apply_quota(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane_id: &str,
    quota: &render::QuotaStamp,
    evidence_at: u64,
    guarded: bool,
) -> Result<(), TmuxError> {
    let _ = guarded;
    apply_metadata_observation(
        tmux,
        all_panes,
        pane_id,
        opt::QUOTA_AT,
        evidence_at,
        &render::render_quota_advisory(pane_id, quota, evidence_at),
    )
}

/// Metadata updates bind the current process birth and session, separately from state ownership.
/// Runtime supplies the same session checks on intake; this adapter also protects direct callers.
pub fn apply_metadata_edits(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane: &str,
    edits: &[OptionEdit],
) -> Result<bool, TmuxError> {
    let Some((observed, expected, next)) = metadata_observation(tmux, all_panes, pane)? else {
        return Ok(false);
    };
    metadata_write(tmux, &observed, expected.as_ref(), Some(&next), edits)
}

fn metadata_binding(
    tmux: &Tmux,
    observed: &PaneRead,
) -> Result<
    (
        Option<tma_core::persistence::MetadataOwner>,
        Option<tma_core::persistence::MetadataOwner>,
    ),
    TmuxError,
> {
    let expected = persistence::metadata_owner(observed);
    let pane_pid = tmux
        .pane_format(&observed.target, "#{pane_pid}")?
        .parse::<u32>()
        .ok();
    let pid = observed
        .owner()
        .map(|owner| owner.processes.agent().pid)
        .or_else(|| {
            observed
                .options
                .get(opt::PID)?
                .parse()
                .ok()
                .filter(|pid| *pid != 0)
        })
        .or_else(|| expected.as_ref().map(|owner| owner.process.pid))
        .or(pane_pid);
    let next = pid.and_then(|pid| {
        let chain = crate::tmux::sample_process_chain(pane_pid?, pid).ok()?;
        let process = *chain.last()?;
        if observed
            .owner()
            .is_some_and(|owner| owner.processes.as_slice() != chain)
        {
            return None;
        }
        if expected
            .as_ref()
            .is_some_and(|owner| owner.process != process)
        {
            return None;
        }
        let session = observed
            .options
            .get(opt::SESSION)
            .cloned()
            .or_else(|| expected.as_ref().map(|owner| owner.session.clone()))
            .unwrap_or_else(|| "<unattributed>".into());
        Some(tma_core::persistence::MetadataOwner { process, session })
    });
    Ok((expected, next))
}

/// Keep the process/session attribution of the caller's payload. Refresh only the admission
/// receipt, allowing context, quota and model from one observation to follow each other.
fn metadata_observation(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane: &str,
) -> Result<
    Option<(
        PaneRead,
        Option<tma_core::persistence::MetadataOwner>,
        tma_core::persistence::MetadataOwner,
    )>,
    TmuxError,
> {
    let original = observed_pane(tmux, all_panes, pane)?;
    let (original_owner, Some(original_next)) = metadata_binding(tmux, &original)? else {
        return Ok(None);
    };
    let current = persistence::read_pane(tmux, pane).map_err(persistence_error)?;
    let (expected, Some(next)) = metadata_binding(tmux, &current)? else {
        return Ok(None);
    };
    if next != original_next
        || (expected != original_owner && expected.as_ref() != Some(&original_next))
    {
        return Ok(None);
    }
    Ok(Some((current, expected, next)))
}

fn metadata_write(
    tmux: &Tmux,
    observed: &PaneRead,
    expected: Option<&tma_core::persistence::MetadataOwner>,
    next: Option<&tma_core::persistence::MetadataOwner>,
    edits: &[OptionEdit],
) -> Result<bool, TmuxError> {
    match persistence::write_metadata(tmux, observed, expected, next, edits) {
        Ok(outcome) => Ok(outcome.won()),
        Err(PersistenceError::Unsupported) => {
            persistence::advisory_metadata(tmux, observed, expected, next, edits)
                .map_err(persistence_error)?;
            Ok(false)
        }
        Err(error) => Err(persistence_error(error)),
    }
}

fn apply_metadata_observation(
    tmux: &Tmux,
    all_panes: &[PaneRecord],
    pane: &str,
    key: &str,
    at: u64,
    commands: &[StampCommand],
) -> Result<(), TmuxError> {
    let Some((observed, expected, next)) = metadata_observation(tmux, all_panes, pane)? else {
        return Ok(());
    };
    let previous = observed
        .options
        .get(key)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if at < previous {
        return Ok(());
    }
    let edits: Result<Vec<_>, _> = commands.iter().map(persistence::edit_from_plain).collect();
    let edits = edits.map_err(persistence_error)?;
    let evidence = if key == opt::CONTEXT_AT {
        tma_core::persistence::MetadataEvidence::Context(at)
    } else {
        tma_core::persistence::MetadataEvidence::Quota(at)
    };
    match persistence::write_metadata_observation(
        tmux,
        &observed,
        expected.as_ref(),
        Some(&next),
        &edits,
        Some(evidence),
    ) {
        Ok(_) => {}
        Err(PersistenceError::Unsupported) => {
            persistence::advisory_metadata_observation(
                tmux,
                &observed,
                expected.as_ref(),
                Some(&next),
                &edits,
                Some(evidence),
            )
            .map_err(persistence_error)?;
        }
        Err(error) => return Err(persistence_error(error)),
    }
    Ok(())
}

/// Arm a notify marker on `pane_id` (`key` is `@agent_context_notified_at` or
/// `@agent_stall_notified_at`): a guarded set-from-absent plus a mandatory read-back, so two
/// concurrent firers resolve to one bell. Returns `true` when THIS call won the marker (its value is
/// the stored one), so only the winner fires. Degrades to a producer-side read-decide-write on a
/// server without `-F`. `now` is the marker value (debuggability only).
pub fn arm_notify_marker(
    tmux: &Tmux,
    rec: &PaneRecord,
    key: &'static str,
    now: u64,
    _guarded: bool,
) -> Result<bool, TmuxError> {
    let observed = PaneRead::from_options(rec.pane_id.clone(), rec.options.clone());
    match persistence::update_notify_marker(tmux, &observed, key, Some(now)) {
        Ok(outcome) => Ok(outcome.won()),
        Err(PersistenceError::Unsupported) => {
            persistence::advisory_notify_marker(tmux, &observed, key, Some(now))
                .map(|outcome| outcome.claimed())
                .map_err(persistence_error)
        }
        Err(error) => Err(persistence_error(error)),
    }
}

/// Rearm only the marker/metric/state observation that requested it.
pub fn rearm_notify_marker(
    tmux: &Tmux,
    rec: &PaneRecord,
    key: &'static str,
) -> Result<(), TmuxError> {
    let observed = PaneRead::from_options(rec.pane_id.clone(), rec.options.clone());
    match persistence::update_notify_marker(tmux, &observed, key, None) {
        Ok(_) => Ok(()),
        Err(PersistenceError::Unsupported) => {
            persistence::advisory_notify_marker(tmux, &observed, key, None)
                .map(|_| ())
                .map_err(persistence_error)
        }
        Err(error) => Err(persistence_error(error)),
    }
}

/// Whether `pane` rolls up into the same `scope` as `member`.
fn in_scope(scope: SummaryScope, pane: &PaneRecord, member: &PaneRecord) -> bool {
    pane.session == member.session
        && (scope == SummaryScope::Session || pane.window_index == member.window_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::verdict::{WinningEvidence, WritePlan};
    use tma_core::{Detail, WriteAction};

    fn verdict(
        state: AgentState,
        source: Provenance,
        at: u64,
        may_override: bool,
        episode_reset: bool,
    ) -> Verdict {
        Verdict {
            state,
            detail: None,
            winning_evidence: WinningEvidence {
                origin: None,
                source,
                at,
                label: "t".into(),
            },
            writes: WritePlan {
                action: WriteAction::Publish,
                may_override,
                set_attention: false,
                episode_reset,
            },
            program: None,
        }
    }

    #[test]
    fn may_override_maps_to_unconditional() {
        let v = verdict(AgentState::Unknown, Provenance::Process, 10, true, false);
        assert_eq!(guard_for_verdict(&v), Guard::Unconditional);
    }

    #[test]
    fn episode_reset_is_unconditional_regardless_of_override() {
        let v = verdict(AgentState::Working, Provenance::Capture, 10, false, true);
        assert_eq!(guard_for_verdict(&v), Guard::Unconditional);
    }

    #[test]
    fn capture_working_protects_a_hook_claim() {
        let v = verdict(AgentState::Working, Provenance::Capture, 10, false, false);
        assert_eq!(guard_for_verdict(&v), Guard::ProtectHook);
    }

    #[test]
    fn blocker_chrome_uses_carveout_with_capture_time() {
        let v = verdict(AgentState::Blocked, Provenance::Capture, 42, false, false);
        assert_eq!(guard_for_verdict(&v), Guard::CarveOut { capture_at: 42 });
    }

    #[test]
    fn hook_corroboration_uses_refresh_claim() {
        let v = verdict(AgentState::Working, Provenance::Hook, 99, false, false);
        assert_eq!(
            guard_for_verdict(&v),
            Guard::RefreshClaim {
                state: AgentState::Working
            }
        );
    }

    #[test]
    fn detail_survives_into_publish_guard_wrapping() {
        // A blocked publish keeps its detail; sanity that the mapping does not drop it.
        let mut v = verdict(AgentState::Blocked, Provenance::Capture, 5, false, false);
        v.detail = Some(Detail::new("permission"));
        // The guard choice does not depend on detail, but the driver carries it through.
        assert_eq!(guard_for_verdict(&v), Guard::CarveOut { capture_at: 5 });
    }
}
