//! Per-cwd repo/branch resolution: the git metadata the grouping and
//! branch-label surfaces render. One bounded `git -C <cwd> rev-parse --abbrev-ref
//! HEAD --git-common-dir --git-dir` per unique working directory, memoized with a
//! ~5 s TTL so no per-frame path spawns git; failure degrades to absent labels,
//! never a surfaced error. Only [`annotate_rows`] and its tighter-budget sibling
//! [`annotate_seed_rows`] resolve — the display/serialize call sites call them, the
//! poll/jump/act/capture paths never do. Both resolve a whole row set in one batch of
//! spawns, so the cost is one git's wall clock rather than one per pane.
//!
//! The rev-parse argument list, its output parsing and the git-path resolution below
//! were written from the `git rev-parse` documentation and the tests in this module.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// Wall-clock bound on a rev-parse batch, shared by every child in it (they are spawned
/// together). A hung git (a network filesystem, a wedged credential helper) must not stall
/// a surface refresh.
const DEADLINE: Duration = Duration::from_secs(3);

/// The bound for [`annotate_seed_rows`], well under [`DEADLINE`]. A surface's stamp seed is drawn
/// before the terminal is even in raw mode, so a git that would take the full three seconds must
/// cost a blank branch column, not three seconds of blank screen.
const SEED_BUDGET: Duration = Duration::from_millis(250);

/// The child-exit poll interval, backing off from `MIN` to `MAX`. git answers a rev-parse in a
/// few ms, so a flat 10 ms wait would spend more time asleep than git spends running; backing
/// off keeps the common case tight without spinning for the three seconds a hung one gets.
const POLL_MIN: Duration = Duration::from_millis(1);
const POLL_MAX: Duration = Duration::from_millis(10);

/// Resolved git metadata for one working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// Basename of the origin repo root (parent of `--git-common-dir`); the grouping key,
    /// so a linked worktree rolls up under its main checkout.
    pub repo_name: String,
    /// `git rev-parse --abbrev-ref HEAD`; the literal `HEAD` when detached.
    pub branch: String,
    /// Whether this checkout is a linked worktree (its git-dir sits under the common dir).
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
/// spawn/deadline failure. Only a definite answer is memoized: a git killed at the
/// deadline leaves the memo untouched, so the next call retries rather than being
/// served a cached "no repo" for the rest of the TTL.
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
    let Some(Resolved::Known(info)) = resolve_batch_uncached(&[cwd], DEADLINE).pop() else {
        return None;
    };
    memo_put(
        &mut MEMO.lock().unwrap_or_else(|e| e.into_inner()),
        cwd,
        now,
        info.clone(),
    );
    info
}

/// Fill each row's `repo` label from its `cwd`. `Some` (name/branch/worktree) for a resolved
/// checkout, `None` exactly when the repo is unresolved; `worktree` is `false` for a main
/// checkout, `true` for a linked worktree.
///
/// Every cwd the memo cannot already answer is resolved in one batch, so N panes across N
/// checkouts cost one git's wall clock rather than N sequential spawns.
pub fn annotate_rows(rows: &mut [AgentRow]) {
    annotate_within(rows, DEADLINE);
}

/// [`annotate_rows`] for a surface's first frame, which is drawn before its terminal is in raw
/// mode: the same labels under a much tighter [`SEED_BUDGET`], so a slow git costs a bare branch
/// column that the next refresh fills in rather than a visibly late window. A cwd the budget cut
/// short is memoized as unresolved like any other failure, so that refresh may be the one after
/// the [`MEMO_TTL`] rather than the next.
pub fn annotate_seed_rows(rows: &mut [AgentRow]) {
    annotate_within(rows, SEED_BUDGET);
}

fn annotate_within(rows: &mut [AgentRow], budget: Duration) {
    prime(rows.iter().filter_map(|r| r.cwd.as_deref()), budget);
    for row in rows.iter_mut() {
        row.repo = row.cwd.as_deref().and_then(resolve).map(|info| RepoLabel {
            name: info.repo_name,
            branch: info.branch,
            worktree: info.is_worktree,
        });
    }
}

