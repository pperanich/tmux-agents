//! Shared bin plumbing: the "no tmux server" error, the load-manifests-or-exit shape, and the
//! daemon-management seam the chained setup steps share, each reused verbatim across the
//! subcommands so their message + exit code cannot drift.

use std::path::Path;
use std::process::ExitCode;

use tma_core::StateToken;

use crate::config::AgentConfig;
use crate::manifests::{self, LoadedManifest, LoadedSet, ManifestFailure};

/// Parse a comma-separated state list (`--until`, `--state`) into de-duplicated tokens, in the
/// order written. `flag` names the offending option in the clap error (exit 2); empty segments are
/// skipped, so the caller decides whether an empty result is legal.
pub(crate) fn parse_states(s: &str, flag: &str) -> Result<Vec<StateToken>, String> {
    let mut out: Vec<StateToken> = Vec::new();
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let parsed = tok.parse::<StateToken>().map_err(|_| {
            format!(
                "unknown {flag} state {tok:?} (expected one of: {})",
                StateToken::VOCABULARY
            )
        })?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// The clap value parser behind `--state` and `--until`: the wrapped fn does the parsing, and this
/// adds the one thing a bare `value_parser = <fn>` cannot answer — what the legal values are.
///
/// clap asks the value parser for its possible values, and a function parser reports none, so the
/// vocabulary reached the user only through a rejection message. Reporting it here is what puts
/// `[possible values: …]` in `--help` and the tokens into the generated completion scripts. It is
/// not a second validator: clap uses possible values for help, completion, and error text, never to
/// accept or reject, so the comma-separated grammar the fn implements still governs.
#[derive(Clone)]
pub(crate) struct StateListParser<T: 'static>(pub(crate) fn(&str) -> Result<T, String>);

impl<T: Clone + Send + Sync + 'static> clap::builder::TypedValueParser for StateListParser<T> {
    type Value = T;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<T, clap::Error> {
        // Delegated to clap's own blanket impl for `Fn(&str) -> Result<T, E>`, so the error text is
        // byte-identical to what the plain `value_parser = <fn>` form produced.
        clap::builder::TypedValueParser::parse_ref(&self.0, cmd, arg, value)
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(
            StateToken::ALL
                .iter()
                .map(|t| clap::builder::PossibleValue::new(t.token())),
        ))
    }
}

/// Print the standard "no tmux server running" error and yield a FAILURE exit code — the shape
/// every `TmuxError::ServerGone` arm shares.
pub(crate) fn no_server() -> ExitCode {
    eprintln!("tma: no tmux server running");
    ExitCode::FAILURE
}

/// Load the manifest set or print the standard error and yield a FAILURE exit code. Only a
/// whole-set failure exits; a single unusable user manifest rides on [`LoadedSet::failures`] for
/// the caller to report. `tma doctor` wants that list, so it takes this rather than the shape below.
pub(crate) fn load_manifest_set_or_exit(
    manifest_dir: Option<&Path>,
    agents: &[AgentConfig],
) -> Result<LoadedSet, ExitCode> {
    manifests::load(manifest_dir, agents).map_err(|err| {
        eprintln!("tma: manifest load failed: {err}");
        ExitCode::FAILURE
    })
}

/// The load-or-exit shape every surface command shares: skipped user manifests are warned about on
/// stderr (never stdout, so `--json` output stays parseable) and the rest of the set is returned.
pub(crate) fn load_manifests_or_exit(
    manifest_dir: Option<&Path>,
    agents: &[AgentConfig],
) -> Result<Vec<LoadedManifest>, ExitCode> {
    let set = load_manifest_set_or_exit(manifest_dir, agents)?;
    warn_manifest_failures(&set.failures);
    Ok(set.manifests)
}

/// One stderr line per skipped user manifest, naming the file and its parse error.
pub(crate) fn warn_manifest_failures(failures: &[ManifestFailure]) {
    for f in failures {
        eprintln!("tma: skipping manifest {}: {}", f.path.display(), f.error);
    }
}

// --- the daemon-management seam ---------------------------------------------------

/// Which daemon verb a chained step is asking `main` to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonMode {
    /// `tma daemon --ensure`: start one if none is running.
    Ensure,
    /// `tma daemon --restart`: replace whatever is running with this build.
    Restart,
}

/// The daemon launcher `main` injects into the chained setup steps (`init`, `install-hooks`), with
/// the target server and config already bound. Tier 3 is reachable only from the `tma daemon`
/// dispatch site (tests/tier_boundary.rs), so a step that needs a daemon started calls this
/// instead of naming the daemon crate.
pub(crate) type DaemonLauncher = Box<dyn Fn(DaemonMode) -> ExitCode>;

/// Offer to replace a resident daemon whose build differs from this binary's, under the same
/// diff-and-confirm discipline as the config writes around it: say what is there, what it costs,
/// and change nothing without a `y` (or `--yes`).
///
/// A resident daemon keeps the detection code it started with, so hooks that were just wired or
/// rewired reach whatever build is already resident — which is why the two steps that rewire them
/// are where this is offered. Silent when there is no server, no daemon, a matching build, or a
/// lock file predating version recording (nothing to compare).
///
/// Returns `false` only when a CONFIRMED restart failed. Declining is not a failure: everything the
/// caller wrote is already in place, and the daemon is strictly additive.
pub(crate) fn offer_daemon_restart(
    tmux: &tma_tmux::tmux::Tmux,
    assume_yes: bool,
    launch: &DaemonLauncher,
) -> bool {
    use tma_runtime::ipc;
    let Some(status) = ipc::daemon_status(tmux) else {
        return true; // no server ⇒ no daemon to speak of
    };
    // `DaemonStatus` reports a version only for a daemon that answered, so this is both gates.
    let Some(running) = status.version.as_deref() else {
        return true; // nothing running, or a lock file that records no version
    };
    if running == ipc::VERSION {
        return true;
    }
    let mine = ipc::VERSION;
    println!(
        "\ntma: the daemon running for this server is build {running}, but this is {mine}. A \
         daemon keeps the\n     detection code it started with, so it will not pick up this build \
         (`tma reload` re-reads\n     config and manifests, not the binary)."
    );
    println!(
        "     Proposed: stop that daemon and start {mine} in its place. Nothing is lost while it \
         is\n     down — a hook that cannot reach a daemon stamps the pane itself."
    );
    if !assume_yes && !crate::install::confirm() {
        println!("tma: left the running daemon alone; `tma daemon --restart` when you are ready");
        return true;
    }
    matches!(launch(DaemonMode::Restart), ExitCode::SUCCESS)
}
