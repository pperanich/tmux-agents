//! Per-cwd repo/branch resolution: the git metadata the grouping and
//! branch-label surfaces render. One bounded `git -C <cwd> rev-parse --abbrev-ref
//! HEAD --git-common-dir --git-dir` per unique working directory, memoized with a
//! ~5 s TTL so no per-frame path spawns git; failure degrades to absent labels,
//! never a surfaced error. Only [`annotate_rows`] resolves — the display/serialize
//! call sites call it, the poll/jump/act/capture paths never do.
//!
//! The rev-parse output handling and relative-git-path resolution are adapted (MIT)
//! from tmux-agent-sidebar's `src/group.rs`
//! (<https://github.com/hiroppy/tmux-agent-sidebar>, Copyright (c) 2026 hiroppy): the
//! single three-value rev-parse call, group key = parent of `--git-common-dir` (so
//! linked worktrees roll up under their origin repo), and worktree detection by
//! comparing the resolved `--git-common-dir` against the resolved `--git-dir`.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tma_core::{AgentRow, RepoLabel};

/// The git program name. Held as a const so the one spawn site is obvious; the
/// runner takes it as a parameter, which is also the test seam for the NotFound path.
const GIT_PROGRAM: &str = "git";

/// How long a resolved (or unresolved) cwd stays memoized. Bounds the staleness of a
/// branch switch or a `git worktree add` against the cost of re-spawning git.
const MEMO_TTL: Duration = Duration::from_secs(5);

/// Wall-clock bound on the one rev-parse call. A hung git (a network filesystem, a
/// wedged credential helper) must not stall a surface refresh.
const DEADLINE: Duration = Duration::from_secs(3);

/// Resolved git metadata for one working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// Basename of the origin repo root (parent of `--git-common-dir`); the grouping key,
    /// so a linked worktree rolls up under its main checkout.
    pub repo_name: String,
    /// `git rev-parse --abbrev-ref HEAD`; the literal `HEAD` when detached.
    pub branch: String,
    /// Whether this checkout is a linked worktree (git-dir differs from common-dir).
    pub is_worktree: bool,
}

/// Process-wide "the git binary is missing" latch. Once a spawn fails with
/// `NotFound`, every later resolve returns `None` without spawning again — a machine
/// without git pays one failed spawn, not one per cwd per refresh.
static GIT_MISSING: AtomicBool = AtomicBool::new(false);

/// The per-cwd memo: `LazyLock<Mutex<...>>` process-local (the `CONTEXT_TAIL`
/// precedent in `cycle.rs`), reached as a static from the one-shot surfaces and the
/// subscribe render closure. `None` values are cached too, so a non-git pane does not
/// re-spawn git every frame.
static MEMO: LazyLock<Mutex<HashMap<String, MemoEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct MemoEntry {
    at: Instant,
    info: Option<RepoInfo>,
}

/// Resolve `cwd` to its repo metadata, memoized with the TTL. `None` for an empty
/// cwd, a non-repo, an unborn or bare repo, a git-binary-missing machine, or any
/// spawn/deadline failure.
pub fn resolve(cwd: &str) -> Option<RepoInfo> {
    if cwd.is_empty() {
        return None;
    }
    let now = Instant::now();
    // The lock is never held across the git spawn (up to DEADLINE): check, drop, resolve, re-lock
    // to insert. A cold-cwd race between two threads costs one duplicate resolve, not a 3 s stall.
    if let Some(hit) = memo_get(
        &mut MEMO.lock().unwrap_or_else(|e| e.into_inner()),
        cwd,
        now,
        MEMO_TTL,
    ) {
        return hit;
    }
    let info = resolve_uncached(cwd);
    memo_put(
        &mut MEMO.lock().unwrap_or_else(|e| e.into_inner()),
        cwd,
        now,
        info.clone(),
    );
    info
}

/// Fill each row's `repo` label from its `cwd` via [`resolve`]. `Some` (name/branch/worktree) for a
/// resolved checkout, `None` exactly when the repo is unresolved; `worktree` is `false` for a main
/// checkout, `true` for a linked worktree.
pub fn annotate_rows(rows: &mut [AgentRow]) {
    for row in rows.iter_mut() {
        row.repo = row.cwd.as_deref().and_then(resolve).map(|info| RepoLabel {
            name: info.repo_name,
            branch: info.branch,
            worktree: info.is_worktree,
        });
    }
}

/// Look up `cwd` in `map`: `Some(cached)` on a fresh (within-`ttl`) entry, `None` on a miss or a
/// stale entry. `now` is injected so the TTL is unit-testable without sleeping.
fn memo_get(
    map: &mut HashMap<String, MemoEntry>,
    cwd: &str,
    now: Instant,
    ttl: Duration,
) -> Option<Option<RepoInfo>> {
    let entry = map.get(cwd)?;
    (now.duration_since(entry.at) < ttl).then(|| entry.info.clone())
}

/// Record a resolve result (including `None` — negatives are cached too) at `now`.
fn memo_put(map: &mut HashMap<String, MemoEntry>, cwd: &str, now: Instant, info: Option<RepoInfo>) {
    map.insert(cwd.to_string(), MemoEntry { at: now, info });
}

