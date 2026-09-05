//! Pane to transcript file.
//!
//! There are two paths, and the cheap one is new. Since 0.5.12 tma stamps `@agent_transcript` with
//! the path the agent's own hook payload named, so for claude, codex, gemini and cursor discovery
//! is a `stat` on a string tma already has. The fallback walks the store's layout from the session
//! id, which is the codex tail's `discover_rollout` generalized to four stores; it exists for a pane
//! detected from the screen alone, for a session that predates the stamp, and for pi, which
//! publishes no path.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::model::Store;
use crate::{Refusal, Source};

/// What the reader knows about a pane. Everything here comes off the pane's own options, so
/// discovery never has to ask tmux anything itself.
#[derive(Debug, Clone, Default)]
pub struct PaneFacts {
    /// `@agent_name`.
    pub agent: String,
    /// `@agent_session`.
    pub session: Option<String>,
    /// `@agent_transcript`, the path the agent's hook payload named.
    pub transcript: Option<String>,
    /// The pane's working directory, which three of the four stores slug into a directory name.
    pub cwd: Option<PathBuf>,
}

/// Where each store's tree lives. Held explicitly rather than read from the environment inside the
/// walk, so a test points at a scratch tree and can never reach a real session.
#[derive(Debug, Clone)]
pub struct StoreRoots {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub gemini: PathBuf,
    pub pi: PathBuf,
}

impl StoreRoots {
    /// The roots as the agents themselves resolve them: `$CODEX_HOME` when set, `$HOME/.<agent>`
    /// otherwise. `None` when `$HOME` is unset and nothing can be resolved.
    pub fn from_env() -> Option<StoreRoots> {
        let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
        let codex = std::env::var_os("CODEX_HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Some(StoreRoots {
            claude: home.join(".claude"),
            codex,
            gemini: home.join(".gemini"),
            pi: home.join(".pi"),
        })
    }

    /// Every root under one directory, which is what the tests use.
    pub fn under(root: &Path) -> StoreRoots {
        StoreRoots {
            claude: root.join(".claude"),
            codex: root.join(".codex"),
            gemini: root.join(".gemini"),
            pi: root.join(".pi"),
        }
    }
}

/// Resolve a pane to the file its conversation lives in.
///
/// The refusals are as informative as the successes: a cursor-agent pane is refused because its
/// store is too thin to render, and an OpenCode pane because its store is a database. Neither is a
/// missing file, and reporting them as one would send someone looking for a path that never existed.
pub fn discover(facts: &PaneFacts, roots: &StoreRoots) -> Result<Source, Refusal> {
    let store = Store::from_agent(&facts.agent).ok_or(Refusal::NoTranscript)?;
    if !store.is_readable() {
        return Err(Refusal::for_store(store));
    }
    if let Some(path) = facts.transcript.as_ref().map(PathBuf::from) {
        if path.is_file() {
            return Ok(Source { store, path });
        }
    }
    let session = facts.session.as_deref().ok_or(Refusal::NoTranscript)?;
    let found = match store {
        Store::Claude => claude_session(&roots.claude, session, facts.cwd.as_deref()),
        Store::Codex => codex_rollout(&roots.codex, session),
        Store::Gemini => gemini_chat(&roots.gemini, session),
        Store::Pi => pi_session(&roots.pi, session, facts.cwd.as_deref()),
        Store::Cursor | Store::OpenCode => None,
    };
    found
        .map(|path| Source { store, path })
        .ok_or(Refusal::NoTranscript)
}

/// The subagent transcripts beside a claude session, as `(child id, path)`. Claude 2.1.2xx writes
/// each nested agent to its own file rather than inlining it, so a `SubagentRef` in the parent
/// window is a pointer into this list.
pub fn subagents(source: &Source) -> Vec<(String, PathBuf)> {
    if source.store != Store::Claude {
        return Vec::new();
    }
    let Some(dir) = subagent_dir(&source.path) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let id = path
                .file_name()
                .and_then(|n| n.to_str())?
                .strip_prefix("agent-")?
                .strip_suffix(".jsonl")?
                .to_string();
            Some((id, path))
        })
        .collect();
    out.sort();
    out
}

