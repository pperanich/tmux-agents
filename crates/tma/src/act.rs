//! `tma act`: fire a guarded action into an agent pane, or enumerate/menu the fireable ones.
//! One verb, three modes: `tma act <name>` fires (through [`broker::fire`], which owns the guard
//! sequence and the exit-code mapping — this module only resolves the target, enforces `confirm`,
//! and formats); `tma act --list` enumerates actions with an optional per-pane fireability verdict;
//! `tma act --menu` renders a tmux `display-menu` of the currently-fireable actions.
//!
//! Target resolution mirrors `wait`: `--pane <ID>` names it, the selector flags resolve to a unique
//! pane (0 = exit 3, >1 = exit 1), and with neither the current pane comes from `$TMUX_PANE`.
//! `--all` turns that same selection into the whole target set, fired sequentially through the same
//! per-pane broker path (its own lock, its own gate re-verification), with one confirmation for the
//! batch and the worst result's exit code. Exit codes are the broker's (`0`/`1`/`3`/`4`/`5`/`124`)
//! plus `2` for usage; `--json` prints the schema-1 result object whose `outcome` field is the
//! authoritative, drift-pinned vocabulary (`--all` wraps those objects in a `results` envelope).

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use tma_core::{ActionKind, ActionManifest, AgentRow, FoldConfig, Selector, When};
use tma_runtime::broker::{self, ActResult, Outcome, TmuxBroker};
use tma_runtime::json::{JsonWriter, JSON_SCHEMA};
use tma_runtime::{actions, cycle, manifests, MenuItem};

use crate::cli_support;
use crate::config::Config;
use crate::tmux::{self, Tmux};

/// Everything `tma act` needs, assembled by the bin's dispatch from the CLI args and loaded config.
pub(crate) struct ActOpts {
    pub name: Option<String>,
    pub pane: Option<String>,
    /// The selector flags. Its `agent` field doubles as the by-name target, exactly as in `wait`.
    pub selector: Selector,
    /// Fire on every selector-matched pane instead of requiring a unique one.
    pub all: bool,
    pub dry_run: bool,
    /// The `--arg` values, for an `exec` action's environment (`keys` actions reject them).
    pub args: Vec<String>,
    pub force: bool,
    pub yes: bool,
    pub json: bool,
    pub list: bool,
    pub menu: bool,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    pub config: Config,
}

