use tma_runtime::ipc;

use super::{
    daemon_version_matches, tier_reason_str, tmux_below_min, Report, COMM_MAX, MIN_TMUX_VERSION_STR,
};
use crate::install::{HookWiring, TmuxHookState};
use crate::json::{JsonWriter, JSON_SCHEMA};

// --- rendering -------------------------------------------------------------------

/// Human-friendly age: sub-minute in seconds (one decimal), else whole minutes/hours.
fn fmt_age(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{}m", (secs / 60.0) as u64)
    } else {
        format!("{}h", (secs / 3600.0) as u64)
    }
}

fn hook_summary(w: &HookWiring) -> String {
    match w {
        HookWiring::Wired => "wired".to_string(),
        HookWiring::Incomplete(reasons) => format!("incomplete ({})", reasons.join("; ")),
        HookWiring::NotInstalled => "not installed".to_string(),
        HookWiring::Hookless => "hookless (screen-detection only)".to_string(),
        HookWiring::NoAdapter => "no installer adapter (wire by hand)".to_string(),
    }
}

/// Stable token for the wiring category, for the `--json` `hook_status` field.
fn hook_status_token(w: &HookWiring) -> &'static str {
    match w {
        HookWiring::Wired => "wired",
        HookWiring::Incomplete(_) => "incomplete",
        HookWiring::NotInstalled => "not_installed",
        HookWiring::Hookless => "hookless",
        HookWiring::NoAdapter => "no_adapter",
    }
}