/// Resolve every cold cwd in `cwds` in one batch and memoize the results, so the [`resolve`] calls
/// that follow are all hits. Empty and already-fresh cwds are skipped, and a cwd repeated across
/// panes (the common case: several agents in one checkout) is spawned once.
fn prime<'a>(cwds: impl Iterator<Item = &'a str>, budget: Duration) {
    let now = Instant::now();
    let mut cold: BTreeSet<String> = BTreeSet::new();
    {
        let memo = &mut *MEMO.lock().unwrap_or_else(|e| e.into_inner());
        for cwd in cwds.filter(|c| !c.is_empty()) {
            if memo_get(memo, cwd, now, MEMO_TTL).is_none() {
                cold.insert(cwd.to_string());
            }
        }
    }
    if cold.is_empty() {
        return;
    }
    // The lock is dropped across the spawns (up to `budget`), as in `resolve`.
    let cold: Vec<String> = cold.into_iter().collect();
    let refs: Vec<&str> = cold.iter().map(String::as_str).collect();
    let resolved = resolve_batch_uncached(&refs, budget);
    let memo = &mut *MEMO.lock().unwrap_or_else(|e| e.into_inner());
    for (cwd, r) in refs.iter().zip(resolved) {
        // A budget-killed cwd is left out of the memo entirely: caching it would serve "no repo"
        // for the TTL and suppress the retry the next refresh should make. That matters most on
        // the seed path, whose whole point is a budget short enough to give up early.
        if let Resolved::Known(info) = r {
            memo_put(memo, cwd, now, info);
        }
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
/// What a resolve concluded about one cwd, and therefore whether it belongs in the memo.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolved {
    /// A definite answer: the labels, or `None` for a non-repo, an unborn or bare repo, a
    /// git-binary-missing machine, or a spawn failure. Worth caching for the TTL.
    Known(Option<RepoInfo>),
    /// No answer: the budget cut git off before it produced one. Says nothing about the cwd, so
    /// caching it would suppress the retry the next refresh is supposed to make.
    Unknown,
}

/// One un-memoized resolve per cwd, all spawned before any is drained. Results are positional.
fn resolve_batch_uncached(cwds: &[&str], budget: Duration) -> Vec<Resolved> {
    run_rev_parse_batch(GIT_PROGRAM, cwds, &GIT_MISSING, budget)
        .into_iter()
        .zip(cwds)
        .map(|(out, cwd)| match out {
            RevParse::Done(stdout, exit_ok) => {
                Resolved::Known(parse_rev_parse(cwd, &stdout, exit_ok))
            }
            RevParse::Failed => Resolved::Known(None),
            RevParse::TimedOut => Resolved::Unknown,
        })
        .collect()
}

/// What one rev-parse child ended up saying. [`RevParse::TimedOut`] is kept apart from
/// [`RevParse::Failed`] because the two mean opposite things for the memo: a spawn or wait failure
/// is this cwd's answer, while a kill at the budget is the absence of one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RevParse {
    /// git ran to completion: its stdout, and whether it exited zero.
    Done(String, bool),
    /// No answer, and none is coming: the spawn failed, or `wait` errored.
    Failed,
    /// No answer yet: the budget ran out and the child was killed.
    TimedOut,
}

/// Run one rev-parse per cwd, spawning every child before draining any, and capture stdout under a
/// `budget` shared by the batch (they start together). Results are positional. A `NotFound` spawn
/// failure latches `missing`, which skips the rest of the batch and every later call.
/// `program`/`missing` are parameters so the NotFound path is testable in isolation (a fake program
/// + a local flag, no global poisoning).
fn run_rev_parse_batch(
    program: &str,
    cwds: &[&str],
    missing: &AtomicBool,
    budget: Duration,
) -> Vec<RevParse> {
    let mut children: Vec<Option<Child>> = cwds
        .iter()
        .map(|cwd| spawn_rev_parse(program, cwd, missing))
        .collect();
    let mut out: Vec<RevParse> = vec![RevParse::Failed; cwds.len()];

    let deadline = Instant::now() + budget;
    let mut backoff = POLL_MIN;
    loop {
        let mut waiting = false;
        for (slot, child) in out.iter_mut().zip(children.iter_mut()) {
            let Some(running) = child else { continue };
            match running.try_wait() {
                Ok(Some(status)) => {
                    // rev-parse output is a few short lines, far under the pipe buffer, so
                    // draining after exit cannot deadlock.
                    let mut buf = String::new();
                    if let Some(mut so) = running.stdout.take() {
                        let _ = so.read_to_string(&mut buf);
                    }
                    *slot = RevParse::Done(buf, status.success());
                    *child = None;
                }
                Ok(None) => waiting = true,
                // A wait error leaves the slot `Failed`: no answer, and no child left to ask.
                Err(_) => *child = None,
            }
        }
        if !waiting {
            return out;
        }
        if Instant::now() >= deadline {
            for (slot, child) in out.iter_mut().zip(children.iter_mut()) {
                if let Some(straggler) = child {
                    let _ = straggler.kill();
                    let _ = straggler.wait();
                    *slot = RevParse::TimedOut;
                }
            }
            return out;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(POLL_MAX);
    }
}

/// Spawn the one rev-parse call in `cwd`. `None` on any spawn failure; a `NotFound` latches
/// `missing`, and a latched `missing` short-circuits without spawning at all.
fn spawn_rev_parse(program: &str, cwd: &str, missing: &AtomicBool) -> Option<Child> {
    if missing.load(Ordering::Relaxed) {
        return None;
    }
    // rev-parse echoes one line per argument, in argument order, so the three lines of stdout are
    // the branch, the common git dir and this checkout's git dir.
    let spawned = Command::new(program)
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
            "--git-common-dir",
            "--git-dir",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => Some(child),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing.store(true, Ordering::Relaxed);
            }
            None
        }
    }
}

