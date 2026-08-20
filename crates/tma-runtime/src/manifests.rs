//! Runtime manifest loading. Bundled manifests are embedded at compile time via `include_str!`
//! from `tma-core/manifests/`; user overrides shadow them by stem from `~/.config/tma/agents/`;
//! a `--manifest-dir` debug flag loads an isolated set for tests.
//!
//! Each loaded manifest is paired with its compiled [`RuleEngine`] so regex compilation
//! errors surface once, at load, naming the file and rule.
//!
//! User manifests load per file: one broken file is skipped and reported on [`LoadedSet::failures`]
//! rather than failing the whole set, so a typo in `~/.config/tma/agents/` cannot take the bundled
//! corpus down with it. A bundled manifest that fails is still fatal — that is a build bug.

use std::path::{Path, PathBuf};

use tma_core::{Manifest, RuleEngine};

use crate::config::AgentConfig;

/// The bundled manifest corpus, embedded at compile time.
const BUNDLED: &[(&str, &str)] = &[
    (
        "claude",
        include_str!("../../tma-core/manifests/claude.toml"),
    ),
    (
        "opencode",
        include_str!("../../tma-core/manifests/opencode.toml"),
    ),
    ("codex", include_str!("../../tma-core/manifests/codex.toml")),
    (
        "gemini",
        include_str!("../../tma-core/manifests/gemini.toml"),
    ),
    (
        "cursor",
        include_str!("../../tma-core/manifests/cursor.toml"),
    ),
    ("pi", include_str!("../../tma-core/manifests/pi.toml")),
];

// ---- hook-event vocabulary ---------------------------------------------------------
//
// Which hook events an agent wires is agent description, so it lives next to manifest loading, not
// in `event`. The installer reads [`hook_events`]; the bridge consumes the same names.

/// Normative subagent bookkeeping events: they carry no state claim, only append/remove the firing
/// session id in `@agent_subagents`. Recognized by name (the claim schema has no subagent variant).
/// Both names are verified against the Claude Code hooks reference, so `@agent_subagents` is
/// populated by the real START event and the subagent guard is live.
pub const SUBAGENT_START: &str = "SubagentStart";
pub const SUBAGENT_STOP: &str = "SubagentStop";

/// The events the parser is authored and tested for (Claude Code's seven mapped plus the two subagent
/// events). The drift test asserts this equals [`hook_events`], so adding one without coverage fails.
pub const CLAUDE_PARSER_COVERAGE: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    SUBAGENT_START,
    SUBAGENT_STOP,
];

/// The complete set of hook event names tma wires for `manifest`: every mapped event plus the
/// subagent events. Single source of truth for the installer and the parser-coverage drift test.
pub fn hook_events(manifest: &Manifest) -> Vec<String> {
    let mut ev: Vec<String> = Vec::new();
    // A hookless manifest wires nothing: return empty so the installer refuses rather than emitting
    // bare subagent hooks into a foreign config (they belong only to hook-capable agents, appended
    // inside this arm).
    let Some(h) = &manifest.hooks else {
        return ev;
    };
    for m in &h.map {
        if !ev.iter().any(|e| e == &m.event) {
            ev.push(m.event.clone());
        }
    }
    for s in [SUBAGENT_START, SUBAGENT_STOP] {
        if !ev.iter().any(|e| e == s) {
            ev.push(s.to_string());
        }
    }
    ev
}

/// A manifest plus its identity: the stem is the agent name used in output and identity.
pub struct LoadedManifest {
    pub name: String,
    pub manifest: Manifest,
    pub engine: RuleEngine,
}

/// Errors loading the manifest set.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Manifest(#[from] tma_core::ManifestError),
    #[error("{file}: {source}")]
    Engine {
        file: String,
        #[source]
        source: tma_core::EngineError,
    },
    #[error("cannot read manifest dir {dir}: {source}")]
    Dir {
        dir: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read manifest {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: std::io::Error,
    },
}

