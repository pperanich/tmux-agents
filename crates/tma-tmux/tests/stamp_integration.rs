//! Crown-jewel acceptance: the server-side write guards on a scratch tmux server.
//!
//! These replay adversarial traces against a real tmux server via the internal `tma debug stamp`
//! harness (which applies exactly one guarded chain). A scratch `tmux -L tma_test_<unique>` socket
//! started with `-f /dev/null` is killed on drop; the default server (with the user's live agents)
//! is never touched.
//!
//! These tests are the project's crown jewels; do not weaken them to pass.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::{scratch_tmux, unique_id};
use tma_test_support as common;

// Deliberately NOT folded onto the shared `tma-test-support::Scratch`: this harness is socket-only
// (no workdir) and its `tma` carries NO `--manifest-dir`, which the shared type would both add.
struct Scratch {
    socket: String,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        common::reap_orphan_scratch_servers();
        let unique = unique_id();
        Scratch {
            socket: format!("tma_test_{tag}_{unique}"),
        }
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        scratch_tmux(&self.socket, args)
    }

    /// Set a pane option directly (test scaffolding for the prior stamp).
    fn set(&self, pane: &str, key: &str, value: &str) {
        let out = self.tmux(&["set-option", "-p", "-t", pane, key, value]);
        assert!(out.status.success(), "set {key} failed");
    }

    fn get(&self, pane: &str, format: &str) -> String {
        let out = self.tmux(&["display-message", "-p", "-t", pane, format]);
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn tma(&self, args: &[&str]) -> std::process::Output {
        Command::new(common::tma_bin())
            .args(args)
            .arg("--socket-name")
            .arg(&self.socket)
            .env("TMA_CONFIG", common::empty_config_path())
            .output()
            .expect("spawn tma")
    }

    fn new_pane(&self) -> String {
        let out = self.tmux(&["new-session", "-d", "-x", "80", "-y", "24", "sleep 100000"]);
        assert!(
            out.status.success(),
            "new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pane = self.get("", "#{pane_id}");
        assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
        pane
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The non-panicking kill: this runs while a failed assertion is unwinding, where a panic
        // would abort the binary and leave every other scratch server behind.
        common::kill_scratch_server(&self.socket);
        common::cleanup_scratch_socket(&self.socket);
    }
}

fn have_tmux() -> bool {
    tma_test_support::tmux_available()
}

/// The conditional-write probe reports support on the test tmux (3.6a verified).
#[test]
fn probe_reports_conditional_writes_supported() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("probe");
    let pane = s.new_pane();
    let out = s.tma(&["debug", "stamp", &pane, "--mode", "probe"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("conditional-writes: yes"),
        "probe must confirm behaviour on 3.6a: {stdout}"
    );
}

/// Trace (a): a capture producer's `working` write MUST NOT clobber a hook-sourced
/// `blocked` stamp. Blocked survives, source stays `hook`, `@agent_since` unchanged.
#[test]
fn trace_a_hook_blocked_survives_capture_working_write() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("trace_a");
    let pane = s.new_pane();
    // Prior: a hook-sourced blocked claim.
    s.set(&pane, "@agent_source", "hook");
    s.set(&pane, "@agent_state", "blocked");
    s.set(&pane, "@agent_detail", "permission");
    s.set(&pane, "@agent_since", "1000");
    s.set(&pane, "@agent_evidence_at", "1000");
    s.set(&pane, "@agent_stamped_at", "1000");

    // A capture producer attempts to publish working (protect-hook guard).
    let out = s.tma(&[
        "debug",
        "stamp",
        &pane,
        "--mode",
        "publish",
        "--state",
        "working",
        "--source",
        "capture",
        "--guard",
        "protect-hook",
        "--evidence-at",
        "2000",
        "--since",
        "2000",
        "--stamped-at",
        "2000",
    ]);
    assert!(
        out.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "blocked",
        "blocked must survive"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_source}"),
        "hook",
        "source stays hook"
    );
    assert_eq!(s.get(&pane, "#{@agent_since}"), "1000", "since unchanged");
    assert_eq!(
        s.get(&pane, "#{@agent_detail}"),
        "permission",
        "detail unchanged"
    );
    // Freshness still refreshes (writes-on-hold semantics for the tuple).
    assert_eq!(
        s.get(&pane, "#{@agent_stamped_at}"),
        "2000",
        "stamped_at refreshes"
    );
}