pub(super) fn render_text(r: &Report) -> String {
    let mut out = String::new();

    // The server's build, printed only when it is behind what tma is tested on: an old tmux
    // misbehaves in ways (config load order, popup expansion) the rest of the report cannot explain.
    if tmux_below_min(r.tmux_version.as_deref()) {
        let found = r.tmux_version.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "tmux:    tma is tested on tmux {MIN_TMUX_VERSION_STR}+; {found} may mis-load configs \
             or keybindings\n"
        ));
    }

    // Daemon.
    if !r.daemon_known {
        out.push_str("daemon:  unknown (server unreachable)\n");
    } else if r.daemon_alive {
        out.push_str(&format!(
            "daemon:  running ({})\n",
            r.daemon_socket.display()
        ));
        // A resident daemon keeps the manifests and code it started with, so a version skew is why
        // a freshly-installed agent or event mapping is not taking effect.
        if daemon_version_matches(r.daemon_version.as_deref()) == Some(false) {
            let running = r.daemon_version.as_deref().unwrap_or("unknown");
            out.push_str(&format!(
                "         version {running} differs from this CLI ({}) — `tma reload` only re-reads \
                 config and manifests; stop the daemon and run `tma daemon --ensure` to pick up \
                 the new build\n",
                ipc::VERSION
            ));
        }
    } else {
        out.push_str(&format!(
            "daemon:  not running ({}) — tier 3 needs a running daemon (`tma daemon --ensure`)\n",
            r.daemon_socket.display()
        ));
    }

    // Ambient driver.
    match r.ambient_poll_age_ms {
        Some(age) => out.push_str(&format!(
            "ambient: polling — `tma status` last ran {} ago\n",
            fmt_age(age)
        )),
        None => out.push_str(
            "ambient: NOT polling — nothing invokes `tma status`; add `#(tma status)` to \
             status-right (required ambient driver)\n",
        ),
    }

    // What the ambient driver needs from the server itself. A detached server is only a warning
    // when nothing else keeps state fresh: with a daemon running the floor does not matter.
    if r.attached_clients == 0 {
        if r.daemon_alive {
            out.push_str(
                "clients: none attached — `#()` status jobs do not run detached; the daemon is \
                 keeping state fresh meanwhile\n",
            );
        } else {
            out.push_str(
                "clients: none attached — `#()` status jobs only run while a client draws the \
                 status line, so nothing polls this server (run the daemon or attach a client)\n",
            );
        }
    } else {
        out.push_str(&format!("clients: {} attached\n", r.attached_clients));
    }
    if !r.status_enabled {
        out.push_str(
            "status:  the global `status` option is off — the `#(tma status)` driver never runs \
             and `display-message` notifications are invisible (`tmux set -g status on`)\n",
        );
    }

    // The clickable status segments, reported only when they are wired to a server that cannot
    // deliver a click: the bindings are installed but nothing will ever fire them.
    if r.mouse_bindings && !r.mouse_enabled {
        out.push_str(
            "mouse:   the clickable status bindings are installed but the global `mouse` option is \
             off, so no click reaches them (`tmux set -g mouse on`; it also changes selection and \
             copy/paste in every pane, which is why tma does not set it)\n",
        );
    }

    // The notify sink: reported only when the last fire's command failed, since a working (or
    // unconfigured) one has nothing to say.
    if let Some(f) = &r.notify_failure {
        out.push_str(&format!(
            "notify:  the notify command failed {} ago ({}): {}\n         \
             re-run it with `tma debug notify-test` to see its output\n",
            fmt_age(r.notify_failure_age_ms.unwrap_or(0)),
            f.reason,
            f.command,
        ));
    }

    // Middle-tier watcher nudge: resident `tma watch` panes signalled on focus change.
    match r.watch_panes {
        0 => out
            .push_str("watch:   no watcher running (`tma watch` advertises for SIGUSR1 nudges)\n"),
        1 => out.push_str("watch:   1 watcher running (nudged on focus change)\n"),
        n => out.push_str(&format!(
            "watch:   {n} watchers running (nudged on focus change)\n"
        )),
    }

    // tmux hooks + wrapper.
    if r.tmux_hooks.is_empty() {
        out.push_str("hooks:   tmux server hooks unknown\n");
    } else {
        let parts: Vec<String> = r
            .tmux_hooks
            .iter()
            .map(|(h, st)| match st {
                TmuxHookState::Present => format!("{h} \u{2713}"),
                other => format!("{h} \u{2717} {}", other.token()),
            })
            .collect();
        out.push_str(&format!("hooks:   {}\n", parts.join("  ")));
        // The distinct states carry a next step the ✗ alone does not (a restart-wiped array is
        // reinstalled or made durable; a stale entry is repointed).
        for (hook, st) in &r.tmux_hooks {
            if let Some(reason) = st.reason(hook) {
                out.push_str(&format!("         {reason}\n"));
            }
        }
    }
    // Under a bare reference the file and the name the configs use are different facts, and only
    // the second one is what an agent resolves — so lead with it and keep the file as the aside.
    let bare = r.wrapper_ref != r.wrapper_path;
    out.push_str(&format!(
        "wrapper: {} {}\n",
        r.wrapper_ref.display(),
        match (r.wrapper_present, bare) {
            (true, false) => "\u{2713}".to_string(),
            (false, false) => "\u{2717} missing".to_string(),
            (true, true) => format!("\u{2713} on $PATH ({})", r.wrapper_path.display()),
            (false, true) => format!("\u{2717} not on $PATH ({})", r.wrapper_path.display()),
        }
    ));

    // Agent manifests: the loaded count plus any file the loader had to skip.
    if r.manifest_issues.is_empty() {
        out.push_str(&format!("agents:  {} loaded, no issues\n", r.manifest_ok));
    } else {
        out.push_str(&format!(
            "agents:  {} loaded, {} skipped:\n",
            r.manifest_ok,
            r.manifest_issues.len()
        ));
        for issue in &r.manifest_issues {
            out.push_str(&format!("  - {}: {}\n", issue.file, issue.problem));
        }
    }
    for lint in &r.process_name_issues {
        out.push_str(&format!(
            "  - {}: process_names entry {:?} is longer than {COMM_MAX} chars, the width both \
             macOS libproc and the Linux kernel truncate `comm` to, and no truncated spelling sits \
             beside it — add {:?}\n",
            lint.agent,
            lint.name,
            lint.name.chars().take(COMM_MAX).collect::<String>()
        ));
    }

    // Actions: loaded count plus any load errors / dangling agent references.
    if r.action_issues.is_empty() {
        out.push_str(&format!("actions: {} loaded, no issues\n", r.action_ok));
    } else {
        out.push_str(&format!(
            "actions: {} loaded, {} issue(s):\n",
            r.action_ok,
            r.action_issues.len()
        ));
        for issue in &r.action_issues {
            out.push_str(&format!("  - {}: {}\n", issue.file, issue.problem));
        }
    }

    // Nested multiplexers: panes whose agents belong to an inner server this tma cannot reach.
    // Printed only when there are any — an ordinary server has none and needs no line about it.
    if !r.nested.is_empty() {
        out.push_str(&format!(
            "nested:  {} pane(s) running a multiplexer client — agent state lives on the inner \
             server; run tma there\n",
            r.nested.len()
        ));
        for n in &r.nested {
            out.push_str(&format!("  - {} {} ({})\n", n.pane, n.locator, n.command));
        }
    }

    // Panes behind a remote shell: tma sees neither the processes nor the screen on the far side,
    // so an agent there is invisible unless its hooks can reach this socket. Any stamp such a pane
    // still carries is held, not live, which is exactly what makes it worth naming.
    if !r.remote.is_empty() {
        out.push_str(&format!(
            "remote:  {} pane(s) behind a remote shell — an agent there reports only if it can \
             reach this tmux socket (see docs/how-to/agents-in-containers.md)\n",
            r.remote.len()
        ));
        for p in &r.remote {
            let held = if p.stamped {
                "; its @agent_* options are held, not refreshed"
            } else {
                ""
            };
            out.push_str(&format!(
                "  - {} {} ({}{held})\n",
                p.pane, p.locator, p.command
            ));
        }
    }

    // Panes the user opted out of detection. One line each, because the option is invisible
    // otherwise: nothing else distinguishes an ignored pane from one no manifest matched.
    if !r.ignored.is_empty() {
        out.push_str(&format!(
            "ignored: {} pane(s) excluded from detection — unset the option to bring one back \
             (`tmux set-option -pu -t <pane> @agent_ignore`)\n",
            r.ignored.len()
        ));
        for p in &r.ignored {
            out.push_str(&format!(
                "  - {} {} (ignored via @agent_ignore = {})\n",
                p.pane, p.locator, p.value
            ));
        }
    }

    // Corrupt stamps: every reader treats one as no stamp at all, so without this line the pane
    // simply reads as never-stamped and no surface ever says why.
    if !r.stamp_issues.is_empty() {
        out.push_str(&format!(
            "stamps:  {} pane(s) with an undecodable @agent_* option — they read as never-stamped; \
             clear the option or let the next poll rewrite it\n",
            r.stamp_issues.len()
        ));
        for s in &r.stamp_issues {
            out.push_str(&format!("  - {} {}: {}\n", s.pane, s.locator, s.problem));
        }
    }

    // A `ps` that will not run: every pane row below it is missing or thinner than it should be,
    // so the reason comes first rather than leaving the panes section to look like an empty server.
    if let Some(err) = &r.process_walk_error {
        out.push_str(&format!(
            "procs:   the process walk failed ({err}) — detection cannot see what runs in a pane; \
             only panes a hook registered are listed below (check that `ps` is on PATH)\n"
        ));
    }

    // Agent panes. Headed `panes` rather than `agents`, which above names the manifest roster.
    out.push('\n');
    if r.agents.is_empty() {
        out.push_str("panes:   no agent panes detected on this server\n");
        return out;
    }
    out.push_str(&format!("panes ({}):\n", r.agents.len()));
    for a in &r.agents {
        let state = match (a.state, a.source, a.evidence_age_ms) {
            (Some(st), Some(src), Some(age)) => {
                format!("{} ({}, {} ago)", st.token(), src.token(), fmt_age(age))
            }
            (Some(st), Some(src), None) => format!("{} ({})", st.token(), src.token()),
            (Some(st), _, _) => st.token().to_string(),
            _ => "unstamped".to_string(),
        };
        out.push_str(&format!(
            "  {:<4} {:<10} {:<12} tier {}   {}\n",
            a.pane, a.agent, a.locator, a.tier.level, state
        ));
        out.push_str(&format!("       hooks: {}\n", hook_summary(&a.wiring)));
        if a.hook_demoted {
            out.push_str(
                "       demoted: this pane registered through a hook but its current state came \
                 from capture — its hooks have stopped firing (agent restarted without the \
                 wiring, or the wrapper is gone)\n",
            );
        }
        if let Some(model) = &a.model {
            match a.window_covered {
                Some(false) => out.push_str(&format!(
                    "       model: {model} — unrecognized; no [telemetry.windows] entry names it\n"
                )),
                _ => out.push_str(&format!("       model: {model}\n")),
            }
        }
        if a.endpoint_ok == Some(false) {
            out.push_str(
                "       api: pending permission request but no reachable endpoint \
                 (stamp `@agent_api_endpoint` or set `[api.opencode] api_base`)\n",
            );
        }
        if let Some(reason) = tier_reason_str(a.tier, &a.agent, r.daemon_alive) {
            out.push_str(&format!(
                "       not tier {}: {}\n",
                a.tier.level + 1,
                reason
            ));
        }
    }
    out
}

