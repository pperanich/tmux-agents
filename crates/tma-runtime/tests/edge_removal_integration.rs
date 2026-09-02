//! Acceptance: the on-demand tier clears a quit agent's stamp on the pane's next activity edge,
//! and leaves a pid-less registration alone, on an isolated scratch tmux server.
//!
//! Both cases drive [`CaptureState::handle_edges`] directly (no daemon, no sweep), so what they
//! observe is the edge path and nothing else. The agent is a REAL foreground child of the pane's
//! shell, killed to reproduce the exit the daemon has to notice: the pane and its stale stamp
//! outlive it, which is what a status line rendering `#{@agent_summary}` was showing for a whole
//! sweep cadence.

use std::time::{SystemTime, UNIX_EPOCH};

use tma_core::FoldConfig;
use tma_runtime::capture::CaptureState;
use tma_runtime::manifests::{self, LoadedManifest};
use tma_tmux::control::ActivityEdge;
use tma_tmux::tmux::{ps_all, Tmux};

use common::Scratch;
use tma_test_support as common;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn client(s: &Scratch) -> Tmux {
    Tmux::new(Some(s.socket.clone()))
}

/// The manifests in the scratch workdir as the closed test set.
fn manifests(s: &Scratch) -> Vec<LoadedManifest> {
    manifests::load(Some(&s.workdir), &[])
        .expect("load test manifest")
        .manifests
}

/// One activity edge for `pane`, the shape the control-mode reader hands the daemon.
fn edges(pane: &str) -> Vec<ActivityEdge> {
    vec![ActivityEdge {
        pane: pane.to_string(),
        at: now_ms(),
    }]
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// The pane shell's child process, from the same `ps` walk identity uses. `None` once it exits,
/// which is how these tests wait out a kill without reading `#{pane_current_command}` (that one
/// reports the multi-call binary under uutils).
fn child_of(pane_pid: u32) -> Option<(u32, String)> {
    ps_all()
        .expect("ps")
        .into_iter()
        .find(|p| p.ppid == pane_pid)
        .map(|p| (p.pid, basename(&p.comm)))
}

/// A manifest matching the pane's live child: both its `ps` comm and `#{pane_current_command}` (they
/// differ under uutils), so `foreground_is_agent` holds while it runs. `[hooks] covers` makes the
/// pane hook-capable, which is what leaves demotion memory behind for the removal to drop.
fn write_agent_manifest(s: &Scratch, pane: &str, comm: &str) {
    let cur = basename(&s.get(pane, "#{pane_current_command}"));
    let mut names = vec![cur, comm.to_string()];
    names.sort();
    names.dedup();
    let names = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        s.workdir.join("edgeagent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names}]\n\
             [hooks]\ncovers = [\"working\", \"idle\", \"blocked\"]\n\
             [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n"
        ),
    )
    .unwrap();
}

/// Every `@agent_*` / `@tma_*` option still set ON the pane (`show-options -p` is pane scope only,
/// so the window and session rollups are read separately).
fn pane_agent_options(s: &Scratch, pane: &str) -> Vec<String> {
    let out = s.tmux(&["show-options", "-p", "-t", pane]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("@agent_") || l.starts_with("@tma_"))
        .map(str::to_string)
        .collect()
}