/// Trace (b) part 1: blocker chrome whose capture POSTDATES the hook claim overrides it.
#[test]
fn trace_b_newer_blocker_chrome_overrides_hook_working() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("trace_b1");
    let pane = s.new_pane();
    s.set(&pane, "@agent_source", "hook");
    s.set(&pane, "@agent_state", "working");
    s.set(&pane, "@agent_since", "1000");
    s.set(&pane, "@agent_evidence_at", "1000");
    s.set(&pane, "@agent_stamped_at", "1000");

    // Capture at 2000 > stamped evidence 1000 → carve-out fires.
    let out = s.tma(&[
        "debug",
        "stamp",
        &pane,
        "--mode",
        "publish",
        "--state",
        "blocked",
        "--detail",
        "permission",
        "--source",
        "capture",
        "--guard",
        "carveout:2000",
        "--evidence-at",
        "2000",
        "--since",
        "2000",
        "--stamped-at",
        "2000",
        "--attention",
    ]);
    assert!(
        out.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "blocked",
        "newer blocker overrides"
    );
    assert_eq!(s.get(&pane, "#{@agent_detail}"), "permission");
    assert_eq!(s.get(&pane, "#{@agent_source}"), "capture");
    assert_eq!(
        s.get(&pane, "#{@agent_since}"),
        "2000",
        "transition recorded"
    );
    assert_eq!(s.get(&pane, "#{@agent_attention}"), "1", "attention set");
}

/// Trace (b) part 2: blocker chrome whose capture PREDATES the hook claim is suppressed
/// (the answered-prompt race: the hook is newer evidence and wins with no decay wait).
#[test]
fn trace_b_older_blocker_chrome_is_suppressed() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("trace_b2");
    let pane = s.new_pane();
    s.set(&pane, "@agent_source", "hook");
    s.set(&pane, "@agent_state", "working");
    s.set(&pane, "@agent_since", "5000");
    s.set(&pane, "@agent_evidence_at", "5000");
    s.set(&pane, "@agent_stamped_at", "5000");

    // Capture at 2000 < stamped evidence 5000 → carve-out suppressed.
    let out = s.tma(&[
        "debug",
        "stamp",
        &pane,
        "--mode",
        "publish",
        "--state",
        "blocked",
        "--detail",
        "permission",
        "--source",
        "capture",
        "--guard",
        "carveout:2000",
        "--evidence-at",
        "2000",
        "--since",
        "2000",
        "--stamped-at",
        "2000",
        "--attention",
    ]);
    assert!(
        out.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        s.get(&pane, "#{@agent_state}"),
        "working",
        "hook working (newer) wins"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_source}"),
        "hook",
        "source stays hook"
    );
    assert_eq!(s.get(&pane, "#{@agent_since}"), "5000", "since unchanged");
    assert_eq!(
        s.get(&pane, "#{@agent_detail}"),
        "",
        "no detail written under suppression"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_attention}"),
        "",
        "no attention under suppression"
    );
}