impl LoadedManifest {
    fn build(name: &str, source: &str, toml_src: &str) -> Result<LoadedManifest, LoadError> {
        let manifest = Manifest::parse(toml_src, source)?;
        let engine = RuleEngine::build(&manifest).map_err(|e| LoadError::Engine {
            file: source.to_string(),
            source: e,
        })?;
        Ok(LoadedManifest {
            name: name.to_string(),
            manifest,
            engine,
        })
    }
}

/// A user manifest that could not be loaded, so the rest of the set still could.
pub struct ManifestFailure {
    pub path: PathBuf,
    pub error: LoadError,
}

/// The effective manifest set plus the user files skipped building it. Callers that only need the
/// agents take [`LoadedSet::manifests`]; the surfaces warn on `failures`, `tma doctor` lists them,
/// and the hook path ignores them (a hook must stay quiet).
pub struct LoadedSet {
    pub manifests: Vec<LoadedManifest>,
    pub failures: Vec<ManifestFailure>,
}

/// Load the effective manifest set. `manifest_dir` loads exactly that dir (test isolation);
/// otherwise the bundled corpus overlaid with user overrides from `~/.config/tma/agents/` (shadowing
/// by stem), then the `[[agent]]` config (drop disabled, extend `process_names`). `Err` is reserved
/// for whole-set failures (an unreadable `--manifest-dir`, a broken bundled manifest); a single bad
/// user file lands on [`LoadedSet::failures`].
pub fn load(manifest_dir: Option<&Path>, agents: &[AgentConfig]) -> Result<LoadedSet, LoadError> {
    let mut set = load_raw(manifest_dir)?;
    apply_agent_config(&mut set.manifests, agents);
    Ok(set)
}

/// Load the manifest set before the `[[agent]]` config is applied.
fn load_raw(manifest_dir: Option<&Path>) -> Result<LoadedSet, LoadError> {
    if let Some(dir) = manifest_dir {
        let (manifests, failures) = load_dir(dir)?;
        return Ok(LoadedSet {
            manifests,
            failures,
        });
    }

    let mut loaded: Vec<LoadedManifest> = Vec::new();
    for (name, toml_src) in BUNDLED {
        // A bundled manifest that fails to build is a build bug, not user input: stay fatal.
        loaded.push(LoadedManifest::build(
            name,
            &format!("<bundled>/{name}.toml"),
            toml_src,
        )?);
    }

    let mut failures = Vec::new();
    if let Some(user_dir) = user_agents_dir() {
        if user_dir.is_dir() {
            let (user, failed) = load_dir(&user_dir)?;
            failures = failed;
            for lm in user {
                match loaded.iter_mut().find(|e| e.name == lm.name) {
                    Some(existing) => *existing = lm, // user override shadows bundled
                    None => loaded.push(lm),
                }
            }
        }
    }
    Ok(LoadedSet {
        manifests: loaded,
        failures,
    })
}

/// Apply the `[[agent]]` config: `enabled = false` drops the named manifest; extra `process_names`
/// extend an enabled manifest's identity match (deduped, no engine rebuild since identity does not
/// affect screen rules). Extending an agent with no loaded manifest is a no-op (the map extends a
/// match, it does not synthesize one).
fn apply_agent_config(loaded: &mut Vec<LoadedManifest>, agents: &[AgentConfig]) {
    for a in agents {
        if !a.enabled {
            loaded.retain(|m| m.name != a.name);
            continue;
        }
        if let Some(m) = loaded.iter_mut().find(|m| m.name == a.name) {
            for pn in &a.process_names {
                if !m.manifest.identity.process_names.contains(pn) {
                    m.manifest.identity.process_names.push(pn.clone());
                }
            }
        }
    }
}