/// Read the three lines [`spawn_rev_parse`] asks for into a [`RepoInfo`]. `None` unless git exited
/// zero and printed all three: an unborn branch, a bare repo and a non-repo all fail that way, and
/// so does output cut short, which must be unresolved rather than a panic.
fn parse_rev_parse(cwd: &str, stdout: &str, exit_ok: bool) -> Option<RepoInfo> {
    if !exit_ok {
        return None;
    }
    let mut lines = stdout.lines().map(str::trim);
    let branch = lines.next()?;
    let (common_line, git_dir_line) = (lines.next()?, lines.next()?);
    if branch.is_empty() || common_line.is_empty() || git_dir_line.is_empty() {
        return None;
    }
    let common = resolve_git_path(cwd, common_line);
    let git_dir = resolve_git_path(cwd, git_dir_line);
    // The grouping key is the origin repo root, the directory holding the common git dir, so a
    // linked worktree and the checkout it was added from share one key.
    let repo_name = common.parent()?.file_name()?.to_string_lossy().into_owned();
    Some(RepoInfo {
        repo_name,
        branch: branch.to_string(),
        // A linked worktree's git dir sits under the common dir (`.git/worktrees/<name>`), a main
        // checkout's is the common dir itself. Containment, not inequality: see `resolve_git_path`.
        is_worktree: git_dir != common && git_dir.starts_with(&common),
    })
}