pub(crate) fn run(opts: ActOpts) -> ExitCode {
    // The effective action set (bundled + user dir, shadowed by stem), loaded fresh every invocation.
    let action_set = match actions::load(None) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("tma: action load failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    let manifests = match cli_support::load_manifests_or_exit(
        opts.manifest_dir.as_deref(),
        &opts.config.agent_overrides,
    ) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let cfg = opts.config.fold_config();
    let tmux = Tmux::connect(&opts.server);

    if opts.list {
        return run_list(
            &action_set,
            opts.pane,
            opts.json,
            &tmux,
            &manifests,
            &cfg,
            &opts.config.api,
        );
    }
    if opts.menu {
        return run_menu(
            &action_set,
            opts.pane,
            &tmux,
            &manifests,
            &cfg,
            &opts.config.api,
            &opts.server,
        );
    }

    // Fire (or dry-run) one named action.
    let Some(name) = opts.name.as_deref() else {
        eprintln!("tma: no action named (usage: `tma act <name>`, or `--list` / `--menu`)");
        return ExitCode::from(2);
    };
    let Some(action) = actions::find(&action_set, name) else {
        eprintln!("tma: unknown action {name:?} (run `tma act --list` to see them)");
        return ExitCode::from(2);
    };
    // A `keys` action's sequence is manifest-static by design (that is what makes it reviewable), so
    // there is nowhere for a value to go: refuse rather than accept and silently drop it.
    if !opts.args.is_empty() && action.kind == ActionKind::Keys {
        eprintln!(
            "tma: `{name}` is a keys action and takes no --arg \
             (its key sequence comes from the manifest); use an exec action to pass values"
        );
        return ExitCode::from(2);
    }

    let panes = match resolve_targets(&opts, &tmux, &manifests, &cfg) {
        Ok(p) => p,
        Err(code) => return code,
    };

    if opts.dry_run {
        let io = TmuxBroker {
            tmux: &tmux,
            manifests: &manifests,
            cfg: &cfg,
            api_bases: &opts.config.api,
            server: tma_tmux::tmux::Server::default(),
            notify_command: None,
        };
        let runs: Vec<broker::DryRun> = panes
            .iter()
            .map(|pane| broker::dry_run(&io, action, pane))
            .collect();
        // The fan-out wants the verdict per target at a glance; a single target keeps the detailed
        // block (context ages and the would-be effect), which is what the author is iterating on.
        if opts.all {
            print!("{}", render_dry_run_targets(&runs));
        } else {
            print!("{}", render_dry_run(&runs[0]));
        }
        return ExitCode::SUCCESS;
    }

    // `confirm` enforcement (per-surface): a TTY prompts, `--yes` bypasses, a non-TTY refuses
    // so a script cannot stumble into a second-factor action. One prompt covers the batch: the
    // second factor is the operator's intent to fire this action here, not a per-pane ritual.
    if let Some(refusals) = confirm_gate(action, &panes, opts.yes) {
        return emit_all(&refusals, opts.json, opts.all);
    }

    let detach = broker::DetachCtx {
        server: Some(&opts.server),
        notify_command: opts.config.notify.command.as_deref(),
    };
    // Sequential, one full broker sequence per pane: each target takes its own single-flight lock
    // and re-verifies its own gate, so a fan-out is exactly N independent fires, never a shortcut.
    let results: Vec<ActResult> = panes
        .iter()
        .map(|pane| {
            broker::fire(
                &tmux,
                &manifests,
                &cfg,
                &opts.config.api,
                detach,
                action,
                pane,
                broker::FireArgs {
                    force: opts.force,
                    args: &opts.args,
                },
            )
        })
        .collect();
    emit_all(&results, opts.json, opts.all)
}

// ---- target resolution (mirrors `wait`) --------------------------------------------------------

/// Resolve the target panes: an explicit `--pane`, every selector match under `--all`, a unique
/// selector match otherwise, else the current `$TMUX_PANE`. On failure the error is already printed
/// and the mapped exit code returned (`2` usage, `3` nothing matched, `1` an ambiguous selection).
fn resolve_targets(
    opts: &ActOpts,
    tmux: &Tmux,
    manifests: &[manifests::LoadedManifest],
    cfg: &FoldConfig,
) -> Result<Vec<String>, ExitCode> {
    if let Some(p) = &opts.pane {
        // A pane id is already unique, so scoping flags alongside it are a usage error rather than
        // a silently-ignored narrowing (the `wait --pane` rule).
        if !opts.selector.is_empty() {
            eprintln!(
                "tma: --pane names one pane; drop the selector flags \
                 (--session/--repo/--branch/--agent/--state) or use --all"
            );
            return Err(ExitCode::from(2));
        }
        return Ok(vec![p.clone()]);
    }
    if !opts.all && opts.selector.is_empty() {
        return match std::env::var("TMUX_PANE") {
            Ok(p) if !p.is_empty() => Ok(vec![p]),
            _ => {
                eprintln!(
                    "tma: not inside a tmux pane; name the target with --pane <ID> or --agent <NAME>"
                );
                Err(ExitCode::from(2))
            }
        };
    }

    // Deferred, never inline: `--state done` is idle + `@agent_attention`, so an inline clear would
    // retract the mark out of the rows the selector matches on and drop the target it just found.
    let mut report = match cycle::run_cycle_with(tmux, manifests, cfg, cycle::SeenClear::Deferred) {
        Ok(r) => r,
        Err(tmux::TmuxError::ServerGone) => return Err(cli_support::no_server()),
        Err(err) => {
            eprintln!("tma: {err}");
            return Err(ExitCode::FAILURE);
        }
    };
    // A repo/branch selector needs the labels the cycle deliberately leaves unresolved.
    if opts.selector.needs_repo() {
        tma_runtime::repo::annotate_rows(&mut report.rows);
    }
    let matched: Vec<&AgentRow> = report
        .rows
        .iter()
        .filter(|r| opts.selector.matches(r))
        .collect();
    let ids: Vec<String> = matched.iter().map(|r| r.pane_id.clone()).collect();
    // The cycle's clear, strictly after the selector read and before every early return below.
    if !report.deferred_seen.is_empty() {
        tma_runtime::seen::clear_seen(tmux, &report.deferred_seen);
    }

    if opts.all {
        if ids.is_empty() {
            eprintln!("tma: no agent pane matched the selector; nothing to act on (exit 2)");
            return Err(ExitCode::from(2));
        }
        return Ok(ids);
    }
    match ids.as_slice() {
        [_only] => Ok(ids),
        [] => {
            eprintln!("tma: {} (exit 3)", nothing_matched(&opts.selector));
            Err(ExitCode::from(3))
        }
        many => {
            eprintln!(
                "tma: {} matches {} panes ({}); target one with --pane, or fire on all of them with --all",
                scope_label(&opts.selector),
                many.len(),
                many.join(", ")
            );
            Err(ExitCode::FAILURE)
        }
    }
}

