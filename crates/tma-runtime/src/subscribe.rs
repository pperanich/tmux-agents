//! `tma subscribe`: the surface subscribe stream. A long-running loop that cycles and emits,
//! riding the daemon's edge pushes when one is present (probe → `TMAS` → wake → its OWN
//! [`cycle::run_cycle`]) and degrading to an `--interval` poll when not. It lives beside the `wait`
//! client ([`crate::ipc`]) so the wake-hint design pays off twice; the daemon is unchanged. Every
//! emitted line is ALWAYS built from the subscriber's own cycle, never from a socket byte: a `PUSH`
//! is a wake hint only, which is exactly what keeps push and poll modes contract-identical by
//! construction.
//!
//! The loop owns the WAKE policy and nothing else. What a wake turns into — a snapshot document, the
//! same document only when it changed, or a set of transition records — belongs to the injected
//! renderer, which returns the lines to emit and an empty vec for "nothing to say". So a new
//! emission mode is a new renderer in the bin, not a branch in here.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tma_tmux::tmux::{Tmux, TmuxError};

use crate::config::{self, Config};
use crate::cycle::{self, CycleReport};
use crate::ipc::{WaitSubscription, WaitWake};
use crate::manifests::LoadedManifest;

/// The 100 ms hint-coalescing window: pushes arriving within it collapse into one emission, so an
/// edge burst does not spam identical documents.
const COALESCE: Duration = Duration::from_millis(100);

/// The push-mode belt cadence: a fallback re-cycle for an edge the daemon did not push (a wedged
/// subscription that never EOFs). Deliberately longer than a typical `--interval`, and it emits ONLY
/// on an observed change, so a quiet system stays silent in push mode (there is no heartbeat).
const BELT: Duration = Duration::from_secs(5);

/// How often poll mode re-probes for a returning daemon, so one started later is picked back
/// up. Cheap: a non-blocking connect that fails fast when none is listening.
const REPROBE: Duration = Duration::from_secs(5);

/// Why the stream loop returned. It runs until stdout closes (the consumer owns the process's stdout;
/// EOF ⇒ dead, respawn) or the tmux server vanishes — never on a degrade, which is invisible latency.
pub enum StreamEnd {
    /// stdout closed (a write hit `BrokenPipe`): a clean exit, the consumer went away.
    StdoutClosed,
    /// The tmux server is gone: nothing left to observe.
    ServerGone,
    /// A non-transient tmux failure (spawn/parse): surfaced verbatim by the caller.
    Failed(TmuxError),
}

/// Everything the loop owns so it hot-reloads config + manifests on its own tick, exactly as `tma
/// wait` and the picker do.
pub struct StreamParams {
    /// Poll-mode emit cadence AND the degrade cadence (`--interval`, default 1 s).
    pub interval: Duration,
    /// `--changes-only`: make the poll tick a [`Tick::OnChange`] wake, so the daemonless path stops
    /// re-emitting an unchanged document every interval. Push mode already behaves this way (its
    /// wakes are edges), so the flag is a silent no-op there.
    pub changes_only: bool,
    pub config_path: Option<PathBuf>,
    pub manifest_dir: Option<PathBuf>,
}

/// Why the loop is about to render, so the renderer knows whether it may skip an emission. The loop
/// owns the wake policy; what "unchanged" means belongs to the renderer (a snapshot compares
/// documents, an edge stream has nothing to emit when nothing moved).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    /// The entry cycle and every wake that guarantees an emission (a daemon push, a poll tick).
    Forced,
    /// A belt re-cycle (and a `--changes-only` poll tick): emit only what actually changed.
    OnChange,
}