/// Trace (c): torn-read ordering. A reader observing a fresh `@agent_stamped_at` MUST also observe
/// the matching `@agent_state` (stamped_at is written last). Hammer the guarded chain while a
/// concurrent reader classifies every tuple it sees.
#[test]
fn trace_c_torn_read_never_sees_stamped_at_ahead_of_state() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("trace_c");
    let pane = s.new_pane();
    // Seed a settled tuple so the reader has something to parse from the first poll.
    for (k, v) in [
        ("@agent_source", "capture"),
        ("@agent_state", "idle"),
        ("@agent_since", "1"),
        ("@agent_evidence_at", "1"),
        ("@agent_stamped_at", "1"),
    ] {
        s.set(&pane, k, v);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    let socket = s.socket.clone();
    let pane_r = pane.clone();

    // Reader: one atomic display-message reads the whole tuple, classified with the in-progress
    // predicate; a Settled tuple must be self-consistent (even stamped_at ⇒ working, odd ⇒ idle).
    let reader = std::thread::spawn(move || {
        let mut settled_reads = 0u64;
        while !reader_stop.load(Ordering::Relaxed) {
            let out = Command::new("tmux")
                .arg("-L")
                .arg(&socket)
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    &pane_r,
                    "#{@agent_since}:#{@agent_evidence_at}:#{@agent_stamped_at}:#{@agent_state}",
                ])
                .output();
            let Ok(out) = out else { continue };
            let line = String::from_utf8_lossy(&out.stdout);
            let line = line.trim();
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() != 4 {
                continue;
            }
            let (Ok(since), Ok(evidence), Ok(stamped)) = (
                parts[0].parse::<u64>(),
                parts[1].parse::<u64>(),
                parts[2].parse::<u64>(),
            ) else {
                continue;
            };
            let state = parts[3];
            if state.is_empty() {
                continue;
            }
            // The read-consistency predicate.
            let in_progress = stamped < since || stamped < evidence;
            if in_progress {
                continue;
            }
            settled_reads += 1;
            let expected = if stamped % 2 == 0 { "working" } else { "idle" };
            assert_eq!(
                state, expected,
                "torn read: settled stamped_at={stamped} but state={state} (expected {expected})"
            );
        }
        settled_reads
    });

    // Writer: alternate state with a monotonically increasing stamped_at, unconditional.
    for i in 2..=250u64 {
        let state = if i % 2 == 0 { "working" } else { "idle" };
        let n = i.to_string();
        let out = s.tma(&[
            "debug",
            "stamp",
            &pane,
            "--mode",
            "publish",
            "--state",
            state,
            "--source",
            "capture",
            "--guard",
            "unconditional",
            "--evidence-at",
            &n,
            "--since",
            &n,
            "--stamped-at",
            &n,
        ]);
        assert!(out.status.success(), "stamp {i} failed");
    }

    stop.store(true, Ordering::Relaxed);
    let settled = reader.join().unwrap();
    assert!(settled > 0, "reader observed at least one settled tuple");
}

