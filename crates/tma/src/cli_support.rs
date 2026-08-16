//! Shared bin plumbing: the "no tmux server" error and the load-manifests-or-exit shape, each
//! reused verbatim across the surface subcommands so their message + exit code cannot drift.

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
