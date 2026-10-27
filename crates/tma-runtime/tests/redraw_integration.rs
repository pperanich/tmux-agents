//! Acceptance: how many `tmux` write invocations one poll cycle costs, on a scratch server.
//!
//! A working cycle batches the pane stamp and advisory `@tma_last_poll` hint in one writing
//! invocation. A logging shim counts actual spawns; it does not measure attached-client redraws.
//! The hint proves complete server execution of the exact receipt, even if a later replacement
//! defeats client readback.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use tma_core::FoldConfig;
use tma_runtime::cycle;
use tma_runtime::manifests::{self, LoadedManifest};
use tma_tmux::tmux::{Server, Tmux};

use common::Scratch;
use tma_test_support as common;

/// Long enough that a stamp taken after it is a whole activity-second past the pane's last write,
/// which is what the quiet-pane skip requires (tmux reports activity in whole seconds).
const PAST_ONE_ACTIVITY_SECOND: Duration = Duration::from_millis(1200);

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// The first `tmux` on this process's PATH, for the shim to exec.
fn real_tmux() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("tmux"))
        .find(|p| p.is_file())
        .expect("tmux on PATH")
}

/// A `tmux` stand-in that appends its whole argv to a log file and then execs the real binary. One
/// log line per spawn, which is what "invocations per cycle" counts.
fn logging_shim(sx: &Scratch) -> (PathBuf, PathBuf) {
    let shim = sx.workdir.join("tmux-shim");
    let log = sx.workdir.join("tmux-calls.log");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexec {real} \"$@\"\n",
            log = log.display(),
            real = real_tmux().display(),
        ),
    )
    .expect("write shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
    (shim, log)
}

/// A `Tmux` bound to this scratch server through the logging shim.
fn client(sx: &Scratch, shim: &Path) -> Tmux {
    Tmux::connect(&Server {
        socket_name: Some(sx.socket.clone()),
        bin: Some(shim.to_string_lossy().into_owned()),
        ..Server::default()
    })
}

/// The invocations logged so far that wrote at least one option. Chained writes are one spawn, so
/// this counts lines, never `set-option` tokens.
fn option_writes(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.contains("'set-option'") || l.split_whitespace().any(|word| word == "set-option")
        })
        .map(String::from)
        .collect()
}

/// Forget everything logged so far, so the next cycle is measured on its own.
fn reset_log(log: &Path) {
    std::fs::remove_file(log).ok();
}

/// A manifest matching the pane's real process names (discovered at runtime, so this works on both
/// macOS and Linux), with one rule keyed on `marker`.
fn write_manifest(sx: &Scratch, pane: &str, state: &str, marker: &str) {
    let current_command = basename(&sx.display(pane, "#{pane_current_command}"));
    let pane_pid = sx.display(pane, "#{pane_pid}");
    let ps = Command::new("ps")
        .args(["-o", "comm=", "-p", &pane_pid])
        .output()
        .expect("ps");
    let mut names = vec![
        current_command,
        basename(&String::from_utf8_lossy(&ps.stdout)),
    ];
    names.sort();
    names.dedup();
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        sx.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names_toml}]\n\
             [capture]\nvisible = [\"working\", \"idle\"]\n\
             [[rules]]\nstate = \"{state}\"\npriority = 100\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"{marker}\" }}\n",
        ),
    )
    .expect("write manifest");
}

fn manifests(sx: &Scratch) -> Vec<LoadedManifest> {
    manifests::load(Some(&sx.workdir), &[])
        .expect("load test manifest")
        .manifests
}

/// A one-pane scratch server whose screen carries `marker`, with a manifest folding it to `state`.
fn scratch_with_pane(tag: &str, marker: &str, state: &str) -> (Scratch, String) {
    let sx = Scratch::new(tag);
    let out = sx.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        &format!("printf '{marker}\\n'; exec sleep 100000"),
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_capture_contains(&sx.socket, "", marker, common::POLL_CEILING),
        "the pane never rendered"
    );
    let pane = sx.display("", "#{pane_id}");
    write_manifest(&sx, &pane, state, marker);
    (sx, pane)
}

/// Drop the stampede claim so the next cycle produces instead of consuming. Goes through the
/// scratch harness's own tmux, not the shim, so it never lands in the measured log.
fn release_stampede_claim(sx: &Scratch) {
    sx.tmux(&["set-option", "-su", "@tma_last_poll"]);
}

#[test]
fn a_working_pane_costs_one_option_write_per_cycle() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (sx, pane) = scratch_with_pane("redraw-working", "WORKING", "working");
    let (shim, log) = logging_shim(&sx);
    let tmux = client(&sx, &shim);
    let manifests = manifests(&sx);
    // `freshness_secs = 0` puts the pane on the producer path every cycle, which is also what a
    // real working pane does: `can_reuse_stamp` never reuses a `working` stamp.
    let cfg = FoldConfig {
        freshness_secs: 0,
        ..FoldConfig::default()
    };

    // Warm-up: caches the `-F` capability probe in `@tma_setpf_ok` and settles the summaries, so
    // the measured cycle carries only its own writes.
    cycle::run_cycle(&tmux, &manifests, &cfg).expect("warm-up cycle");
    assert_eq!(
        sx.pane_option(&pane, "@agent_state"),
        "working",
        "the pane must fold to working for this to measure a producing cycle"
    );

    release_stampede_claim(&sx);
    reset_log(&log);
    let report = cycle::run_cycle(&tmux, &manifests, &cfg).expect("measured cycle");
    assert_eq!(report.skipped_quiet, 0, "a working pane is never skipped");

    let writes = option_writes(&log);
    assert_eq!(
        writes.len(),
        1,
        "one working pane must cost ONE option-writing tmux invocation. \
         Invocations logged:\n{}",
        writes.join("\n")
    );
    // The advisory hint retains its server key and shares the pane-writing invocation.
    assert!(
        writes[0].contains("@agent_state") && writes[0].contains("@tma_last_poll"),
        "the stampede claim must ride the stamp chain: {}",
        writes[0]
    );
    assert!(
        !sx.display("", "#{@tma_last_poll}").is_empty(),
        "a producing cycle still claims the stampede hint"
    );
}