/// cmdq-no-yield probe: two producers race guarded chains on ONE pane; every Settled tuple a reader
/// observes must be self-consistent. The "whole tuple commits together" guarantee (render.rs) rests
/// on tmux never interleaving another client's command mid-chain; a yield would surface as a tuple
/// mixing producer A's state with B's source+detail. Writer 1 (capture) republishes `idle` under
/// `Guard::ProtectHook` (reasserting a baseline first to reopen the race a bare protect-hook writer
/// would lock out); writer 2 (hook) alternates `blocked/permission` and `working/thinking`. The
/// reader asserts each Settled read: `source=hook` ⇒ one of writer 2's pairs, `source=capture` ⇒
/// `idle` with empty detail.
#[test]
fn cross_chain_guard_tear_atomicity_under_concurrent_writers() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("guardtear");
    let pane = s.new_pane();

    // Writer 2's exact hook pairs; a Settled hook tuple must equal one of these.
    const HOOK_PAIRS: [(&str, &str); 2] = [("blocked", "permission"), ("working", "thinking")];
    // Writer 1's capture state; its detail is always empty.
    const W1_STATE: &str = "idle";
    // "A few hundred chain writes per writer" (writer 1 emits two chains per iteration).
    const W1_ITERS: u64 = 200;
    const W2_ITERS: u64 = 400;

    let bin_owned = common::tma_bin();
    let bin = bin_owned.as_str();
    let cfg = common::empty_config_path();
    let socket = s.socket.clone();

    // One shared monotonic clock (since==evidence==stamped==n per publish), so a complete tuple never
    // trips the reader's `stamped < since || stamped < evidence` in-progress guard.
    let clock = Arc::new(AtomicU64::new(1000));
    let stop = Arc::new(AtomicBool::new(false));
    // First torn read wins; asserted on the main thread for a legible failure message.
    let tear: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // How many Settled tuples the reader vetted; the test is only meaningful if it saw some.
    let settled_reads = Arc::new(AtomicU64::new(0));

    // One guarded `set-option` chain via the internal write adapter. Returns false on a spawn
    // that did not exit 0 (a killed server during teardown), which the writers treat as "stop".
    fn stamp(bin: &str, socket: &str, cfg: &Path, pane: &str, extra: &[&str]) -> bool {
        Command::new(bin)
            .args(["debug", "stamp", pane])
            .args(extra)
            .args(["--socket-name", socket])
            .env("TMA_CONFIG", cfg)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    // Seed a consistent capture tuple atomically so the reader parses from its first poll.
    let n = clock.fetch_add(1, Ordering::Relaxed).to_string();
    assert!(
        stamp(
            bin,
            &socket,
            cfg,
            &pane,
            &[
                "--mode",
                "publish",
                "--state",
                W1_STATE,
                "--source",
                "capture",
                "--guard",
                "unconditional",
                "--evidence-at",
                &n,
                "--since",
                &n,
                "--stamped-at",
                &n,
            ],
        ),
        "seed publish failed"
    );

    let chains = std::thread::scope(|scope| {
        // Reader: classify every Settled tuple; record the first contradiction and halt.
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let out = Command::new("tmux")
                    .arg("-L")
                    .arg(&socket)
                    .args([
                        "display-message",
                        "-p",
                        "-t",
                        &pane,
                        "#{@agent_source}:#{@agent_state}:#{@agent_detail}:#{@agent_since}:#{@agent_evidence_at}:#{@agent_stamped_at}",
                    ])
                    .output();
                let Ok(out) = out else { continue };
                let line = String::from_utf8_lossy(&out.stdout);
                let parts: Vec<&str> = line.trim().split(':').collect();
                if parts.len() != 6 {
                    continue;
                }
                let (source, state, detail) = (parts[0], parts[1], parts[2]);
                let (Ok(since), Ok(evidence), Ok(stamped)) = (
                    parts[3].parse::<u64>(),
                    parts[4].parse::<u64>(),
                    parts[5].parse::<u64>(),
                ) else {
                    continue;
                };
                if source.is_empty() || stamped < since || stamped < evidence {
                    continue; // unseeded, removed, or mid-write
                }
                settled_reads.fetch_add(1, Ordering::Relaxed);
                let ok = match source {
                    "hook" => HOOK_PAIRS.contains(&(state, detail)),
                    "capture" => state == W1_STATE && detail.is_empty(),
                    _ => true, // no other producer writes @agent_source in this test
                };
                if !ok {
                    *tear.lock().unwrap() = Some(format!(
                        "torn tuple: source={source} state={state} detail={detail:?} \
                         (since={since} evidence={evidence} stamped={stamped})"
                    ));
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });

        // Writer 1 (capture producer): reclaim-then-guarded, per the doc comment above.
        // Returns the number of chains it committed so the probe can prove it really ran.
        let w1 = scope.spawn(|| {
            let mut chains = 0u64;
            for _ in 0..W1_ITERS {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let n = clock.fetch_add(1, Ordering::Relaxed).to_string();
                if !stamp(
                    bin,
                    &socket,
                    cfg,
                    &pane,
                    &[
                        "--mode",
                        "publish",
                        "--state",
                        W1_STATE,
                        "--source",
                        "capture",
                        "--guard",
                        "unconditional",
                        "--evidence-at",
                        &n,
                        "--since",
                        &n,
                        "--stamped-at",
                        &n,
                    ],
                ) {
                    break;
                }
                chains += 1;
                let n = clock.fetch_add(1, Ordering::Relaxed).to_string();
                if !stamp(
                    bin,
                    &socket,
                    cfg,
                    &pane,
                    &[
                        "--mode",
                        "publish",
                        "--state",
                        W1_STATE,
                        "--source",
                        "capture",
                        "--guard",
                        "protect-hook",
                        "--evidence-at",
                        &n,
                        "--since",
                        &n,
                        "--stamped-at",
                        &n,
                    ],
                ) {
                    break;
                }
                chains += 1;
            }
            chains
        });

        // Writer 2 (hook producer): unconditional, alternating pairs.
        let w2 = scope.spawn(|| {
            let mut chains = 0u64;
            for i in 0..W2_ITERS {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (state, detail) = HOOK_PAIRS[(i % 2) as usize];
                let n = clock.fetch_add(1, Ordering::Relaxed).to_string();
                if !stamp(
                    bin,
                    &socket,
                    cfg,
                    &pane,
                    &[
                        "--mode",
                        "publish",
                        "--state",
                        state,
                        "--detail",
                        detail,
                        "--source",
                        "hook",
                        "--guard",
                        "unconditional",
                        "--evidence-at",
                        &n,
                        "--since",
                        &n,
                        "--stamped-at",
                        &n,
                    ],
                ) {
                    break;
                }
                chains += 1;
            }
            chains
        });

        // Join the writers, THEN release the reader; the scope's implicit join would
        // otherwise wait on a reader that only stops when we tell it to.
        let w1_chains = w1.join().unwrap();
        let w2_chains = w2.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        (w1_chains, w2_chains)
    });

    if let Some(msg) = tear.lock().unwrap().take() {
        panic!("cross-chain guard tear observed, cmdq-no-yield assumption VIOLATED: {msg}");
    }
    // No tear ⇒ nothing stopped the writers early; a failed stamp spawn (which breaks a
    // writer's loop silently) would shrink the probe to nothing, so prove the full load ran.
    assert_eq!(chains, (2 * W1_ITERS, W2_ITERS), "every chain must commit");
    assert!(
        settled_reads.load(Ordering::Relaxed) > 0,
        "reader must have vetted at least one settled tuple"
    );
}

