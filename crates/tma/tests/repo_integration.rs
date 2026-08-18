//! Repo/worktree annotation on a scratch server: two worktrees of one repo plus a
//! non-git pane, asserting `ls --json` rolls the worktrees up under one `repo`, labels distinct
//! branches, splits `worktree` true/false, and nulls all three keys for the non-git pane.
//!
//! Scratch `tmux -L tma_test_<unique>` (`-f /dev/null`), killed on drop. Skips gracefully when
//! tmux or git is unavailable, or when the host does not report `#{pane_current_path}` (the cwd the
//! resolver reads); the assertions never fall through vacuously.

use std::path::Path;
use std::process::{Command, Stdio};

use tma_test_support as common;
use tma_test_support::Scratch;

/// Run `tma <args>` against the scratch server + this suite's manifest dir (the workdir).
fn tma(s: &Scratch, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(args)
        .arg("--socket-name")
        .arg(&s.socket)
        .arg("--manifest-dir")
        .arg(&s.workdir)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma")
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// Run a git command in `dir`; `false` if git is unavailable or the command failed.
fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|st| st.success())
        .unwrap_or(false)
}

/// Spawn a pane whose process runs in `cwd` (printing known chrome, then a long-lived `sleep`),
/// returning its pane id once the chrome has rendered.
fn spawn_pane(s: &Scratch, cwd: &Path) -> String {
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        cwd.to_str().unwrap(),
        "-x",
        "100",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    assert!(
        common::wait_capture_contains(&s.socket, &pane, "READY", common::POLL_CEILING),
        "pane chrome did not render"
    );
    pane
}

/// Author a manifest matching `pane`'s real process names, so every `sleep` pane classifies as an
/// idle agent (the "READY" tail rule). Mirrors the surfaces suite's derivation so the identity path
/// works regardless of how `sleep` resolves on the host.
fn write_sleep_manifest(s: &Scratch, pane: &str) {
    let current_command = basename(&s.display(pane, "#{pane_current_command}"));
    let pane_pid = s.display(pane, "#{pane_pid}");
    let ps_comm = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pane_pid])
            .output()
            .expect("ps")
            .stdout,
    ));
    let mut names = vec![current_command, ps_comm];
    names.sort();
    names.dedup();
    let names_toml = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        s.workdir.join("agent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [{names_toml}]\n\
             [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
             [[rules]]\nstate = \"idle\"\npriority = 50\n\
             region = \"tail_lines(50)\"\nmatch = {{ contains = \"READY\" }}\n"
        ),
    )
    .unwrap();
}

/// The flat row object (agents-array element) whose `pane` field is `pane`. The row objects are
/// flat (no nested objects), so splitting the array text on `},{` isolates each one; the field
/// `.contains` checks below do not depend on the stripped braces.
fn row_for<'a>(json: &'a str, pane: &str) -> &'a str {
    json.split("},{")
        .find(|obj| obj.contains(&format!("\"pane\":\"{pane}\"")))
        .unwrap_or_else(|| panic!("no row for {pane} in {json}"))
}

#[test]
fn ls_json_rolls_worktrees_up_and_nulls_non_git() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not installed");
        return;
    }

    let s = Scratch::new("repo");
    // Git scaffold under the workdir (torn down with the Scratch): an origin checkout, a linked
    // worktree on its own branch, and a plain non-git directory. Subdirs are ignored by the
    // `.toml`-only manifest loader, so they never perturb identity.
    let origin = s.workdir.join("origin");
    let linked = s.workdir.join("linked");
    let plain = s.workdir.join("plain");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&plain).unwrap();

    if !git(&origin, &["init", "-q", "-b", "main"]) {
        eprintln!("skipping: git init failed (unavailable or too old for -b)");
        return;
    }
    assert!(git(&origin, &["config", "user.email", "t@t"]));
    assert!(git(&origin, &["config", "user.name", "t"]));
    assert!(git(
        &origin,
        &["commit", "-q", "--allow-empty", "-m", "init"]
    ));
    assert!(git(
        &origin,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().unwrap()
        ],
    ));

    let pane_main = spawn_pane(&s, &origin);
    write_sleep_manifest(&s, &pane_main);
    let pane_wt = spawn_pane(&s, &linked);
    let pane_plain = spawn_pane(&s, &plain);

    // The resolver reads `#{pane_current_path}`; if this host does not report a pane's cwd, the
    // annotation has nothing to work from and the assertions would be vacuous. Skip in that case.
    if !s
        .display(&pane_main, "#{pane_current_path}")
        .contains("origin")
    {
        eprintln!("skipping: host does not report #{{pane_current_path}} for the pane");
        return;
    }

    let out = tma(&s, &["ls", "--json"]);
    assert!(
        out.status.success(),
        "ls --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("\"schema\":1"), "schema stays 1: {json}");

    let main_row = row_for(&json, &pane_main);
    let wt_row = row_for(&json, &pane_wt);
    let plain_row = row_for(&json, &pane_plain);

    // Both checkouts roll up under the same origin repo name (basename of the git common dir's parent).
    assert!(
        main_row.contains("\"repo\":\"origin\""),
        "main checkout's repo is the origin basename: {main_row}"
    );
    assert!(
        wt_row.contains("\"repo\":\"origin\""),
        "the linked worktree rolls up under the same repo: {wt_row}"
    );

    // Distinct branch labels.
    assert!(
        main_row.contains("\"branch\":\"main\""),
        "main checkout keeps its branch: {main_row}"
    );
    assert!(
        wt_row.contains("\"branch\":\"feature\""),
        "the worktree keeps its own branch: {wt_row}"
    );

    // The worktree bool split: the origin checkout is not a worktree, the linked one is.
    assert!(
        main_row.contains("\"worktree\":false"),
        "the origin checkout is the main worktree: {main_row}"
    );
    assert!(
        wt_row.contains("\"worktree\":true"),
        "the linked checkout is a worktree: {wt_row}"
    );

    // The non-git pane nulls all three keys.
    assert!(
        plain_row.contains("\"repo\":null"),
        "a non-git pane has no repo: {plain_row}"
    );
    assert!(
        plain_row.contains("\"branch\":null"),
        "a non-git pane has no branch: {plain_row}"
    );
    assert!(
        plain_row.contains("\"worktree\":null"),
        "a non-git pane's worktree bool is null: {plain_row}"
    );

    // The same three labels on the plain output's trailing columns, over the same fixture: the text
    // and JSON surfaces resolve from one annotated row set and must agree about it.
    let text = String::from_utf8_lossy(&tma(&s, &["ls"]).stdout).to_string();
    let cols = |pane: &str| -> Vec<String> {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{pane}\t")))
            .unwrap_or_else(|| panic!("no plain row for {pane}: {text}"));
        line.split('\t').map(str::to_string).collect()
    };

    let main_cols = cols(&pane_main);
    assert_eq!(main_cols.len(), 12, "12 columns: {main_cols:?}");
    assert_eq!(
        &main_cols[9..],
        &["origin".to_string(), "main".to_string(), String::new()],
        "the main checkout's repo/branch, and an empty worktree marker"
    );

    let wt_cols = cols(&pane_wt);
    assert_eq!(
        &wt_cols[9..],
        &["origin".to_string(), "feature".to_string(), "1".to_string()],
        "the linked worktree rolls up under the origin, keeps its branch, and is marked"
    );

    assert_eq!(
        &cols(&pane_plain)[9..],
        &[String::new(), String::new(), String::new()],
        "a non-git pane leaves all three columns empty"
    );
}