/// One un-memoized resolve: spawn git, parse the output.
fn resolve_uncached(cwd: &str) -> Option<RepoInfo> {
    let (stdout, exit_ok) = run_rev_parse(GIT_PROGRAM, cwd, &GIT_MISSING)?;
    parse_rev_parse(cwd, &stdout, exit_ok)
}

/// Run the one rev-parse call in `cwd`, capturing stdout under [`DEADLINE`]. Returns
/// `(stdout, exit_ok)`, or `None` on any spawn/deadline/wait failure. A `NotFound`
/// spawn failure latches `missing`; a latched `missing` short-circuits without
/// spawning. `program`/`missing` are parameters so the NotFound path is testable in
/// isolation (a fake program + a local flag, no global poisoning).
fn run_rev_parse(program: &str, cwd: &str, missing: &AtomicBool) -> Option<(String, bool)> {
    if missing.load(Ordering::Relaxed) {
        return None;
    }
    let mut child = match Command::new(program)
        .args([
            "-C",
            cwd,
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
            "--git-common-dir",
            "--git-dir",
        ])
        // A read-only query; skip index-lock acquisition (matches the reference).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing.store(true, Ordering::Relaxed);
            }
            return None;
        }
    };

    let deadline = Instant::now() + DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // rev-parse output is a few short lines, far under the pipe buffer, so
                // draining after exit cannot deadlock.
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_string(&mut out);
                }
                return Some((out, status.success()));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Parse the three-line rev-parse output into [`RepoInfo`]. Pure over `(stdout,
/// exit_ok)`; relative git paths resolve against `cwd`. `None` for a non-zero exit (a
/// non-repo, an unborn branch, or a bare repo, which all exit non-zero here) or output
/// missing any of the three lines / a rootless common dir.
fn parse_rev_parse(cwd: &str, stdout: &str, exit_ok: bool) -> Option<RepoInfo> {
    if !exit_ok {
        return None;
    }
    let mut lines = stdout.lines();
    let branch = lines.next()?.trim();
    let common_dir = lines.next()?.trim();
    let git_dir = lines.next()?.trim();
    if branch.is_empty() || common_dir.is_empty() || git_dir.is_empty() {
        return None;
    }

    let common_abs = resolve_git_path(cwd, common_dir);
    let git_abs = resolve_git_path(cwd, git_dir);
    let is_worktree = common_abs != git_abs;

    // `--git-common-dir` is the main worktree's `.git`; its parent is the origin repo
    // root, so a linked worktree shares the main checkout's group key.
    let repo_root = common_abs.parent()?.to_path_buf();
    let repo_name = repo_root.file_name()?.to_string_lossy().to_string();

    Some(RepoInfo {
        repo_name,
        branch: branch.to_string(),
        is_worktree,
    })
}

/// Resolve a possibly-relative git path against `base`, canonicalized (so a `/var` →
/// `/private/var` symlink or a `.` common-dir compares equal across the two reads).
/// Canonicalization failure falls back to the joined path.
fn resolve_git_path(base: &str, git_path: &str) -> PathBuf {
    let p = if std::path::Path::new(git_path).is_absolute() {
        PathBuf::from(git_path)
    } else {
        PathBuf::from(base).join(git_path)
    };
    p.canonicalize().unwrap_or(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ---- pure parse fixtures (real `git rev-parse` outputs) --------------------------------------

    #[test]
    fn parse_normal_checkout() {
        // `main\n.git\n.git` in a plain checkout: not a worktree, repo = cwd basename.
        let info = parse_rev_parse("/work/myrepo", "main\n.git\n.git\n", true).unwrap();
        assert_eq!(info.repo_name, "myrepo");
        assert_eq!(info.branch, "main");
        assert!(!info.is_worktree);
    }

    #[test]
    fn parse_linked_worktree() {
        // A linked worktree: common-dir is the origin `.git`, git-dir is under
        // `.git/worktrees/<name>`, so they differ and roll up under the origin repo.
        let stdout = "feature\n/work/myrepo/.git\n/work/myrepo/.git/worktrees/wt\n";
        let info = parse_rev_parse("/work/wt", stdout, true).unwrap();
        assert_eq!(info.repo_name, "myrepo");
        assert_eq!(info.branch, "feature");
        assert!(info.is_worktree);
    }

    #[test]
    fn parse_detached_head_keeps_literal_label() {
        let info = parse_rev_parse("/work/myrepo", "HEAD\n.git\n.git\n", true).unwrap();
        assert_eq!(info.branch, "HEAD");
        assert!(!info.is_worktree);
    }

    #[test]
    fn parse_unborn_branch_is_none() {
        // A fresh `git init` with no commit: rev-parse exits non-zero.
        assert!(parse_rev_parse("/work/myrepo", "HEAD\n", false).is_none());
    }

    #[test]
    fn parse_bare_repo_is_none() {
        // A bare repo exits non-zero for this arg set (v1: unresolved).
        assert!(parse_rev_parse("/work/repo.git", "", false).is_none());
    }

    #[test]
    fn parse_non_repo_is_none() {
        // Not a git repository: non-zero exit, no usable output.
        assert!(parse_rev_parse("/tmp/plain", "", false).is_none());
    }

    #[test]
    fn parse_truncated_output_is_none() {
        // exit_ok but a missing third line: unresolved, never a panic.
        assert!(parse_rev_parse("/work/myrepo", "main\n.git\n", true).is_none());
    }

    // ---- runner NotFound latch (isolated: fake program + local flag) -----------------------------

    #[test]
    fn notfound_spawn_latches_the_flag_and_short_circuits() {
        let missing = AtomicBool::new(false);
        // A path that cannot exist: the spawn fails with NotFound.
        let out = run_rev_parse("/nonexistent/definitely-not-git", "/tmp", &missing);
        assert!(out.is_none());
        assert!(
            missing.load(Ordering::Relaxed),
            "a NotFound spawn must latch the never-spawn-again flag"
        );
        // A latched flag returns None without spawning (even a valid program).
        assert!(run_rev_parse(GIT_PROGRAM, "/tmp", &missing).is_none());
    }

    // ---- memo / TTL (injected now, no sleep) -----------------------------------------------------

    /// The `resolve` flow over an explicit map: get-else-resolve-put, mirroring the lock-drop
    /// sequence in [`resolve`] (which never holds the lock across the resolve).
    fn via_memo(
        map: &mut HashMap<String, MemoEntry>,
        cwd: &str,
        now: Instant,
        ttl: Duration,
        resolve_fn: impl FnOnce() -> Option<RepoInfo>,
    ) -> Option<RepoInfo> {
        if let Some(hit) = memo_get(map, cwd, now, ttl) {
            return hit;
        }
        let info = resolve_fn();
        memo_put(map, cwd, now, info.clone());
        info
    }

    #[test]
    fn memo_serves_within_ttl_and_refetches_after() {
        let mut map = HashMap::new();
        let ttl = Duration::from_secs(5);
        let t0 = Instant::now();
        let calls = Cell::new(0u32);
        let info = RepoInfo {
            repo_name: "r".into(),
            branch: "main".into(),
            is_worktree: false,
        };
        let mk = || {
            calls.set(calls.get() + 1);
            Some(info.clone())
        };

        assert!(via_memo(&mut map, "/r", t0, ttl, mk).is_some());
        assert_eq!(calls.get(), 1);
        // Within the TTL: served from the memo, no resolve.
        via_memo(&mut map, "/r", t0 + Duration::from_secs(3), ttl, mk);
        assert_eq!(calls.get(), 1);
        // Past the TTL: refetched.
        via_memo(&mut map, "/r", t0 + Duration::from_secs(6), ttl, mk);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn memo_caches_none_results() {
        // A non-repo cwd must not re-spawn git every frame: None is cached too.
        let mut map = HashMap::new();
        let ttl = Duration::from_secs(5);
        let t0 = Instant::now();
        let calls = Cell::new(0u32);
        let mk = || {
            calls.set(calls.get() + 1);
            None::<RepoInfo>
        };
        assert!(via_memo(&mut map, "/plain", t0, ttl, mk).is_none());
        via_memo(&mut map, "/plain", t0 + Duration::from_secs(1), ttl, mk);
        assert_eq!(
            calls.get(),
            1,
            "a cached None must not re-resolve within the TTL"
        );
    }

    // ---- integration: real temp repo + linked worktree -------------------------------------------

    /// Run a git command in `dir`; `false` if git is unavailable or the command failed.
    fn git_ok(dir: &std::path::Path, args: &[&str]) -> bool {
        Command::new(GIT_PROGRAM)
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn resolve_rolls_a_worktree_under_its_origin_repo() {
        // Unique scratch root so the process-global memo can't collide across tests.
        let root = std::env::temp_dir().join(format!(
            "tma-repo-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let repo = root.join("origin");
        let wt = root.join("linked");
        std::fs::create_dir_all(&repo).unwrap();

        // Skip gracefully if git is missing or init fails.
        if !git_ok(&repo, &["init", "-q", "-b", "main"]) {
            let _ = std::fs::remove_dir_all(&root);
            eprintln!("skipping: git unavailable");
            return;
        }
        assert!(git_ok(&repo, &["config", "user.email", "t@t"]));
        assert!(git_ok(&repo, &["config", "user.name", "t"]));
        assert!(git_ok(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "init"]
        ));
        assert!(git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        ));

        let main = resolve(repo.to_str().unwrap()).expect("main checkout resolves");
        let linked = resolve(wt.to_str().unwrap()).expect("worktree resolves");

        // Same origin repo, distinct branches, the worktree bool split.
        assert_eq!(main.repo_name, linked.repo_name);
        assert_eq!(main.branch, "main");
        assert_eq!(linked.branch, "feature");
        assert!(!main.is_worktree);
        assert!(linked.is_worktree);

        // A non-git dir under the same root resolves to nothing.
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(resolve(plain.to_str().unwrap()).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }
}
