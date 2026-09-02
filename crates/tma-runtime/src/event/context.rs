use std::process::ExitCode;

use tma_core::stamp::opt;
use tma_tmux::stamp;
use tma_tmux::tmux::Tmux;

use crate::manifests;

use super::{read_payload, EventArgs};

/// The context-telemetry intake: resolve the pane, look up the agent's `[telemetry.context]`
/// format, parse the payload with the compiled-in pure parser, then ownership-filter and stamp under
/// the evidence-time guard. Direct-stamp only (the metric lane is independent of the state lane and
/// guarded, so it needs no daemon hand-off). Like every hook path it exits 0 on every miss.
pub(super) fn run_context(args: EventArgs) -> ExitCode {
    // Pane: the shim's explicit `--pane "$TMUX_PANE"` wins; fall back to the env for a hand-wired shim.
    let pane = args
        .pane
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()));
    let Some(pane) = pane else {
        return ExitCode::SUCCESS;
    };

    // A skipped user manifest is silent here: the hook path must never write to a hook's stderr.
    let manifests = match manifests::load(args.manifest_dir.as_deref(), &args.agents) {
        Ok(set) => set.manifests,
        Err(_) => return ExitCode::SUCCESS,
    };
    // The agent's context channel format; absent ⇒ this agent has no context telemetry, nothing to do.
    let Some(format) = manifests
        .iter()
        .find(|m| m.name == args.agent)
        .and_then(|lm| lm.manifest.telemetry.as_ref())
        .and_then(|t| t.context.as_ref())
        .map(|c| c.format.clone())
    else {
        return ExitCode::SUCCESS;
    };

    let payload = read_payload(args.payload.as_deref());
    // A malformed or deliberately-ignored payload (a cumulative-shape reading) yields `None`. The
    // quota half is parsed from the same bytes and is independently absent: a payload can carry a
    // gauge with no `rate_limits` block, or the reverse.
    let now = crate::now_ms();
    let context = tma_core::parse_context(&format, &payload);
    let usage = tma_core::parse_usage(&format, &payload, now);
    if context.is_none() && usage.is_none() {
        return ExitCode::SUCCESS;
    }

    // Daemonless context-high notify rides the same `from_event` opt-in as state: when a daemon
    // runs it dispatches from its reconcile instead, so the two paths never double-fire (the marker
    // would dedup them regardless). `TMA_NOTIFY_FROM_EVENT` overrides the config flag (test/CI seam).
    let notify_opt_in = match std::env::var("TMA_NOTIFY_FROM_EVENT") {
        Ok(v) => v == "1",
        Err(_) => args.notify_from_event,
    };
    let commands = args
        .notify_commands
        .clone()
        .overridden_by(crate::config::notify_cmd_env());
    let command = commands.for_context_high();

    let tmux = Tmux::connect(&args.server);
    stamp_observation(&tmux, &pane, context.as_ref(), usage.as_ref(), now);

    if notify_opt_in {
        if let Some(threshold) = args.notify_context_high {
            fire_context_high(&tmux, &pane, threshold, command, &args.notify_sinks, now);
        }
    }
    ExitCode::SUCCESS
}

/// Daemonless context-high notify for one pane: re-read the just-stamped gauge + armed flag,
/// run the shared decision, and fire iff it won the guarded marker. Re-reads post-stamp so a stamp the
/// evidence-time guard dropped decides off the winner's value, never this call's stale observation.
fn fire_context_high(
    tmux: &Tmux,
    pane: &str,
    threshold: u8,
    command: Option<&str>,
    sinks: &crate::config::NotifySinks,
    now: u64,
) {
    let Ok(panes) = tmux.list_panes() else {
        return;
    };
    let Some(rec) = panes.iter().find(|r| r.pane_id == pane) else {
        return;
    };
    let guarded = stamp::guarded_writes_supported(tmux, &panes);
    if let Some(mut child) =
        crate::notify::evaluate_context_high(tmux, guarded, rec, threshold, command, sinks, now)
    {
        // Deliver before this one-shot process exits (matches the state direct-fire).
        let _ = child.wait();
    }
}

/// Ownership-filter then stamp one payload's observations: the context gauge on its own chain, the
/// quota/cost trio on its own, and the model label as a plain set. Only the owning `@agent_session`
/// may stamp: a foreign session (a subagent running its own statusline) is dropped so it cannot
/// clobber the parent pane's values. A pane with no recorded owner, or a payload carrying no
/// session, is unattributable and proceeds, the same posture as the hook path's subagent guard.
///
/// The two chains are independent and each guards on its own marker, so a payload carrying only one
/// of them touches only that lane; one `list_panes` serves both.
fn stamp_observation(
    tmux: &Tmux,
    pane: &str,
    context: Option<&tma_core::ContextReport>,
    usage: Option<&tma_core::UsageReport>,
    now: u64,
) {
    let panes = match tmux.list_panes() {
        Ok(p) => p,
        Err(_) => return, // server gone or unreadable
    };
    let Some(rec) = panes.iter().find(|r| r.pane_id == pane) else {
        return;
    };
    let owner = rec.options.get(opt::SESSION).filter(|v| !v.is_empty());
    let session = context
        .and_then(|c| c.session.as_deref())
        .or_else(|| usage.and_then(|u| u.session.as_deref()));
    if let (Some(owner), Some(ev)) = (owner, session) {
        if owner != ev {
            return; // a subagent's own session must not clobber the owner's values
        }
    }

    let guarded = stamp::guarded_writes_supported(tmux, &panes);
    if let Some(report) = context {
        let _ = stamp::apply_context(tmux, &panes, pane, report.pct, report.tokens, now, guarded);
    }
    let Some(usage) = usage else { return };
    if usage.has_quota_observation() {
        let cost = usage.cost_usd.and_then(tma_core::format_cost_usd);
        let quota = tma_core::QuotaStamp {
            pct: usage.quota.as_ref().map(|q| q.pct),
            window: usage.quota.as_ref().map(|q| q.window.token()),
            resets_at_ms: usage.quota.as_ref().and_then(|q| q.resets_at_ms),
            cost_usd: cost.as_deref(),
        };
        let _ = stamp::apply_quota(tmux, &panes, pane, &quota, now, guarded);
    }
    // The model label rides no chain: a plain last-write-wins set, as the rollout tail does it.
    if let Some(model) = usage.model.as_deref() {
        let cmd = tma_core::render::set_pane_option(pane, opt::MODEL, model);
        let _ = tmux.apply(&[cmd]);
    }
}