/// `@agent_summary` recompute (window and session mirror) + agent-exit removal. A publish rolls
/// both scopes up; a removal clears the pane options and drops it from both.
#[test]
fn window_summary_recompute_and_removal() {
    if !have_tmux() {
        return;
    }
    let s = Scratch::new("summary");
    let pane = s.new_pane();

    // Publish blocked → window summary rolls up this one agent.
    let out = s.tma(&[
        "debug",
        "stamp",
        &pane,
        "--mode",
        "publish",
        "--state",
        "blocked",
        "--source",
        "capture",
        "--guard",
        "unconditional",
        "--evidence-at",
        "10",
        "--since",
        "10",
        "--stamped-at",
        "10",
        "--pid",
        "4242",
        "--name",
        "claude",
    ]);
    assert!(
        out.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        s.get(&pane, "#{@agent_summary}"),
        "blocked:1",
        "window rollup"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_session_summary}"),
        "blocked:1",
        "session mirror carries the same grammar"
    );
    assert_eq!(s.get(&pane, "#{@agent_name}"), "claude", "identity written");
    assert_eq!(s.get(&pane, "#{@agent_pid}"), "4242");

    // Remove → all @agent_* pane options cleared; the window summary empties (agentless).
    let out = s.tma(&["debug", "stamp", &pane, "--mode", "remove"]);
    assert!(
        out.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.get(&pane, "#{@agent_state}"), "", "state option removed");
    assert_eq!(s.get(&pane, "#{@agent_pid}"), "", "pid removed");
    assert_eq!(
        s.get(&pane, "#{@agent_summary}"),
        "",
        "summary empty when agentless"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_session_summary}"),
        "",
        "session mirror empties too"
    );
}
