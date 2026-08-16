//! Acceptance: the dead-registration reaper, on an isolated scratch tmux server.
//!
//! A hook-registered pane (`@agent_session` + `@agent_name`, no walkable process ⇒ `agent_pid == 0`)
//! whose agent died without a SessionEnd used to hold its stamp forever. The reaper clears a
//! truly-dead agent (shell-only subtree) but holds a live pid-less one (gemini: a non-shell process
//! despite matching no `process_names`). Driven through the real [`cycle::run_cycle`]; the marker is
//! manipulated directly to exercise the timed transition without a 30 s wait.

use std::time::{SystemTime, UNIX_EPOCH};

use tma_core::FoldConfig;
use tma_runtime::cycle;
use tma_runtime::manifests::{self, LoadedManifest};
use tma_tmux::tmux::Tmux;

use common::Scratch;
use tma_test_support as common;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A scratch server seeded with a ghost manifest whose `process_names` match nothing, so a
/// registered pane always takes the pid-less (`agent_pid == 0`) hold branch (the reaper's domain).
fn scratch() -> Scratch {
    let sx = Scratch::new("regdead");
    std::fs::write(
        sx.workdir.join("ghostagent.toml"),
        "min_engine_version = \"0.1\"\n\
         [identity]\n\
         process_names = [\"tma-no-such-agent-proc\"]\n\
         [capture]\n\
         visible = [\"working\"]\n",
    )
    .unwrap();
    sx
}

/// Load the ghost manifest as the closed test set (the reaper's domain of pid-less agents).
fn manifests(sx: &Scratch) -> Vec<LoadedManifest> {
    manifests::load(Some(&sx.workdir), &[])
        .expect("load test manifest")
        .manifests
}

/// A `Tmux` client bound to this scratch server's `-L` socket.
fn client(sx: &Scratch) -> Tmux {
    Tmux::new(Some(sx.socket.clone()))
}

/// Seed the registered half on `pane`: a stored `@agent_session` + `@agent_name` plus a STALE
/// stamp (so the consumer fast-path is skipped and the pane reaches the producer path).
fn register(sx: &Scratch, pane: &str) {
    sx.set_opt(pane, "@agent_name", "ghostagent");
    sx.set_opt(pane, "@agent_session", "sess-1");
    sx.set_opt(pane, "@agent_state", "working");
    sx.set_opt(pane, "@agent_pid", "0");
    // Stale by a minute (13-digit ms, above the legacy-seconds floor): never "fresh".
    sx.set_opt(pane, "@agent_stamped_at", &(now_ms() - 60_000).to_string());
}

#[test]
fn dead_registered_pane_is_reaped_once_shell_only_persists() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let sx = scratch();
    // A pane whose ONLY process is a shell — a registered agent that died back to its shell.
    // `/bin/sh` (not the developer's $SHELL) makes "shell-only" deterministic.
    let out = sx.tmux(&[
        "new-session",
        "-d",
        "-s",
        "w",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane = "w:0.0";
    register(&sx, pane);

    let tmux = client(&sx);
    let manifests = manifests(&sx);
    let cfg = FoldConfig::default();

    // Cycle 1: first shell-only observation ⇒ the marker is stamped, the stamp is HELD (not yet
    // reaped — still within the window).
    cycle::run_cycle(&tmux, &manifests, &cfg).expect("cycle 1");
    assert!(
        !sx.pane_option(pane, "@tma_reg_dead_since").is_empty(),
        "first shell-only cycle must stamp the reaper marker"
    );
    assert_eq!(
        sx.pane_option(pane, "@agent_state"),
        "working",
        "within the window the registration is still held, not reaped"
    );

    // Age the marker past the threshold (simulating ~40 s of continuous shell-only).
    sx.set_opt(
        pane,
        "@tma_reg_dead_since",
        &(now_ms() - 40_000).to_string(),
    );

    // Cycle 2: shell-only past the threshold ⇒ the registration is cleared (Remove).
    cycle::run_cycle(&tmux, &manifests, &cfg).expect("cycle 2");
    assert_eq!(
        sx.pane_option(pane, "@agent_state"),
        "",
        "a dead registered pane must be deregistered once shell-only persists past the threshold"
    );
    assert_eq!(
        sx.pane_option(pane, "@tma_reg_dead_since"),
        "",
        "the reaper marker is cleared with the registration (it is in REMOVABLE)"
    );
}

#[test]
fn live_pidless_registered_pane_survives() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let sx = scratch();
    // A registered pane whose real process is a LIVE non-shell process the manifest cannot name (the
    // gemini shape). Its subtree is never shell-only, so the reaper holds it even with an aged marker.
    let out = sx.tmux(&[
        "new-session",
        "-d",
        "-s",
        "w",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 600",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane = "w:0.0";
    register(&sx, pane);
    // Even a stale marker from a prior blip must not cause a reap while a non-shell process runs.
    sx.set_opt(
        pane,
        "@tma_reg_dead_since",
        &(now_ms() - 40_000).to_string(),
    );

    let tmux = client(&sx);
    let manifests = manifests(&sx);
    let cfg = FoldConfig::default();

    cycle::run_cycle(&tmux, &manifests, &cfg).expect("cycle");
    assert_eq!(
        sx.pane_option(pane, "@agent_state"),
        "working",
        "a live pid-less agent (non-shell process in the subtree) must never be reaped"
    );
    assert_eq!(
        sx.pane_option(pane, "@tma_reg_dead_since"),
        "",
        "a non-shell process clears any stale reaper marker (flapping resets)"
    );
}