/// `tma doctor --json`: a versioned, additive-only document (`"schema": 1`), matching
/// `tma ls --json`'s convention.
pub(super) fn render_json(r: &Report) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);

    // The server's build against the floor tma is tested on. `below_min` is a report field, not a
    // gate: an old tmux is a machine fact, and failing CI on it would help nobody.
    j.key("tmux");
    j.begin_object();
    match &r.tmux_version {
        Some(v) => j.string("version", v),
        None => j.null("version"),
    }
    j.string("min_version", MIN_TMUX_VERSION_STR);
    j.bool("below_min", tmux_below_min(r.tmux_version.as_deref()));
    j.end_object();

    j.key("daemon");
    j.begin_object();
    j.bool("alive", r.daemon_alive);
    if r.daemon_known {
        j.string("socket", &r.daemon_socket.display().to_string());
    } else {
        j.null("socket");
    }
    match &r.daemon_version {
        Some(v) => j.string("version", v),
        None => j.null("version"),
    }
    match daemon_version_matches(r.daemon_version.as_deref()) {
        Some(ok) => j.bool("version_matches", ok),
        None => j.null("version_matches"),
    }
    j.end_object();

    j.key("ambient_driver");
    j.begin_object();
    j.bool("polling", r.ambient_poll_age_ms.is_some());
    match r.ambient_poll_age_ms {
        Some(age) => j.number("last_poll_age_ms", age as i64),
        None => j.null("last_poll_age_ms"),
    }
    j.end_object();

    // The server-side prerequisites of the ambient driver: an attached client to run the `#()`
    // status jobs, and `status` left on so they (and `display-message`) happen at all.
    j.key("clients");
    j.begin_object();
    j.number("attached", r.attached_clients as i64);
    j.end_object();

    j.key("status_option");
    j.begin_object();
    j.bool("enabled", r.status_enabled);
    j.end_object();

    // The clickable status segments: the two halves that only work together.
    j.key("mouse");
    j.begin_object();
    j.bool("bindings_installed", r.mouse_bindings);
    j.bool("enabled", r.mouse_enabled);
    j.end_object();

    // Middle-tier watcher nudge: resident `tma watch` panes advertising `@tma_watch_pid`.
    j.key("watch");
    j.begin_object();
    j.bool("running", r.watch_panes > 0);
    j.number("watchers", r.watch_panes as i64);
    j.end_object();

    j.key("wrapper");
    j.begin_object();
    j.string("path", &r.wrapper_path.display().to_string());
    // What the agent configs actually name; equal to `path` unless `wrapper_ref = "bare"`.
    j.string("reference", &r.wrapper_ref.display().to_string());
    j.bool("present", r.wrapper_present);
    j.end_object();

    // The notify sink's last recorded failure. Null when nothing has failed since the last clean
    // fire, so a consumer's key lookup does not depend on there being a problem.
    j.key("notify");
    j.begin_object();
    match &r.notify_failure {
        Some(f) => {
            j.key("last_failure");
            j.begin_object();
            j.number("at", f.at as i64);
            j.string("reason", &f.reason);
            j.string("command", &f.command);
            j.end_object();
        }
        None => j.null("last_failure"),
    }
    j.end_object();

    // The `ps` walk. `ok: false` means the pane list below carries only hook-registered agents.
    j.key("process_walk");
    j.begin_object();
    j.bool("ok", r.process_walk_error.is_none());
    match &r.process_walk_error {
        Some(err) => j.string("error", err),
        None => j.null("error"),
    }
    j.end_object();

    j.key("tmux_hooks");
    j.begin_array();
    for (hook, state) in &r.tmux_hooks {
        j.begin_object();
        j.string("hook", hook);
        j.bool("present", state.is_present());
        j.string("hook_state", state.token());
        j.end_object();
    }
    j.end_array();

    // Agent manifests: the effective roster plus the files the loader skipped.
    j.key("manifests");
    j.begin_object();
    j.number("ok", r.manifest_ok as i64);
    j.key("issues");
    j.begin_array();
    for issue in &r.manifest_issues {
        j.begin_object();
        j.string("file", &issue.file);
        j.string("problem", &issue.problem);
        j.end_object();
    }
    j.end_array();
    j.end_object();

    // Manifest identity that cannot match: a `process_names` entry past the comm truncation width.
    j.key("process_name_issues");
    j.begin_array();
    for lint in &r.process_name_issues {
        j.begin_object();
        j.string("agent", &lint.agent);
        j.string("name", &lint.name);
        j.number("comm_max", COMM_MAX as i64);
        j.end_object();
    }
    j.end_array();

    // Panes running a nested multiplexer client: named so a "why is my agent missing" search ends
    // here rather than in the absence of a row.
    j.key("nested_multiplexers");
    j.begin_array();
    for n in &r.nested {
        j.begin_object();
        j.string("pane", &n.pane);
        j.string("locator", &n.locator);
        j.string("command", n.command);
        j.end_object();
    }
    j.end_array();

    // Panes behind a remote shell, with whether the pane still carries a held stamp: the machine
    // form of "this pane's agent, if any, lives where tma cannot look".
    j.key("remote_panes");
    j.begin_array();
    for p in &r.remote {
        j.begin_object();
        j.string("pane", &p.pane);
        j.string("locator", &p.locator);
        j.string("command", p.command);
        j.bool("stamped", p.stamped);
        j.end_object();
    }
    j.end_array();

    // Panes the user took out of detection, with the value they set: the machine form of "this
    // pane is silent on purpose".
    j.key("ignored_panes");
    j.begin_array();
    for p in &r.ignored {
        j.begin_object();
        j.string("pane", &p.pane);
        j.string("locator", &p.locator);
        j.string("value", &p.value);
        j.end_object();
    }
    j.end_array();

    // Panes carrying an `@agent_*` option that does not decode: they read as never-stamped
    // everywhere else, so this is the only machine-readable record of the bad value.
    j.key("stamp_issues");
    j.begin_array();
    for s in &r.stamp_issues {
        j.begin_object();
        j.string("pane", &s.pane);
        j.string("locator", &s.locator);
        j.string("problem", &s.problem);
        j.end_object();
    }
    j.end_array();

    j.key("agents");
    j.begin_array();
    for a in &r.agents {
        j.begin_object();
        j.string("pane", &a.pane);
        j.string("agent", &a.agent);
        j.string("locator", &a.locator);
        match a.state {
            Some(st) => j.string("state", st.token()),
            None => j.null("state"),
        }
        match a.source {
            Some(src) => j.string("source", src.token()),
            None => j.null("source"),
        }
        match a.evidence_age_ms {
            Some(age) => j.number("evidence_age_ms", age as i64),
            None => j.null("evidence_age_ms"),
        }
        j.string("hook_status", hook_status_token(&a.wiring));
        j.bool("hooks_wired", matches!(a.wiring, HookWiring::Wired));
        match &a.model {
            Some(model) => j.string("model", model),
            None => j.null("model"),
        }
        match a.window_covered {
            Some(covered) => j.bool("window_covered", covered),
            None => j.null("window_covered"),
        }
        match a.endpoint_ok {
            Some(ok) => j.bool("endpoint_ok", ok),
            None => j.null("endpoint_ok"),
        }
        j.bool("hook_demoted", a.hook_demoted);
        j.number("tier", a.tier.level as i64);
        match tier_reason_str(a.tier, &a.agent, r.daemon_alive) {
            Some(reason) => j.string("tier_reason", &reason),
            None => j.null("tier_reason"),
        }
        j.end_object();
    }
    j.end_array();

    // Actions: loaded count + issues (load errors and dangling agent references).
    j.key("actions");
    j.begin_object();
    j.number("ok", r.action_ok as i64);
    j.key("issues");
    j.begin_array();
    for issue in &r.action_issues {
        j.begin_object();
        j.string("file", &issue.file);
        j.string("problem", &issue.problem);
        j.end_object();
    }
    j.end_array();
    j.end_object();

    j.end_object();
    j.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::tests::sample_report;

    #[test]
    fn a_hook_registered_pane_running_on_capture_is_reported_as_demoted() {
        let mut report = sample_report();
        let text = render_text(&report);
        assert!(
            text.contains("demoted:") && text.contains("stopped firing"),
            "the demotion is named per pane: {text}"
        );
        report.agents[0].hook_demoted = false;
        assert!(!render_text(&report).contains("demoted:"));
    }

    /// A pane behind ssh is named with what it would take to see an agent there, and its held
    /// stamp is called held: before this the pane showed frozen state with no explanation at all.
    #[test]
    fn a_remote_pane_is_named_with_the_socket_condition_and_its_held_stamp() {
        let mut report = sample_report();
        let text = render_text(&report);
        assert!(
            text.contains("%10 s:2.1 (ssh") && text.contains("reach this tmux socket"),
            "the remote pane names its foreground and the condition: {text}"
        );
        assert!(
            text.contains("agents-in-containers.md") && text.contains("held, not refreshed"),
            "the recipe and the held stamp are both named: {text}"
        );
        report.remote[0].stamped = false;
        assert!(!render_text(&report).contains("held, not refreshed"));
        report.remote.clear();
        assert!(!render_text(&report).contains("remote:"));
    }

    #[test]
    fn a_detached_server_warns_only_while_no_daemon_covers_it() {
        let mut report = sample_report();
        report.attached_clients = 0;
        report.daemon_alive = false;
        assert!(
            render_text(&report).contains("nothing polls this server"),
            "no client and no daemon ⇒ the floor is dead"
        );
        // A running daemon keeps state fresh, so the same fact is reported without the warning.
        report.daemon_alive = true;
        let text = render_text(&report);
        assert!(text.contains("none attached") && !text.contains("nothing polls this server"));
        report.attached_clients = 2;
        assert!(render_text(&report).contains("clients: 2 attached"));
    }

    #[test]
    fn a_stale_or_wiped_tmux_hook_carries_its_next_step() {
        let with = |state| {
            let mut r = sample_report();
            r.tmux_hooks = vec![("after-select-pane".to_string(), state)];
            render_text(&r)
        };
        assert!(!with(TmuxHookState::Present).contains("after-select-pane \u{2717}"));
        let drifted = with(TmuxHookState::Drifted);
        assert!(
            drifted.contains("after-select-pane \u{2717} drifted") && drifted.contains("stale")
        );
        assert!(with(TmuxHookState::Wiped).contains("likely restarted"));
        assert!(with(TmuxHookState::Missing).contains("missing"));
    }

    /// A recorded notify failure is reported with its reason and the command to re-run, and vanishes
    /// once the marker is gone (a clean fire cleared it).
    #[test]
    fn a_failed_notify_command_is_reported_with_its_next_step() {
        let mut report = sample_report();
        let text = render_text(&report);
        assert!(
            text.contains("notify command failed")
                && text.contains("exited 127")
                && text.contains("tma debug notify-test"),
            "the failure names its reason and the next step: {text}"
        );
        report.notify_failure = None;
        report.notify_failure_age_ms = None;
        assert!(!render_text(&report).contains("notify command failed"));
    }

    #[test]
    fn status_off_is_flagged_because_it_kills_both_channels() {
        let mut report = sample_report();
        let text = render_text(&report);
        assert!(
            text.contains("`status` option is off") && text.contains("display-message"),
            "both the driver and the notification channel are named: {text}"
        );
        report.status_enabled = true;
        assert!(!render_text(&report).contains("`status` option is off"));
    }

    /// The complete `doctor --json` key inventory (additive-only): a dropped or renamed key
    /// fails here. A new key is intentionally caught too — add it below in the same commit.
    #[test]
    fn doctor_json_pins_full_key_set() {
        let keys = json_test::keys(&render_json(&sample_report()));
        assert_eq!(
            keys,
            [
                "actions",
                "agent",
                "agents",
                "alive",
                "ambient_driver",
                "at",
                "attached",
                "below_min",
                "bindings_installed",
                "clients",
                "comm_max",
                "command",
                "daemon",
                "enabled",
                "endpoint_ok",
                "error",
                "evidence_age_ms",
                "file",
                "hook",
                "hook_demoted",
                "hook_state",
                "hook_status",
                "hooks_wired",
                "ignored_panes",
                "issues",
                "last_failure",
                "last_poll_age_ms",
                "locator",
                "manifests",
                "min_version",
                "model",
                "mouse",
                "name",
                "nested_multiplexers",
                "notify",
                "ok",
                "pane",
                "path",
                "polling",
                "present",
                "problem",
                "process_name_issues",
                "process_walk",
                "reason",
                "reference",
                "remote_panes",
                "running",
                "schema",
                "socket",
                "source",
                "stamp_issues",
                "stamped",
                "state",
                "status_option",
                "tier",
                "tier_reason",
                "tmux",
                "tmux_hooks",
                "value",
                "version",
                "version_matches",
                "watch",
                "watchers",
                "window_covered",
                "wrapper",
            ]
        );
    }
}

/// JSON object-key extraction shared by the `--json` key-inventory drift tests.
#[cfg(test)]
mod json_test {
    /// The sorted, de-duplicated object keys in a JSON document (a quoted string whose next
    /// non-space char is `:`). Byte-scans the ASCII structural bytes, covering nested/repeated keys.
    pub(super) fn keys(json: &str) -> Vec<String> {
        let b = json.as_bytes();
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < b.len() {
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < b.len() && b[k] == b':' {
                    out.insert(json[start..j].to_string());
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out.into_iter().collect()
    }

    #[test]
    fn extracts_nested_and_ignores_string_values_with_colons() {
        let keys = keys(r#"{"a":"x:y","b":{"c":1},"d":[{"e":null}]}"#);
        assert_eq!(keys, ["a", "b", "c", "d", "e"]);
    }
}