/// How to name the selection in an error: the by-name form only when `--agent` is the WHOLE scope
/// (the common case, and the wording `wait --agent` already uses), else the generic scope, so the
/// message never understates what narrowed the match.
fn scope_label(selector: &Selector) -> String {
    match &selector.agent {
        Some(name) if agent_is_the_whole_scope(selector) => format!("--agent {name:?}"),
        _ => "the selector".to_string(),
    }
}

fn nothing_matched(selector: &Selector) -> String {
    match &selector.agent {
        Some(name) if agent_is_the_whole_scope(selector) => {
            format!("no agent pane named {name:?}")
        }
        _ => "no agent pane matched the selector".to_string(),
    }
}

fn agent_is_the_whole_scope(selector: &Selector) -> bool {
    selector.session.is_none()
        && selector.repo.is_none()
        && selector.branch.is_none()
        && selector.state.is_empty()
}

// ---- confirm -----------------------------------------------------------------------------------

/// Enforce `confirm = true` at the CLI surface. `None` clears the action to fire; `Some(results)`
/// is one refusal per target (a declined prompt, or no TTY to prompt on) to emit. `--yes` and a
/// non-`confirm` action always clear. One prompt covers every target: it names them all, so the
/// operator confirms the blast radius rather than the same question N times.
fn confirm_gate(action: &ActionManifest, panes: &[String], yes: bool) -> Option<Vec<ActResult>> {
    if !action.confirm || yes {
        return None;
    }
    let err = |msg: String| {
        panes
            .iter()
            .map(|pane| ActResult {
                action: action.name.clone(),
                pane: pane.clone(),
                outcome: Outcome::Error(msg.clone()),
            })
            .collect()
    };
    if !io::stdin().is_terminal() {
        return Some(err(format!(
            "action {:?} needs confirmation; pass --yes (no TTY to prompt on)",
            action.name
        )));
    }
    if confirm_prompt(&action.name, panes) {
        None
    } else {
        Some(err("aborted at the confirmation prompt".to_string()))
    }
}