#[test]
fn an_all_quiet_cycle_writes_no_options_at_all() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (sx, pane) = scratch_with_pane("redraw-quiet", "READY", "idle");
    let (shim, log) = logging_shim(&sx);
    let tmux = client(&sx, &shim);
    let manifests = manifests(&sx);
    let cfg = FoldConfig {
        freshness_secs: 1,
        ..FoldConfig::default()
    };

    // The stamp has to land a whole activity-second after the pane's last write, or the quiet-pane
    // skip cannot tell "nothing happened since" from "written 300 ms after the stamp".
    sleep(PAST_ONE_ACTIVITY_SECOND);
    cycle::run_cycle(&tmux, &manifests, &cfg).expect("warm-up cycle");
    assert_eq!(
        sx.pane_option(&pane, "@agent_state"),
        "idle",
        "the pane must fold to idle for the quiet skip to apply"
    );

    // Past the freshness window, so the pane reaches the producer path and is skipped there rather
    // than being trivially consumed as fresh.
    release_stampede_claim(&sx);
    sleep(PAST_ONE_ACTIVITY_SECOND);
    reset_log(&log);
    let report = cycle::run_cycle(&tmux, &manifests, &cfg).expect("measured cycle");
    assert_eq!(
        report.skipped_quiet, 1,
        "the pane must reach the producer path and be skipped as quiet"
    );

    let writes = option_writes(&log);
    assert!(
        writes.is_empty(),
        "an all-quiet cycle must write no option at all, the stampede claim included, \
         so this quiet cycle creates no option-writing client. Invocations logged:\n{}",
        writes.join("\n")
    );
    assert!(
        sx.display("", "#{@tma_last_poll}").is_empty(),
        "a cycle that produced nothing must not claim the hint"
    );
}

#[test]
fn departed_pane_during_readback_is_skipped_but_malformed_live_readback_is_fatal() {
    if !common::tmux_available() {
        return;
    }
    for departed in [true, false] {
        let (sx, pane) = scratch_with_pane("readback-departure", "WORKING", "working");
        assert!(sx
            .tmux(&["new-session", "-d", "-s", "keep", "exec /bin/cat"])
            .status
            .success());
        let manifests = manifests(&sx);
        let cfg = FoldConfig {
            freshness_secs: 0,
            ..Default::default()
        };
        let plain = Tmux::new(Some(sx.socket.clone()));
        cycle::run_cycle(&plain, &manifests, &cfg).unwrap();
        release_stampede_claim(&sx);
        let ready = sx.workdir.join("readback-ready");
        let release = sx.workdir.join("readback-release");
        let result = sx.workdir.join("readback-result");
        let wrapper = sx.workdir.join("gated-readback");
        std::fs::write(&wrapper, format!(
            "#!/bin/sh\nfor arg do\ncase \"$arg\" in\n*'@tma_program_origin'*'end')\ntouch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.01; i=$((i+1)); done\n[ -e '{}' ] || exit 125\n{}\nexit $?\n;;\nesac\ndone\nexec tmux \"$@\"\n",
            ready.display(), release.display(), release.display(),
            if departed { format!("tmux \"$@\" > '{}'\nstatus=$?\nprintf '%s' \"$status\" > '{}.status'\ncat '{}'\nexit \"$status\"", result.display(), result.display(), result.display()) }
            else { "printf 'malformed-live-pane'".into() }
        )).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let delayed = client(&sx, &wrapper);
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| cycle::run_cycle(&delayed, &manifests, &cfg));
            common::poll_until("actual persistence readback is gated", || ready.exists());
            if departed {
                assert!(sx.tmux(&["kill-pane", "-t", &pane]).status.success());
            }
            std::fs::write(&release, "release").unwrap();
            let observed = pending.join().unwrap();
            if departed {
                eprintln!(
                    "departed readback: native_status={}, native_output={:?}, cycle={observed:?}",
                    std::fs::read_to_string(result.with_extension("status")).unwrap(),
                    std::fs::read_to_string(&result).unwrap()
                );
                let report = observed.expect("a proved departed pane is skipped");
                assert!(report.skipped_dead > 0);
                assert!(!report.rows.iter().any(|row| row.pane_id == pane));
            } else {
                assert!(
                    matches!(observed, Err(tma_tmux::tmux::TmuxError::Parse { .. })),
                    "malformed output for a live pane must fail closed: {observed:?}"
                );
            }
        });
    }
}
