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
    // A malformed or deliberately-ignored payload (a cumulative-shape reading) yields `None`: do nothing.
    let Some(report) = tma_core::parse_context(&format, &payload) else {
        return ExitCode::SUCCESS;
    };

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
    let now = crate::now_ms();
    stamp_context(&tmux, &pane, &report, now);

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

/// Ownership-filter then stamp a context observation. Only the owning `@agent_session` may
/// stamp: a foreign session (a subagent running its own statusline) is dropped so it cannot clobber
/// the parent pane's gauge. A pane with no recorded owner, or a payload carrying no session, is
/// unattributable and proceeds — the same posture as the hook path's subagent guard.
fn stamp_context(tmux: &Tmux, pane: &str, report: &tma_core::ContextReport, now: u64) {
    let panes = match tmux.list_panes() {
        Ok(p) => p,
        Err(_) => return, // server gone or unreadable
    };
    let Some(rec) = panes.iter().find(|r| r.pane_id == pane) else {
        return;
    };
    let owner = rec.options.get(opt::SESSION).filter(|v| !v.is_empty());
    if let (Some(owner), Some(ev)) = (owner, report.session.as_deref()) {
        if owner != ev {
            return; // a subagent's own session must not clobber the owner's gauge
        }
    }
    let guarded = stamp::guarded_writes_supported(tmux, &panes);
    let _ = stamp::apply_context(tmux, &panes, pane, report.pct, report.tokens, now, guarded);
}