/// Prompt on the controlling TTY; `true` only on an explicit yes.
fn confirm_prompt(action: &str, panes: &[String]) -> bool {
    match panes {
        [one] => print!("tma: fire `{action}` on {one}? [y/N] "),
        many => print!(
            "tma: fire `{action}` on {} panes ({})? [y/N] ",
            many.len(),
            many.join(", ")
        ),
    }
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---- fire result rendering ---------------------------------------------------------------------

/// Emit a fire result: the schema-1 JSON object on stdout when `--json`, and always the human
/// "refusing fact" (or a brief note) on stderr. Returns the broker's exit code.
fn emit(result: &ActResult, json: bool) -> ExitCode {
    if json {
        println!("{}", render_act_json(result));
    }
    if let Some(note) = human_note(result) {
        eprintln!("{note}");
    }
    ExitCode::from(result.exit_code() as u8)
}

/// Emit a whole invocation's results. `fan_out` (`--all`) always emits the `results` envelope, even
/// for one target, so a script's parse does not depend on how many panes matched; without it the
/// single result keeps its bare object. Every result's human note still reaches stderr, and the exit
/// code is the WORST result's ([`ActResult::severity`]): a fan-out succeeds only if all of it did.
fn emit_all(results: &[ActResult], json: bool, fan_out: bool) -> ExitCode {
    if !fan_out {
        return emit(&results[0], json);
    }
    if json {
        println!("{}", render_act_batch_json(results));
    }
    for r in results {
        if let Some(note) = human_note(r) {
            eprintln!("{note}");
        }
    }
    let worst = results
        .iter()
        .max_by_key(|r| r.severity())
        .expect("at least one target");
    ExitCode::from(worst.exit_code() as u8)
}

/// The stderr line for a result: the refusing/failing fact for every non-success outcome, and a
/// brief confirmation for a delivered `keys` action. A synchronous exec child (`exited`) already
/// streamed its own output, so it gets none.
fn human_note(r: &ActResult) -> Option<String> {
    let code = r.exit_code();
    match &r.outcome {
        Outcome::Sent => Some(format!("tma: sent `{}` to {}", r.action, r.pane)),
        Outcome::Replied => Some(format!(
            "tma: replied `{}` to {} over the API",
            r.action, r.pane
        )),
        Outcome::Exited(_) | Outcome::Spawned => None,
        Outcome::Timeout => Some(format!(
            "tma: `{}` timed out and was killed (exit {code})",
            r.action
        )),
        Outcome::Refused(_) => Some(format!(
            "tma: `{}` refused on {}: {} (exit {code})",
            r.action,
            r.pane,
            r.reason().unwrap_or("refused"),
        )),
        Outcome::Vanished => Some(format!("tma: pane {} vanished (exit {code})", r.pane)),
        Outcome::Error(msg) => Some(format!("tma: `{}` failed: {msg} (exit {code})", r.action)),
    }
}

/// The `tma act <name> --json` result object. Exact key set: `schema`, `action`, `pane`,
/// `outcome`, `exit_code`, `reason` (drift-tested). `outcome` is the authoritative closed vocabulary;
/// `reason` carries the refusal token or is null.
fn render_act_json(r: &ActResult) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    write_act_result_fields(&mut j, r);
    j.end_object();
    j.finish()
}

/// One result as its own object, for the `--all` envelope's `results` array.
fn write_act_result(j: &mut JsonWriter, r: &ActResult) {
    j.begin_object();
    write_act_result_fields(j, r);
    j.end_object();
}

/// The result fields (no enclosing object, no `schema`), defined once so the single-fire object and
/// the `--all` envelope's elements cannot drift apart.
fn write_act_result_fields(j: &mut JsonWriter, r: &ActResult) {
    j.string("action", &r.action);
    j.string("pane", &r.pane);
    j.string("outcome", r.outcome.token());
    j.number("exit_code", r.exit_code() as i64);
    match r.reason() {
        Some(reason) => j.string("reason", reason),
        None => j.null("reason"),
    }
}

/// The `tma act --all --json` envelope: `{"schema":1,"results":[<act result object>...]}`, reusing
/// the pinned per-pane key set so a consumer parses one element shape either way. Drift-tested.
fn render_act_batch_json(results: &[ActResult]) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("results");
    j.begin_array();
    for r in results {
        write_act_result(&mut j, r);
    }
    j.end_array();
    j.end_object();
    j.finish()
}

/// The `--all --dry-run` report: one line per resolved target with its gate verdict, so an operator
/// sees the blast radius (and what would be refused) before firing anything.
fn render_dry_run_targets(runs: &[broker::DryRun]) -> String {
    use broker::DryGate;

    let mut out = format!("targets: {}\n", runs.len());
    for d in runs {
        let verdict = match d.gate {
            DryGate::Fireable => "would fire".to_string(),
            DryGate::Refused(refusal) => format!("refused: {}", refusal.token()),
            DryGate::Vanished => "vanished (pane gone)".to_string(),
        };
        out.push_str(&format!(
            "{:<8} {:<12} {}\n",
            d.pane,
            d.agent.as_deref().unwrap_or("(none)"),
            verdict
        ));
    }
    out
}