/// Turn one path `rev-parse` printed into a comparable one. git prints these relative to the
/// directory it ran in or absolute, and mixes the two within a single call: from a subdirectory of
/// a plain checkout the common dir comes back as `../../.git` while the git dir is absolute. So
/// join relatives onto `base` and fold `.`/`..` away, or the two forms never compare equal.
fn resolve_git_path(base: &str, git_path: &str) -> PathBuf {
    let raw = Path::new(git_path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(base).join(raw)
    };
    // Folded lexically rather than with `canonicalize`, which touches the filesystem and fails
    // outright on a path that does not exist.
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`; a leading `..` has nothing to cancel and stays.
                Some(Component::RootDir) => {}
                _ => out.push(Component::ParentDir),
            },
            other => out.push(other),
        }
    }
    out
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

    #[test]
    fn parse_relative_common_dir_from_a_subdirectory() {
        // From a subdirectory of a plain checkout git prints the common dir relative to the cwd
        // and the git dir absolute; folding the `..` away is what keeps that from reading as a
        // worktree, and is what leaves the origin basename recoverable.
        let stdout = "main\n../../.git\n/work/myrepo/.git\n";
        let info = parse_rev_parse("/work/myrepo/sub/deep", stdout, true).unwrap();
        assert_eq!(info.repo_name, "myrepo");
        assert_eq!(info.branch, "main");
        assert!(!info.is_worktree);
    }

    #[test]
    fn parse_mismatched_path_flavours_are_not_a_worktree() {
        // The same checkout reached through a symlinked cwd (`/tmp` on macOS): the relative line
        // resolves under the link, the absolute one under the target, so the two never compare
        // equal. Only containment under the common dir may say "worktree".
        let stdout = "main\n../../.git\n/private/work/myrepo/.git\n";
        let info = parse_rev_parse("/work/myrepo/sub/deep", stdout, true).unwrap();
        assert!(!info.is_worktree);
    }

    // ---- runner NotFound latch (isolated: fake program + local flag) -----------------------------

    #[test]
    fn notfound_spawn_latches_the_flag_and_short_circuits() {
        let missing = AtomicBool::new(false);
        // A path that cannot exist: the spawn fails with NotFound.
        let out = run_rev_parse_batch(
            "/nonexistent/definitely-not-git",
            &["/tmp"],
            &missing,
            DEADLINE,
        );
        assert_eq!(out, vec![RevParse::Failed]);
        assert!(
            missing.load(Ordering::Relaxed),
            "a NotFound spawn must latch the never-spawn-again flag"
        );
        // A latched flag returns None without spawning (even a valid program), for every position
        // of a batch — one missing git must not cost one failed spawn per pane.
        assert_eq!(
            run_rev_parse_batch(GIT_PROGRAM, &["/tmp", "/var"], &missing, DEADLINE),
            vec![RevParse::Failed, RevParse::Failed]
        );
    }

    /// A child that never exits is killed at the budget and reported as `TimedOut`, not `Failed`:
    /// the whole point of the split is that the caller must not cache it.
    ///
    /// The stand-in for a hung git is a script we write, because the runner hands its program the
    /// rev-parse arguments and any real command has an opinion about them. A stock utility is not
    /// portable here: `yes` loops on BSD, which reads `-C` as a string to print, and exits non-zero
    /// on GNU, which reads it as an invalid option. A `#!/bin/sh` script takes them as ignored
    /// positional parameters on both.
    #[test]
    fn a_child_that_outlives_the_budget_is_timed_out_not_failed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tma-repo-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hang = dir.join("hang.sh");
        std::fs::write(&hang, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();

        let missing = AtomicBool::new(false);
        let started = Instant::now();
        let out = run_rev_parse_batch(
            hang.to_str().unwrap(),
            &["/tmp"],
            &missing,
            Duration::from_millis(50),
        );
        let elapsed = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(out, vec![RevParse::TimedOut]);
        assert!(
            elapsed < Duration::from_secs(1),
            "the budget bounds the batch, not DEADLINE: took {elapsed:?}"
        );
    }

    #[test]
    fn only_a_definite_answer_reaches_the_memo() {
        // `prime`'s caching rule over an explicit map: a `Known` result lands (even `Known(None)`,
        // so a non-git pane does not re-spawn git every frame), a `TimedOut` one does not, so the
        // next refresh retries instead of being served a cached "no repo" for the rest of the TTL.
        let mut map = HashMap::new();
        let now = Instant::now();
        let found = RepoInfo {
            repo_name: "myrepo".into(),
            branch: "main".into(),
            is_worktree: false,
        };
        for (cwd, r) in [
            ("/repo", Resolved::Known(Some(found.clone()))),
            ("/plain", Resolved::Known(None)),
            ("/slow", Resolved::Unknown),
        ] {
            if let Resolved::Known(i) = r {
                memo_put(&mut map, cwd, now, i);
            }
        }
        assert_eq!(
            memo_get(&mut map, "/repo", now, MEMO_TTL),
            Some(Some(found))
        );
        assert_eq!(
            memo_get(&mut map, "/plain", now, MEMO_TTL),
            Some(None),
            "a resolved non-repo is cached, so it stops re-spawning git"
        );
        assert_eq!(
            memo_get(&mut map, "/slow", now, MEMO_TTL),
            None,
            "a timed-out cwd is a miss, so the next call resolves it again"
        );
    }

    #[test]
    fn batch_results_stay_aligned_with_their_inputs() {
        // The batch drains children as they exit, which is out of input order whenever one git
        // finishes first, so the positional contract is what the zip in `resolve_batch_uncached`
        // rests on. Asserted without claiming any of these paths is (or is not) a repo, so it
        // holds in a sandbox with no git and in a source tree checked out without `.git`.
        let missing = AtomicBool::new(false);
        let cwds = ["/tmp", "/", "/tmp"];
        let out = run_rev_parse_batch(GIT_PROGRAM, &cwds, &missing, DEADLINE);
        assert_eq!(out.len(), cwds.len());
        assert_eq!(out[0], out[2], "the same cwd resolves the same way twice");
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