/// One subagent's transcript, served as its own session. The exact `agent-<child_id>.jsonl` wins;
/// a file whose id merely contains the `SubagentRef`'s child id is the fallback, because the tool
/// call's id and the child file's id agree in shape but not always in full.
pub fn child_source(source: &Source, child_id: &str) -> Result<Source, Refusal> {
    let children = subagents(source);
    let hit = children
        .iter()
        .find(|(id, _)| id == child_id)
        .or_else(|| children.iter().find(|(id, _)| id.contains(child_id)))
        .ok_or(Refusal::NoTranscript)?;
    Ok(Source {
        store: Store::Claude,
        path: hit.1.clone(),
    })
}

/// `<session>.jsonl` sits beside a `<session>/subagents/` directory of the same name.
fn subagent_dir(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    Some(path.parent()?.join(stem).join("subagents"))
}

/// `~/.claude/projects/<cwd-slug>/<session>.jsonl`. The slug is the cwd with every separator turned
/// into a dash, so a known cwd is one `stat`; without one, the project directories are scanned.
fn claude_session(root: &Path, session: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let projects = root.join("projects");
    let name = format!("{session}.jsonl");
    if let Some(cwd) = cwd {
        let direct = projects.join(claude_slug(cwd)).join(&name);
        if direct.is_file() {
            return Some(direct);
        }
    }
    subdirs(&projects)
        .into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// Claude's project-directory slug: the absolute path with `/` and `.` replaced by `-`.
fn claude_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<iso>-<session>.jsonl`. The date is unknown, so every
/// dated leaf is scanned and the newest-mtime match wins: a resumed session leaves more than one.
fn codex_rollout(root: &Path, session: &str) -> Option<PathBuf> {
    let sessions = root.join("sessions");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for year in subdirs_desc(&sessions) {
        for month in subdirs_desc(&year) {
            for day in subdirs_desc(&month) {
                let Ok(entries) = fs::read_dir(&day) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let matches = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.starts_with("rollout-") && n.ends_with(".jsonl") && n.contains(session)
                    });
                    if !matches {
                        continue;
                    }
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    if best.as_ref().is_none_or(|(bt, _)| mtime >= *bt) {
                        best = Some((mtime, path));
                    }
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// `~/.gemini/tmp/<projectHash>/chats/session-<iso>-<short>.jsonl`. The weak one: the directory is a
/// hash of the project rather than a slug of it, and the filename holds only an eight-hex prefix of
/// the session id, so the id has to be read out of each header. Bounded by reading one line each.
fn gemini_chat(root: &Path, session: &str) -> Option<PathBuf> {
    let short = session.get(..8).unwrap_or(session);
    let mut fallback = None;
    for project in subdirs(&root.join("tmp")) {
        let Ok(entries) = fs::read_dir(project.join("chats")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("session-") || !name.ends_with(".jsonl") {
                continue;
            }
            if header_session_id(&path).as_deref() == Some(session) {
                return Some(path);
            }
            // The filename's short id is weaker evidence than the header, so it only stands in
            // when no header matched anywhere.
            if fallback.is_none() && name.contains(short) {
                fallback = Some(path);
            }
        }
    }
    fallback
}

/// The `sessionId` from a gemini chat file's header record, reading only the first line.
fn header_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    let value = crate::json::parse(line.trim()).ok()?;
    value
        .get("sessionId")
        .and_then(crate::json::Value::as_str)
        .map(str::to_string)
}

/// `~/.pi/agent/sessions/--<cwd-slug>--/<iso>_<session>.jsonl`.
fn pi_session(root: &Path, session: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let sessions = root.join("agent").join("sessions");
    let suffix = format!("_{session}.jsonl");
    let mut dirs = Vec::new();
    if let Some(cwd) = cwd {
        dirs.push(sessions.join(format!("--{}--", pi_slug(cwd))));
    }
    dirs.extend(subdirs(&sessions));
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

/// pi's directory slug: the absolute path with separators turned into dashes, wrapped in `--`.
fn pi_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_matches('/')
        .replace('/', "-")
        .to_string()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(rd) => {
            let mut v: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            v.sort();
            v
        }
        Err(_) => Vec::new(),
    }
}

/// Subdirectories newest-date first, so a dated tree is walked from today backwards.
fn subdirs_desc(dir: &Path) -> Vec<PathBuf> {
    let mut v = subdirs(dir);
    v.reverse();
    v
}
