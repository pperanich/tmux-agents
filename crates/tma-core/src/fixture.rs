//! Fixture format and loader for captured-screen test data.
//!
//! Test support only, gated behind the `fixtures` feature so it stays out of consumer
//! builds. Format: a `# key: value` header, a `---` separator, then the raw capture text
//! verbatim — the exact shape `tma debug capture` emits, so a captured pane becomes a
//! fixture with no reformatting. [`Fixture::parse`] is pure; the filesystem helpers
//! ([`Fixture::load`], [`load_dir`]) read files only because this module is test support.

use std::path::{Path, PathBuf};

use crate::state::AgentState;

/// A parsed fixture: header metadata plus the raw capture body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    pub agent: String,
    pub state: AgentState,
    pub title: String,
    pub command: String,
    pub pid: u32,
    /// The header value verbatim: the parser scales nothing. The shipped fixtures carry epoch
    /// seconds, and the manifest suites only ever offset from it, so it never meets a real clock.
    pub captured_at: u64,
    /// Raw capture text, preserved byte-for-byte after the `---` separator.
    pub capture: String,
}

/// Errors parsing the fixture text format.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FixtureError {
    #[error("missing '---' separator between header and capture body")]
    MissingSeparator,
    #[error("header line is not '# key: value': {0:?}")]
    BadHeaderLine(String),
    #[error("missing required header key: {0}")]
    MissingKey(&'static str),
    #[error("header key {key} has invalid value {value:?}: {reason}")]
    BadValue {
        key: &'static str,
        value: String,
        reason: String,
    },
    #[error("unknown header key: {0:?}")]
    UnknownKey(String),
}

impl Fixture {
    /// Parse a fixture from its text form. Pure: no filesystem, no clock.
    pub fn parse(text: &str) -> Result<Fixture, FixtureError> {
        let (header_src, capture) = split_header(text).ok_or(FixtureError::MissingSeparator)?;

        let mut agent = None;
        let mut state = None;
        let mut title = None;
        let mut command = None;
        let mut pid = None;
        let mut captured_at = None;

        for line in header_src.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let content = line
                .strip_prefix('#')
                .ok_or_else(|| FixtureError::BadHeaderLine(line.to_string()))?;
            let (key, value) = content
                .split_once(':')
                .ok_or_else(|| FixtureError::BadHeaderLine(line.to_string()))?;
            let (key, value) = (key.trim(), value.trim());
            match key {
                "agent" => agent = Some(value.to_string()),
                "state" => {
                    let parsed =
                        value
                            .parse::<AgentState>()
                            .map_err(|e| FixtureError::BadValue {
                                key: "state",
                                value: value.to_string(),
                                reason: e.to_string(),
                            })?;
                    state = Some(parsed);
                }
                "title" => title = Some(value.to_string()),
                "command" => command = Some(value.to_string()),
                "pid" => {
                    let parsed = value.parse::<u32>().map_err(|e| FixtureError::BadValue {
                        key: "pid",
                        value: value.to_string(),
                        reason: e.to_string(),
                    })?;
                    pid = Some(parsed);
                }
                "captured_at" => {
                    let parsed = value.parse::<u64>().map_err(|e| FixtureError::BadValue {
                        key: "captured_at",
                        value: value.to_string(),
                        reason: e.to_string(),
                    })?;
                    captured_at = Some(parsed);
                }
                other => return Err(FixtureError::UnknownKey(other.to_string())),
            }
        }

        Ok(Fixture {
            agent: agent.ok_or(FixtureError::MissingKey("agent"))?,
            state: state.ok_or(FixtureError::MissingKey("state"))?,
            title: title.ok_or(FixtureError::MissingKey("title"))?,
            command: command.ok_or(FixtureError::MissingKey("command"))?,
            pid: pid.ok_or(FixtureError::MissingKey("pid"))?,
            captured_at: captured_at.ok_or(FixtureError::MissingKey("captured_at"))?,
            capture,
        })
    }

    /// Render back to the text form. `parse(&f.to_text()) == f` for any `f`.
    pub fn to_text(&self) -> String {
        format!(
            "# agent: {}\n# state: {}\n# title: {}\n# command: {}\n# pid: {}\n# captured_at: {}\n---\n{}",
            self.agent,
            self.state,
            self.title,
            self.command,
            self.pid,
            self.captured_at,
            self.capture,
        )
    }

    /// Read and parse a fixture file. Filesystem access — test support only.
    pub fn load(path: &Path) -> Result<Fixture, FixtureLoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| FixtureLoadError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Fixture::parse(&text).map_err(|source| FixtureLoadError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}