/// Read every `*.toml` in `dir`, per file: the ones that build, and the ones that did not. Only an
/// unreadable *directory* is an error — a single bad file must not cost the caller the whole set.
fn load_dir(dir: &Path) -> Result<(Vec<LoadedManifest>, Vec<ManifestFailure>), LoadError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| LoadError::Dir {
            dir: dir.display().to_string(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    entries.sort();

    let mut out = Vec::with_capacity(entries.len());
    let mut failures = Vec::new();
    for path in entries {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let built = std::fs::read_to_string(&path)
            .map_err(|source| LoadError::Read {
                file: path.display().to_string(),
                source,
            })
            .and_then(|toml_src| {
                LoadedManifest::build(&name, &path.display().to_string(), &toml_src)
            });
        match built {
            Ok(lm) => out.push(lm),
            Err(error) => failures.push(ManifestFailure { path, error }),
        }
    }
    Ok((out, failures))
}

fn user_agents_dir() -> Option<PathBuf> {
    // XDG_CONFIG_HOME, else ~/.config (the config *file* is deferred, not the standard dir).
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("tma/agents"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".config/tma/agents"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"good\"]\n\
                        [capture]\nvisible = [\"working\"]\n";

    /// A scratch dir under the temp dir, removed when the test ends.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let path = std::env::temp_dir().join(format!(
                "tma-manifests-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_broken_file_is_skipped_and_reported_while_the_rest_load() {
        let dir = Dir::new("mixed");
        dir.write("good.toml", GOOD);
        dir.write("broken.toml", "this is not = = toml\n");
        let (loaded, failures) = load_dir(&dir.0).expect("the directory itself is readable");
        assert_eq!(loaded.len(), 1, "the good manifest still loads");
        assert_eq!(loaded[0].name, "good");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].path.ends_with("broken.toml"));
    }

    #[test]
    fn an_all_broken_dir_loads_empty_rather_than_erroring() {
        // The caller gets a usable (here empty) set plus the diagnosis, never an Err it must
        // translate into a dead surface.
        let dir = Dir::new("broken");
        dir.write("broken.toml", "[identity]\nprocess_names = 7\n");
        let set = load(Some(&dir.0), &[]).expect("a readable dir is not an error");
        assert!(set.manifests.is_empty());
        assert_eq!(set.failures.len(), 1);
    }

    #[test]
    fn the_bundled_corpus_builds() {
        // Bundled failures are fatal, so this is the guard that the shipped six actually compile.
        let set = load_raw(None).expect("the bundled corpus must build");
        for (name, _) in BUNDLED {
            assert!(
                set.manifests.iter().any(|m| &m.name == name),
                "bundled manifest {name} is missing from the loaded set"
            );
        }
    }

    /// Every bundled `state = "idle"` hook entry is a turn end, and nothing else is. The flag is
    /// what lets a SECOND completion re-raise a cleared done marker (an idle→idle edge the fold
    /// cannot see), so a manifest that forgets it silently loses that pane's re-signal; and an
    /// event that merely OBSERVES idleness carrying it would make the marker unclearable.
    #[test]
    fn every_bundled_turn_end_is_an_idle_claim_and_every_idle_claim_is_a_turn_end() {
        use tma_core::evidence::Claim;
        use tma_core::AgentState;
        let set = load_raw(None).expect("the bundled corpus must build");
        let mut turn_ends = 0;
        for lm in &set.manifests {
            let Some(hooks) = &lm.manifest.hooks else {
                continue;
            };
            for entry in &hooks.map {
                let idle = matches!(&entry.claim, Claim::State(sc) if sc.state == AgentState::Idle);
                assert_eq!(
                    entry.turn_end, idle,
                    "{}: `{}` claims idle={idle} but turn_end={}",
                    lm.name, entry.event, entry.turn_end
                );
                turn_ends += usize::from(entry.turn_end);
            }
        }
        // Seven, not six: codex reports one turn end on two channels (`Stop` in hooks.json and
        // `notify` in config.toml), which is exactly why the intake records a turn only when the
        // marker was down.
        assert_eq!(turn_ends, 7, "one turn-end event per agent, codex two");
    }

    #[test]
    fn an_unreadable_manifest_dir_is_still_a_whole_set_error() {
        // Directory-level failure is not a per-file skip: the caller asked for a dir that is gone.
        let missing = std::env::temp_dir().join("tma-manifests-no-such-dir-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(matches!(
            load(Some(&missing), &[]),
            Err(LoadError::Dir { .. })
        ));
    }
}