/// Run the subscribe stream until stdout closes or the server vanishes. `render` turns a cycle's
/// report into the lines to emit, empty for nothing (injected so this tier stays free of `tma-ui`);
/// `emit` writes one rendered line and reports `Err` when stdout is closed. The loop NEVER decodes a
/// socket byte into state — pushes are drained purely as wake hints — so every emitted line comes
/// from `render(&run_cycle(...))`, which is what keeps push and poll modes contract-identical.
pub fn run_stream(
    tmux: &Tmux,
    mut config: Config,
    mut manifests: Vec<LoadedManifest>,
    params: StreamParams,
    mut render: impl FnMut(&CycleReport, Tick) -> Vec<String>,
    mut emit: impl FnMut(&str) -> std::io::Result<()>,
) -> StreamEnd {
    let StreamParams {
        interval,
        changes_only,
        config_path,
        manifest_dir,
    } = params;

    // Ride the daemon's edge pushes when present; `None` is the silent poll degrade (no daemon, a
    // older daemon that NAKs the subscribe magic, or any I/O error). The entry cycle emits regardless.
    let mut subscription = WaitSubscription::try_subscribe(tmux);
    let mut last_probe = Instant::now();
    // The entry cycle and every push / poll tick force an emit; the push-mode belt (and a
    // `--changes-only` poll tick) leave it to the renderer to decide there is nothing to say.
    let mut tick = Tick::Forced;
    // The last reported hot-reload failure, so a config left malformed says so once per breakage.
    let mut reload_error: Option<String> = None;

    loop {
        // Hot-reload all-or-nothing (keep the last good pair on a mid-save error), then one cycle.
        // The failure goes to stderr: stdout is the subscriber's stream and must stay parseable.
        if let Some(msg) = config::reload_notice(
            config::reload_pair(
                &mut config,
                &mut manifests,
                config_path.as_deref(),
                manifest_dir.as_deref(),
            ),
            &mut reload_error,
        ) {
            eprintln!("{msg}");
        }
        // Deferred, never inline: `done` is idle + `@agent_attention`, and an ordered-input clear
        // running inside the cycle retracts the flag before the renderer sees the rows — so the
        // completion is not merely followed by its retraction, it is never reported at all. The
        // clear still happens, after the emission, exactly as the daemon orders it around dispatch.
        match cycle::run_cycle_with(
            tmux,
            &manifests,
            &config.fold_config(),
            cycle::SeenClear::Deferred,
        ) {
            Ok(report) => {
                for line in render(&report, tick) {
                    if emit(&line).is_err() {
                        return StreamEnd::StdoutClosed;
                    }
                }
                if !report.deferred_seen.is_empty() {
                    crate::seen::clear_seen(tmux, &report.deferred_seen);
                }
            }
            // A transient tmux stall (a socket blip) is ridden out as a skipped emission, never an
            // error line: the next wake re-cycles. Only a gone server or a hard failure ends the stream.
            Err(TmuxError::Timeout { .. }) => {}
            Err(TmuxError::ServerGone) => return StreamEnd::ServerGone,
            Err(err) => return StreamEnd::Failed(err),
        }

        // Wait for the next wake and set the emit rule for the cycle it drives.
        match subscription.as_mut() {
            Some(sub) => match sub.wait_edge(BELT) {
                WaitWake::Pushed => {
                    // Coalesce a burst (the 100 ms debounce), THEN loop to a fresh cycle so the
                    // emission reflects the latest state. A hangup mid-window degrades to polling.
                    if !sub.coalesce(COALESCE) {
                        subscription = None;
                    }
                    tick = Tick::Forced;
                }
                // Belt: a periodic safety re-cycle that emits only on an observed change.
                WaitWake::Elapsed => tick = Tick::OnChange,
                // Daemon died/restarted mid-stream: degrade to the poll loop — no error, no EOF.
                WaitWake::Closed => {
                    subscription = None;
                    tick = Tick::OnChange;
                }
            },
            None => {
                // Poll mode: emit every `interval` unconditionally (the pre-daemon self-poller
                // contract) unless `--changes-only` asked for the push-mode discipline instead, and
                // periodically re-probe for a returning daemon.
                std::thread::sleep(interval);
                tick = if changes_only {
                    Tick::OnChange
                } else {
                    Tick::Forced
                };
                if last_probe.elapsed() >= REPROBE {
                    last_probe = Instant::now();
                    subscription = WaitSubscription::try_subscribe(tmux);
                }
            }
        }
    }
}