/// Split header text (before `---`) from the raw capture body (after it), preserving
/// the body byte-for-byte. Returns `None` when there is no `---` separator line.
fn split_header(text: &str) -> Option<(&str, String)> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if trimmed == "---" {
            let header = &text[..offset];
            let body = &text[offset + line.len()..];
            return Some((header, body.to_string()));
        }
        offset += line.len();
    }
    None
}

/// Filesystem/parse errors from the loader helpers.
#[derive(Debug, thiserror::Error)]
pub enum FixtureLoadError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: FixtureError,
    },
}

/// Load every `*.txt` fixture in `dir`, sorted by filename for determinism.
pub fn load_dir(dir: &Path) -> Result<Vec<(PathBuf, Fixture)>, FixtureLoadError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| FixtureLoadError::Io {
            path: dir.display().to_string(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    paths.sort();

    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let fixture = Fixture::load(&path)?;
        out.push((path, fixture));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Fixture {
        Fixture {
            agent: "claude".to_string(),
            state: AgentState::Blocked,
            title: "✳ Redact my resume".to_string(),
            command: "claude".to_string(),
            pid: 4242,
            captured_at: 1_721_500_000,
            capture: "╭────────╮\n│ ❯      │\n╰────────╯\n".to_string(),
        }
    }

    #[test]
    fn round_trips_through_text() {
        let f = sample();
        assert_eq!(Fixture::parse(&f.to_text()).unwrap(), f);
    }

    #[test]
    fn parses_canonical_text() {
        let text = "# agent: codex\n# state: working\n# title: t\n# command: codex\n# pid: 7\n# captured_at: 100\n---\nbody line 1\nbody line 2\n";
        let f = Fixture::parse(text).unwrap();
        assert_eq!(f.agent, "codex");
        assert_eq!(f.state, AgentState::Working);
        assert_eq!(f.pid, 7);
        assert_eq!(f.captured_at, 100);
        assert_eq!(f.capture, "body line 1\nbody line 2\n");
    }

    #[test]
    fn preserves_empty_body() {
        let text = "# agent: x\n# state: idle\n# title: t\n# command: x\n# pid: 1\n# captured_at: 1\n---\n";
        let f = Fixture::parse(text).unwrap();
        assert_eq!(f.capture, "");
    }

    #[test]
    fn missing_separator_errors() {
        let text = "# agent: x\n# state: idle\n";
        assert_eq!(Fixture::parse(text), Err(FixtureError::MissingSeparator));
    }

    #[test]
    fn missing_key_errors() {
        let text = "# agent: x\n# state: idle\n# title: t\n# command: x\n# pid: 1\n---\nbody";
        assert_eq!(
            Fixture::parse(text),
            Err(FixtureError::MissingKey("captured_at"))
        );
    }

    #[test]
    fn unknown_key_errors() {
        let text = "# agent: x\n# surprise: y\n---\nbody";
        assert_eq!(
            Fixture::parse(text),
            Err(FixtureError::UnknownKey("surprise".to_string()))
        );
    }

    #[test]
    fn bad_state_value_errors() {
        let text =
            "# agent: x\n# state: running\n# title: t\n# command: x\n# pid: 1\n# captured_at: 1\n---\nb";
        assert!(matches!(
            Fixture::parse(text),
            Err(FixtureError::BadValue { key: "state", .. })
        ));
    }

    /// Every fixture bundled under `tma-core/fixtures/` MUST parse. Real rule fixtures
    /// arrive with the agent manifests; this guards them against format drift.
    #[test]
    fn bundled_fixtures_all_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let loaded = load_dir(&dir).expect("fixtures dir loads");
        for (path, _fixture) in &loaded {
            // load_dir already parsed; reaching here means it parsed. Assert we saw the
            // synthetic smoke fixture so an empty dir can't make this test vacuous.
            let _ = path;
        }
        assert!(
            loaded
                .iter()
                .any(|(p, _)| p.file_name().is_some_and(|n| n == "_harness_smoke.txt")),
            "expected the harness smoke fixture to be present"
        );
    }
}