/// Human `--dry-run` output: the resolved context with each value's age, the gate verdict,
/// and the would-be keys or command — no side effects.
fn render_dry_run(d: &broker::DryRun) -> String {
    use broker::{DryGate, Effect};

    let mut out = String::new();
    out.push_str(&format!("action:  {}\n", d.action));
    out.push_str(&format!("pane:    {}\n", d.pane));
    out.push_str(&format!(
        "agent:   {}\n",
        d.agent.as_deref().unwrap_or("(none)")
    ));
    let gate = match d.gate {
        DryGate::Fireable => "fireable".to_string(),
        DryGate::Refused(refusal) => format!("refused: {}", refusal.token()),
        DryGate::Vanished => "vanished (pane gone)".to_string(),
    };
    out.push_str(&format!("gate:    {gate}\n"));
    let effect = match &d.effect {
        Effect::Keys(seq) => format!("keys: {}", seq.join(" ")),
        Effect::Api {
            endpoint,
            op,
            reply,
        } => format!("api: POST {endpoint}/permission/<id>/reply  op={op} reply={reply}"),
        Effect::Command(cmd) => format!("command: {cmd}"),
        Effect::None => "none".to_string(),
    };
    out.push_str(&format!("effect:  {effect}\n"));
    out.push_str("context:\n");
    for c in &d.context {
        let age = match c.age_ms {
            Some(ms) => format!("  ({} ms old)", ms),
            None => "  (live)".to_string(),
        };
        out.push_str(&format!("  {:<15} {}{}\n", c.name, c.value, age));
    }
    out
}

// ---- --list ------------------------------------------------------------------------------------

