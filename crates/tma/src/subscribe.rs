//! `tma subscribe`: spawn one long-running process that streams the read path. By default it emits
//! one complete `ls --json` schema-1 document per line (snapshot semantics), riding the daemon's edge
//! pushes when present and degrading to an `--interval` poll otherwise — contract-identical either
//! way. A plugin spawns THIS instead of a polling timer and re-renders on each line; it exits
//! only on a signal or when its stdout closes (the consumer went away). `--events` swaps the
//! snapshots for one edge record per state transition (the pure `tma_core::diff_rows`), which is the
//! shape a log wants. The loop itself lives in `tma-runtime` beside the `wait` client; this file is the
//! thin bin dispatch (the ls/wait precedent) and owns the per-mode emission policy.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use tma_core::AgentRow;
use tma_runtime::subscribe::{run_stream, StreamEnd, StreamParams, Tick};
use tma_ui::surfaces;

use crate::cli_support;
use crate::config::Config;
use crate::cycle::CycleReport;
use crate::tmux;

/// Assembled by the bin's dispatch from the CLI args + loaded config, mirroring `WaitOpts`.
pub(crate) struct SubscribeOpts {
    pub json: bool,
    pub interval: u64,
    /// `--changes-only`: suppress a poll-mode emission that would repeat the last document.
    pub changes_only: bool,
    /// `--events`: emit transitions instead of snapshots.
    pub events: bool,
    /// Narrows each emitted document's rows. Applied at render (and so BEFORE the edge diff), so the
    /// loop's cycle — and therefore the stamping of every pane — is untouched.
    pub selector: tma_core::Selector,
    pub server: tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    pub config: Config,
    /// The `--config` path (env/defaults resolve inside the loader), for the tick reload.
    pub config_path: Option<PathBuf>,
}

pub(crate) fn run(opts: SubscribeOpts) -> ExitCode {
    let SubscribeOpts {
        json,
        interval,
        changes_only,
        events,
        selector,
        server,
        manifest_dir,
        config,
        config_path,
    } = opts;

    // JSON is the only supported emission today (snapshots or, under `--events`, edges); require the
    // flag explicitly so a future line format can be added without changing the default. A usage
    // error, like a bad flag combination (exit 2).
    if !json {
        eprintln!("tma: --json is required (the stream emits one JSON document per line)");
        return ExitCode::from(2);
    }
    if interval == 0 {
        eprintln!("tma: --interval must be at least 1 second");
        return ExitCode::from(2);
    }

    let manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let tmux = tmux::Tmux::connect(&server);

    // The row documents' provenance keys: resolved once here, not per cycle and never per row (one
    // tmux call and one `uname` for the life of the stream).
    let origin = tma_runtime::origin::Origin::resolve(&tmux);

    // One document per line, flushed so a piped consumer sees each line promptly. A write error
    // (BrokenPipe) means the consumer closed our stdout: a clean exit.
    let stdout = io::stdout();
    let emit = |doc: &str| -> io::Result<()> {
        let mut h = stdout.lock();
        h.write_all(doc.as_bytes())?;
        h.write_all(b"\n")?;
        h.flush()
    };

    // Annotate each emission's rows with repo/branch/worktree just before render, so the stream's
    // documents match `ls --json`. `run_stream` builds every line from its own cycle and stays free
    // of `tma-ui` and the resolver; the closure clones the rows, annotates via the process-local
    // memo, and renders. Cloning is cheap against a spawn-avoiding, TTL-memoized resolve.
    let selected = |report: &CycleReport| -> Vec<AgentRow> {
        let mut rows = report.rows.clone();
        tma_runtime::repo::annotate_rows(&mut rows);
        selector.retain(&mut rows);
        rows
    };

    // Snapshot mode owns the last document emitted (its definition of "changed"); event mode owns
    // the last row set (the diff's left side). Neither is loop state: what an unchanged cycle means
    // differs per mode, so `run_stream` only says whether an emission is forced.
    let mut last_doc: Option<String> = None;
    let mut prev_rows: Option<Vec<AgentRow>> = None;

    let render = |report: &CycleReport, tick: Tick| -> Vec<String> {
        let rows = selected(report);
        if !events {
            let doc = surfaces::render_ls_json(
                &CycleReport {
                    rows,
                    ..Default::default()
                },
                &origin,
            );
            if tick == Tick::Forced || last_doc.as_deref() != Some(doc.as_str()) {
                last_doc = Some(doc.clone());
                return vec![doc];
            }
            return Vec::new();
        }
        // Events: the first cycle establishes the baseline and emits nothing. A consumer starting
        // fresh has no prior state to reconcile, so synthesizing "appeared" edges for panes that
        // were already running would be a lie about when they started.
        let Some(prev) = prev_rows.replace(rows) else {
            return Vec::new();
        };
        let next = prev_rows.as_deref().unwrap_or_default();
        let at_ms = tma_runtime::now_ms();
        tma_core::diff_rows(&prev, next)
            .iter()
            .map(|e| surfaces::render_edge_json(e, at_ms))
            .collect()
    };

    match run_stream(
        &tmux,
        config,
        manifests,
        StreamParams {
            interval: Duration::from_secs(interval),
            // Events are edge-triggered by construction, so the flag adds nothing there.
            changes_only: changes_only || events,
            config_path,
            manifest_dir,
        },
        render,
        emit,
    ) {
        StreamEnd::StdoutClosed => ExitCode::SUCCESS,
        StreamEnd::ServerGone => {
            eprintln!("tma: no tmux server running");
            ExitCode::FAILURE
        }
        StreamEnd::Failed(err) => {
            eprintln!("tma: {err}");
            ExitCode::FAILURE
        }
    }
}
