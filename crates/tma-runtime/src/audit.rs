//! The JSONL audit sink both opt-in records write through: `[notify] log` (one line per fired
//! notification, appended where both fire paths meet in [`crate::notify::fire`]) and `[act] log`
//! (one line per broker fire, [`crate::broker::audit`]). One writer so the two files cannot drift on
//! the rule that matters, the `0600` create mode. Best-effort throughout: a log that cannot be
//! written must never fail or delay a hook, and must never turn a delivered action into a failure.

use std::path::{Path, PathBuf};

/// Append one line to `path`, creating parent directories and the file as needed. `O_APPEND`, so two
/// concurrent firers (a hook and the daemon) interleave whole lines rather than overwriting.
pub(crate) fn append(path: &Path, line: &str) {
    let path = expand_tilde(path);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // 0600, matching the daemon socket's discipline in `docs/explanation/security-model.md`. Without
    // an explicit mode the file lands at `0666 & ~umask`: 0664 under the common `umask 002`, and
    // world-WRITABLE under `umask 000`. `mode` applies only when this call creates the file, so an
    // existing log keeps whatever the user set.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        use std::io::Write;
        // One write of a line that is far under PIPE_BUF-sized atomicity concerns for a regular
        // file; appended under O_APPEND so concurrent writers do not clobber each other's offsets.
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// Expand a leading `~` against `$HOME`. Config paths are user-written strings, and a literal `~`
/// directory is never what someone meant; anything else is returned unchanged.
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return path.to_path_buf();
    };
    match rest.strip_prefix('/') {
        Some(tail) => PathBuf::from(home).join(tail),
        // A bare `~` is HOME itself; `~other` is another user's home, which we do not resolve.
        None if rest.is_empty() => PathBuf::from(home),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_only_for_this_user() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            eprintln!("skipping: no HOME");
            return;
        }
        assert_eq!(
            expand_tilde(Path::new("~/state/tma.jsonl")),
            PathBuf::from(&home).join("state/tma.jsonl")
        );
        assert_eq!(expand_tilde(Path::new("~")), PathBuf::from(&home));
        // Another user's home is left alone rather than guessed at.
        assert_eq!(
            expand_tilde(Path::new("~someone/x")),
            PathBuf::from("~someone/x")
        );
        assert_eq!(
            expand_tilde(Path::new("/var/log/tma.jsonl")),
            PathBuf::from("/var/log/tma.jsonl")
        );
    }

    #[test]
    fn append_creates_the_parent_and_adds_lines() {
        let dir = std::env::temp_dir().join(format!("tma-audit-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/fires.jsonl");
        append(&path, r#"{"a":1}"#);
        append(&path, r#"{"a":2}"#);
        let body = std::fs::read_to_string(&path).expect("the log was created");
        assert_eq!(body, "{\"a\":1}\n{\"a\":2}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mode is the whole point of the explicit `mode(0o600)`: without it the file lands at
    /// `0666 & ~umask`, which is group-readable under the common `umask 002` and world-WRITABLE
    /// under `umask 000`. Both audit records hold a fleet's activity, so this is pinned.
    #[cfg(unix)]
    #[test]
    fn a_created_log_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tma-audit-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("fires.jsonl");
        append(&path, "{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the log must not be readable by anyone else"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