fn run_list(
    actions_set: &[ActionManifest],
    pane: Option<String>,
    json: bool,
    tmux: &Tmux,
    manifests: &[manifests::LoadedManifest],
    cfg: &FoldConfig,
    api_bases: &tma_runtime::config::ApiSection,
) -> ExitCode {
    // Per-action fireability only when a pane is named; otherwise the plain enumeration.
    let verdicts = match &pane {
        Some(pane_id) => {
            let io = TmuxBroker {
                tmux,
                manifests,
                cfg,
                api_bases,
                server: tma_tmux::tmux::Server::default(),
                notify_command: None,
            };
            match broker::list_fireability(&io, actions_set, pane_id) {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    eprintln!("tma: pane {pane_id} does not exist (exit 3)");
                    return ExitCode::from(3);
                }
                Err(tmux::TmuxError::ServerGone) => return cli_support::no_server(),
                Err(err) => {
                    eprintln!("tma: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    if json {
        println!("{}", render_list_json(actions_set, verdicts.as_deref()));
    } else {
        print!("{}", render_list_text(actions_set, verdicts.as_deref()));
    }
    ExitCode::SUCCESS
}

/// The applicability list: a `keys` action's covered agents are the union of its
/// `[keys]` and `[api]` tables (the `--list` document reports the union, with no per-transport
/// surface in v1); an `exec` action's from `agents` (empty means all agents). Sorted + deduped so
/// the union is stable regardless of table order.
fn applicability(action: &ActionManifest) -> Vec<&str> {
    match action.kind {
        ActionKind::Keys => {
            let mut agents: Vec<&str> = action
                .keys
                .keys()
                .chain(action.api.keys())
                .map(String::as_str)
                .collect();
            agents.sort_unstable();
            agents.dedup();
            agents
        }
        ActionKind::Exec => action.agents.iter().map(String::as_str).collect(),
    }
}

fn kind_token(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Keys => "keys",
        ActionKind::Exec => "exec",
    }
}

/// `tma act --list --json`: a schema-1 document. With `verdicts` (given `--pane`) each action
/// also carries `fireable` + `reason`. Exact key set drift-tested.
fn render_list_json(
    actions_set: &[ActionManifest],
    verdicts: Option<&[broker::ListVerdict]>,
) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("actions");
    j.begin_array();
    for (i, action) in actions_set.iter().enumerate() {
        j.begin_object();
        j.string("name", &action.name);
        j.string("label", &action.label);
        j.string("kind", kind_token(action.kind));
        j.key("agents");
        j.begin_array();
        for agent in applicability(action) {
            j.raw_string(agent);
        }
        j.end_array();
        write_when(&mut j, action.when.as_ref());
        if let Some(verdicts) = verdicts {
            let verdict = &verdicts[i];
            j.bool("fireable", verdict.is_none());
            match verdict {
                Some(refusal) => j.string("reason", refusal.token()),
                None => j.null("reason"),
            }
        }
        j.end_object();
    }
    j.end_array();
    j.end_object();
    j.finish()
}

/// Render the optional `when` gate as an object (or null): `state`/`detail` token arrays and the
/// context-percent bounds (null when unset).
fn write_when(j: &mut JsonWriter, when: Option<&When>) {
    let Some(when) = when else {
        j.null("when");
        return;
    };
    j.key("when");
    j.begin_object();
    j.key("state");
    j.begin_array();
    for s in &when.state {
        j.raw_string(s.token());
    }
    j.end_array();
    j.key("detail");
    j.begin_array();
    for d in &when.detail {
        j.raw_string(d.as_str());
    }
    j.end_array();
    match when.context_pct_min {
        Some(v) => j.number("context_pct_min", v as i64),
        None => j.null("context_pct_min"),
    }
    match when.context_pct_max {
        Some(v) => j.number("context_pct_max", v as i64),
        None => j.null("context_pct_max"),
    }
    j.end_object();
}

fn render_list_text(
    actions_set: &[ActionManifest],
    verdicts: Option<&[broker::ListVerdict]>,
) -> String {
    let mut out = String::new();
    for (i, action) in actions_set.iter().enumerate() {
        let agents = applicability(action);
        let agents = if agents.is_empty() {
            "(all)".to_string()
        } else {
            agents.join(",")
        };
        let verdict = match verdicts {
            Some(v) => match &v[i] {
                None => "  fireable".to_string(),
                Some(refusal) => format!("  refused: {}", refusal.token()),
            },
            None => String::new(),
        };
        out.push_str(&format!(
            "{:<12} {:<5} {}{}\n",
            action.name,
            kind_token(action.kind),
            agents,
            verdict
        ));
    }
    out
}

// ---- --menu ------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_menu(
    actions_set: &[ActionManifest],
    pane: Option<String>,
    tmux: &Tmux,
    manifests: &[manifests::LoadedManifest],
    cfg: &FoldConfig,
    api_bases: &tma_runtime::config::ApiSection,
    server: &tma_tmux::tmux::Server,
) -> ExitCode {
    let pane = match pane {
        Some(p) => p,
        None => match std::env::var("TMUX_PANE") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                eprintln!("tma: not inside a tmux pane; name the target with --pane <ID>");
                return ExitCode::from(2);
            }
        },
    };

    let io = TmuxBroker {
        tmux,
        manifests,
        cfg,
        api_bases,
        server: tma_tmux::tmux::Server::default(),
        notify_command: None,
    };
    let verdicts = match broker::list_fireability(&io, actions_set, &pane) {
        Ok(Some(v)) => v,
        Ok(None) => {
            eprintln!("tma: pane {pane} does not exist (exit 3)");
            return ExitCode::from(3);
        }
        Err(tmux::TmuxError::ServerGone) => return cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };

    let fireable: Vec<(String, String)> = actions_set
        .iter()
        .zip(&verdicts)
        .filter(|(_, v)| v.is_none())
        .map(|(a, _)| (a.name.clone(), a.label.clone()))
        .collect();
    if fireable.is_empty() {
        eprintln!("tma: no actions are fireable on {pane} right now");
        return ExitCode::SUCCESS;
    }

    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "tma".to_string());
    let items: Vec<MenuItem> = tma_ui::menu::action_menu_items(&bin, server, &pane, &fireable);
    match tma_ui::menu::show(tmux, &pane, "tma actions", &items) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tma: cannot show the action menu: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_action(name: &str, when: &str, keys: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"{name}\"\nlabel = \"L\"\nkind = \"keys\"\n{when}\n[keys]\n{keys}\n"
        );
        ActionManifest::parse(&src, name, &format!("{name}.toml")).unwrap()
    }

    fn result(action: &str, pane: &str, outcome: Outcome) -> ActResult {
        ActResult {
            action: action.to_string(),
            pane: pane.to_string(),
            outcome,
        }
    }

    /// The `--json` fire result carries exactly the pinned key set; a dropped, renamed, or new key
    /// fails here.
    #[test]
    fn act_json_pins_full_key_set() {
        let json = render_act_json(&result("approve", "%5", Outcome::Sent));
        assert!(json.starts_with("{\"schema\":1"));
        assert_eq!(
            json_keys(&json),
            ["action", "exit_code", "outcome", "pane", "reason", "schema"]
        );
    }

    /// A refusal carries its reason token and exit 4; a locked refusal exits 5.
    #[test]
    fn act_json_refusal_carries_reason_and_exit_code() {
        use tma_core::RefusalReason;
        use tma_runtime::broker::Refusal;
        let gated = result(
            "approve",
            "%5",
            Outcome::Refused(Refusal::Gate(RefusalReason::Gated)),
        );
        let json = render_act_json(&gated);
        assert!(json.contains("\"outcome\":\"refused\""));
        assert!(json.contains("\"reason\":\"gated\""));
        assert!(json.contains("\"exit_code\":4"));
        let locked = result("approve", "%5", Outcome::Refused(Refusal::Locked));
        assert!(render_act_json(&locked).contains("\"exit_code\":5"));
        assert!(render_act_json(&locked).contains("\"reason\":\"locked\""));
    }

    /// A `sent` result reports its outcome and a null reason.
    #[test]
    fn act_json_sent_has_null_reason() {
        let json = render_act_json(&result("approve", "%5", Outcome::Sent));
        assert!(json.contains("\"outcome\":\"sent\""));
        assert!(json.contains("\"reason\":null"));
        assert!(json.contains("\"exit_code\":0"));
    }

    /// The closed `outcome` value set: every JSON-emittable token, pinned so a new or renamed
    /// outcome is caught. Kept in lockstep with the broker's own vocabulary test.
    #[test]
    fn outcome_value_set_is_closed() {
        use tma_core::RefusalReason;
        use tma_runtime::broker::Refusal;
        let mut tokens: Vec<&str> = [
            Outcome::Sent,
            Outcome::Replied,
            Outcome::Exited(0),
            Outcome::Spawned,
            Outcome::Timeout,
            Outcome::Refused(Refusal::Gate(RefusalReason::Gated)),
            Outcome::Vanished,
            Outcome::Error(String::new()),
        ]
        .iter()
        .map(Outcome::token)
        .collect();
        tokens.sort();
        assert_eq!(
            tokens,
            ["error", "exited", "refused", "replied", "sent", "spawned", "timeout", "vanished"]
        );
    }

    /// The `--all` envelope adds exactly `schema` + `results` around the pinned per-pane key set:
    /// one element shape, whether a script read a single fire or a fan-out.
    #[test]
    fn act_batch_json_wraps_the_pinned_result_objects() {
        use tma_core::RefusalReason;
        use tma_runtime::broker::Refusal;
        let results = [
            result("approve", "%1", Outcome::Sent),
            result(
                "approve",
                "%2",
                Outcome::Refused(Refusal::Gate(RefusalReason::Gated)),
            ),
        ];
        let json = render_act_batch_json(&results);
        assert!(json.starts_with("{\"schema\":1,\"results\":["));
        assert_eq!(
            json_keys(&json),
            [
                "action",
                "exit_code",
                "outcome",
                "pane",
                "reason",
                "results",
                "schema"
            ]
        );
        assert!(json.contains("\"pane\":\"%1\"") && json.contains("\"pane\":\"%2\""));
        assert!(json.contains("\"reason\":\"gated\"") && json.contains("\"reason\":null"));
    }

    /// A fan-out reports the WORST result's exit code, so a batch is a success only if every target
    /// acted. The ordering is the broker's severity ladder, not the numeric codes.
    #[test]
    fn batch_exit_code_is_the_worst_result() {
        use tma_core::RefusalReason;
        use tma_runtime::broker::Refusal;
        let sent = result("approve", "%1", Outcome::Sent);
        let locked = result("approve", "%2", Outcome::Refused(Refusal::Locked));
        let gated = result(
            "approve",
            "%3",
            Outcome::Refused(Refusal::Gate(RefusalReason::Gated)),
        );
        let vanished = result("approve", "%4", Outcome::Vanished);
        let errored = result("approve", "%5", Outcome::Error("boom".to_string()));
        let worst = |rs: &[ActResult]| {
            rs.iter()
                .max_by_key(|r| r.severity())
                .map(|r| r.exit_code())
                .unwrap()
        };
        assert_eq!(worst(&[sent.clone(), sent.clone()]), 0);
        assert_eq!(worst(&[sent.clone(), locked.clone()]), 5);
        assert_eq!(worst(&[locked.clone(), gated.clone()]), 4);
        assert_eq!(worst(&[gated.clone(), vanished.clone()]), 3);
        assert_eq!(worst(&[vanished, errored.clone(), sent]), 1);
        assert_eq!(worst(&[errored]), 1);
    }

    /// The `--all --dry-run` report names every target and its verdict, and fires nothing.
    #[test]
    fn dry_run_target_list_reports_each_verdict() {
        use tma_core::RefusalReason;
        use tma_runtime::broker::{DryGate, DryRun, Effect, Refusal};
        let run = |pane: &str, agent: Option<&str>, gate| DryRun {
            action: "approve".to_string(),
            pane: pane.to_string(),
            agent: agent.map(String::from),
            context: Vec::new(),
            gate,
            effect: Effect::None,
        };
        let text = render_dry_run_targets(&[
            run("%1", Some("claude"), DryGate::Fireable),
            run(
                "%2",
                Some("claude"),
                DryGate::Refused(Refusal::Gate(RefusalReason::Gated)),
            ),
            run("%3", Some("codex"), DryGate::Refused(Refusal::Locked)),
            run("%4", None, DryGate::Vanished),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "targets: 4");
        assert!(lines[1].contains("%1") && lines[1].contains("would fire"));
        assert!(lines[2].contains("refused: gated"));
        assert!(lines[3].contains("refused: locked"));
        assert!(lines[4].contains("vanished"));
    }

    /// `--list --json` (with `--pane` verdicts) carries exactly the documented key set.
    #[test]
    fn list_json_pins_full_key_set() {
        let approve = keys_action(
            "approve",
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let plain = keys_action("interrupt", "", "claude = [\"Escape\"]");
        let actions = [approve, plain];
        let verdicts: Vec<broker::ListVerdict> = vec![None, None];
        let json = render_list_json(&actions, Some(&verdicts));
        assert!(json.starts_with("{\"schema\":1"));
        assert_eq!(
            json_keys(&json),
            [
                "actions",
                "agents",
                "context_pct_max",
                "context_pct_min",
                "detail",
                "fireable",
                "kind",
                "label",
                "name",
                "reason",
                "schema",
                "state",
                "when",
            ]
        );
    }

    /// Without `--pane` the list omits the per-pane verdict keys (`fireable`/`reason`).
    #[test]
    fn list_json_without_pane_omits_verdict_keys() {
        let actions = [keys_action("approve", "", "claude = [\"1\"]")];
        let json = render_list_json(&actions, None);
        assert!(!json.contains("fireable"));
        assert!(!json.contains("\"reason\""));
        assert!(json.contains("\"when\":null"));
    }

    /// The sorted, de-duplicated object keys of a JSON document (a quoted string whose next
    /// non-space char is `:`). Mirrors the surfaces/doctor drift-test helper.
    fn json_keys(json: &str) -> Vec<String> {
        let b = json.as_bytes();
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut k = start;
                while k < b.len() {
                    match b[k] {
                        b'\\' => k += 2,
                        b'"' => break,
                        _ => k += 1,
                    }
                }
                let mut n = k + 1;
                while n < b.len() && b[n].is_ascii_whitespace() {
                    n += 1;
                }
                if n < b.len() && b[n] == b':' {
                    out.insert(json[start..k].to_string());
                }
                i = k + 1;
            } else {
                i += 1;
            }
        }
        out.into_iter().collect()
    }
}