/// Split `anchor`'s window (or open a new window in its session) and park a pane there carrying a
/// stored `@agent_state`, so the two rollups have something to recompute FROM and differ from each
/// other. `-P -F` prints the new pane's id.
fn parked_agent(s: &Scratch, args: &[&str], state: &str) -> String {
    let out = s.tmux(args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(pane.starts_with('%'), "got pane {pane:?}");
    s.set_opt(&pane, "@agent_state", state);
    pane
}

/// The bug: an agent quits, the pane falls back to its shell, and the stamp plus both rollups sit
/// there until the next reconciliation sweep (36 s, measured, with the default 45 s cadence). The
/// pane's own next activity edge is the daemon's first look at it after the exit, so that is where
/// the removal belongs.
#[test]
fn a_quit_agent_loses_its_stamp_on_the_next_edge() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("edge_removal");
    let pane = s.new_shell_pane();
    let session = s.get(&pane, "#{session_name}");
    let pane_pid: u32 = s.get(&pane, "#{pane_pid}").parse().unwrap();

    // A second agent pane in the SAME window and a third in another window of the same session:
    // the window and session rollups then differ, so each assertion below names its own recompute.
    let _sibling = parked_agent(
        &s,
        &["split-window", "-d", "-P", "-F", "#{pane_id}", "-t", &pane],
        "working",
    );
    // `<session>:` (with the colon) targets the session, not a window index: the scratch session
    // is named `0`, which a bare name would resolve as window 0.
    let session_target = format!("{session}:");
    let _elsewhere = parked_agent(
        &s,
        &[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &session_target,
        ],
        "blocked",
    );

    // The agent: a real foreground child of the pane's shell.
    s.tmux(&["send-keys", "-t", &pane, "sleep 600", "Enter"]);
    common::poll_until("the agent child to start", || child_of(pane_pid).is_some());
    let (agent_pid, comm) = child_of(pane_pid).expect("the agent child");
    write_agent_manifest(&s, &pane, &comm);

    let tmux = client(&s);
    let manifests = manifests(&s);
    // `demote_edges = 1`: the pane is hook-capable (its manifest declares `[hooks] covers`), so one
    // edge both captures it and leaves the hook-liveness memory the removal has to drop.
    let mut capture = CaptureState::new(FoldConfig::default(), 1);
    capture
        .handle_edges(&tmux, &manifests, edges(&pane))
        .expect("the live agent's edge");
    assert!(
        !s.pane_option(&pane, "@agent_state").is_empty(),
        "the first edge must stamp the live agent"
    );
    assert!(
        capture.status_lines().contains("demoted=1"),
        "the hook-capable pane is tracked (demoted) after its edge: {}",
        capture.status_lines()
    );

    // The lanes a real agent pane accumulates beyond the state tuple, plus both anchors: every one
    // of them is in `REMOVABLE` and must go with the stamp.
    for (key, value) in [
        ("@agent_attention", "1".to_string()),
        ("@agent_context_pct", "42".to_string()),
        ("@agent_context_at", now_ms().to_string()),
        ("@agent_model", "sonnet".to_string()),
        ("@agent_cost_usd", "1.25".to_string()),
        ("@tma_title_match_pid", agent_pid.to_string()),
        ("@tma_reg_dead_since", now_ms().to_string()),
    ] {
        s.set_opt(&pane, key, &value);
    }
    // Bogus rollups, in the shape the status line was stuck showing, so "recomputed" cannot pass by
    // coincidence with "never written".
    s.tmux(&["set-option", "-w", "-t", &pane, "@agent_summary", "idle:9"]);
    s.tmux(&[
        "set-option",
        "-t",
        &pane,
        "@agent_session_summary",
        "idle:9",
    ]);

    // The quit: the agent dies, the pane and its stamp do not.
    let victim = rustix::process::Pid::from_raw(agent_pid as i32).expect("agent pid > 0");
    assert!(
        rustix::process::kill_process(victim, rustix::process::Signal::KILL).is_ok(),
        "SIGKILL {agent_pid}"
    );
    common::poll_until("the pane to fall back to its shell", || {
        child_of(pane_pid).is_none()
    });

    capture
        .handle_edges(&tmux, &manifests, edges(&pane))
        .expect("the quiet edge after the exit");

    let left = pane_agent_options(&s, &pane);
    assert!(
        left.is_empty(),
        "the quit agent's pane must keep no tma option at all, found {left:?}"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_summary}"),
        "working:1",
        "the window rollup is recomputed in the same invocation (the surviving sibling only)"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_session_summary}"),
        "blocked:1 working:1",
        "and so is the session rollup, over the other window's pane too"
    );
    assert!(
        capture.status_lines().contains("demoted=0"),
        "the pane's hook-liveness memory is dropped with its stamp: {}",
        capture.status_lines()
    );
}

/// The carve-out the removal must not swallow: a pane holding a hook registration whose agent has
/// no walkable process (`agent_pid == 0`) is NOT an exit the edge path may act on. A live agent the
/// `ps` walk momentarily misses looks exactly like this, so the liveness call stays with the poll
/// cycle's 30 s dead-registration reaper, which runs in the sweep.
#[test]
fn a_pid_less_registration_survives_its_edge() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("edge_regdead");
    std::fs::write(
        s.workdir.join("ghostagent.toml"),
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [\"tma-no-such-agent-proc\"]\n\
         [capture]\nvisible = [\"working\"]\n",
    )
    .unwrap();
    // A shell-only pane: nothing in its subtree answers to the manifest, so identity resolves
    // through the registration alone.
    let pane = s.new_shell_pane();
    s.set_opt(&pane, "@agent_name", "ghostagent");
    s.set_opt(&pane, "@agent_session", "sess-1");
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_pid", "0");
    s.set_opt(&pane, "@agent_stamped_at", &(now_ms() - 60_000).to_string());
    // Aged past the reaper threshold: even then the edge path holds, because the reap is a decision
    // the sweep owns and re-derives from the subtree.
    s.set_opt(
        &pane,
        "@tma_reg_dead_since",
        &(now_ms() - 40_000).to_string(),
    );

    let mut capture = CaptureState::new(FoldConfig::default(), 1);
    capture
        .handle_edges(&client(&s), &manifests(&s), edges(&pane))
        .expect("edge");

    assert_eq!(
        s.pane_option(&pane, "@agent_state"),
        "working",
        "a pid-less registration is held by the edge path, not deregistered"
    );
    assert!(
        !s.pane_option(&pane, "@tma_reg_dead_since").is_empty(),
        "and the reaper's marker is left standing for the sweep that owns it"
    );
}
