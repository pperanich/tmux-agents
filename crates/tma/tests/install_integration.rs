//! `tma install-hooks` acceptance on TEMP config + a scratch tmux server.
//!
//! Everything writes to temp paths and a scratch `tmux -L` server, never the user's real
//! `~/.claude/settings.json`, `~/.config/tma`, or the default tmux server (SAFETY): the
//! settings/config/wrapper paths are passed explicitly and the scratch socket is started with
//! `-f /dev/null` (config isolation).
//! Asserts the byte-identical install/uninstall round-trip, tmux-hook install, and that
//! `--check` detects a deleted tmux hook.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{scratch_tmux, unique_id};
use tma_test_support as common;

// Deliberately NOT folded onto the shared `tma-test-support::Scratch`: this suite's whole surface is
// install-path plumbing (the per-agent config paths + a nine-flag `install_hooks` builder) no other
// suite shares, so converging it would trade ~90 call-site edits for a few lines of boilerplate.
struct Scratch {
    socket: String,
    workdir: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        common::reap_orphan_scratch_servers();
        let unique = unique_id();
        let workdir = std::env::temp_dir().join(format!("tma_install_{unique}"));
        std::fs::create_dir_all(&workdir).unwrap();
        Scratch {
            socket: format!("tma_test_install_{unique}"),
            workdir,
        }
    }

    fn tmux(&self, args: &[&str]) -> Output {
        scratch_tmux(&self.socket, args)
    }

    fn settings(&self) -> PathBuf {
        self.workdir.join("settings.json")
    }
    fn config_dir(&self) -> PathBuf {
        self.workdir.join("cfg")
    }
    fn wrapper(&self) -> PathBuf {
        self.workdir.join("bin/tma-hook")
    }
    /// Codex's `config.toml`, pinned to the temp workdir so the real (dotfiles-symlinked)
    /// `~/.codex/config.toml` is NEVER touched (SAFETY).
    fn codex_config(&self) -> PathBuf {
        self.workdir.join("codex/config.toml")
    }
    /// Codex's `hooks.json`, pinned to the temp workdir — same SAFETY rule as `codex_config`.
    fn codex_hooks(&self) -> PathBuf {
        self.workdir.join("codex/hooks.json")
    }
    /// Gemini's `settings.json`, pinned to the temp workdir so the real `~/.gemini/settings.json`
    /// is NEVER touched (SAFETY).
    fn gemini_settings(&self) -> PathBuf {
        self.workdir.join("gemini/settings.json")
    }
    /// Cursor's `hooks.json`, pinned to the temp workdir so the real `~/.cursor/hooks.json` is
    /// NEVER touched (SAFETY).
    fn cursor_hooks(&self) -> PathBuf {
        self.workdir.join("cursor/hooks.json")
    }
    /// Cursor's `cli-config.json` (the statusLine context shim), pinned to the temp workdir so the
    /// real `~/.cursor/cli-config.json` is NEVER touched (SAFETY).
    fn cursor_cli_config(&self) -> PathBuf {
        self.workdir.join("cursor/cli-config.json")
    }
    /// pi's extension module, pinned to the temp workdir so the real
    /// `~/.pi/agent/extensions/tma.js` is NEVER touched (SAFETY).
    fn pi_extension(&self) -> PathBuf {
        self.workdir.join("pi/extensions/tma.js")
    }
    /// OpenCode's plugin, pinned to the temp workdir so the real
    /// `~/.config/opencode/plugin/tma.js` is NEVER touched, and so a developer who HAS opencode
    /// wired never makes a test read another agent as still installed (SAFETY).
    fn opencode_plugin(&self) -> PathBuf {
        self.workdir.join("opencode/plugin/tma.js")
    }

    /// Read a pane option (`show-options -pqv`), trimmed; empty when unset.
    fn pane_option(&self, pane: &str, key: &str) -> String {
        let out = self.tmux(&["show-options", "-pqv", "-t", pane, key]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `tma install-hooks …` with every path pinned to the temp workdir and the scratch
    /// socket — never the real config or default server.
    fn install_hooks(&self, extra: &[&str]) -> Output {
        self.install_hooks_env(extra, &[])
    }

    /// [`Self::install_hooks`] with extra environment for the child. The `--wrapper-ref bare` tests
    /// set `PATH`, which they can only do per-spawn: the suite runs in parallel, so mutating this
    /// process's environment would leak into every other test.
    fn install_hooks_env(&self, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let settings = self.settings();
        let cfg = self.config_dir();
        let wrapper = self.wrapper();
        let codex_config = self.codex_config();
        let mut args: Vec<String> = vec!["install-hooks".into()];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.extend([
            "--settings".into(),
            settings.display().to_string(),
            "--config-dir".into(),
            cfg.display().to_string(),
            "--wrapper-path".into(),
            wrapper.display().to_string(),
            "--codex-config".into(),
            codex_config.display().to_string(),
            "--codex-hooks".into(),
            self.codex_hooks().display().to_string(),
            "--gemini-settings".into(),
            self.gemini_settings().display().to_string(),
            "--cursor-hooks".into(),
            self.cursor_hooks().display().to_string(),
            "--cursor-cli-config".into(),
            self.cursor_cli_config().display().to_string(),
            "--pi-extension".into(),
            self.pi_extension().display().to_string(),
            "--opencode-plugin".into(),
            self.opencode_plugin().display().to_string(),
            "--socket-name".into(),
            self.socket.clone(),
        ]);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tma"));
        cmd.args(&args)
            .env("TMA_BIN", env!("CARGO_BIN_EXE_tma"))
            .env("TMA_CONFIG", common::empty_config_path());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn tma")
    }

    /// A `PATH` with the wrapper's own directory in front, so a bare reference resolves. The
    /// inherited `PATH` stays behind it: tma still has to find `tmux`.
    fn path_with_wrapper_dir(&self) -> String {
        let dir = self.wrapper().parent().unwrap().display().to_string();
        match self.path_without_wrapper() {
            rest if !rest.is_empty() => format!("{dir}:{rest}"),
            _ => dir,
        }
    }

    /// The inherited `PATH` with `tma-hook` made unresolvable, so a bare reference does NOT
    /// resolve. A developer running the suite usually has tma installed for real, and that copy
    /// would otherwise answer for the one under test.
    ///
    /// Each offending directory is replaced IN PLACE by a scratch mirror of symlinks to everything
    /// it holds except `tma-hook`, rather than being dropped. Dropping was the obvious move and it
    /// was wrong: it assumes one binary per directory. A nix profile
    /// (`/etc/profiles/per-user/<user>/bin`) aggregates every installed tool into ONE directory, so
    /// on a machine where tma is installed that way, dropping it to hide `tma-hook` took `tmux`
    /// down with it and the install tests failed with "tmux is not installed or not on PATH" —
    /// exactly what the doc comment above promises will still work. Homebrew's `/opt/homebrew/bin`
    /// and `~/.cargo/bin` share the shape.
    fn path_without_wrapper(&self) -> String {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let kept: Vec<PathBuf> = std::env::split_paths(&inherited)
            .enumerate()
            .map(|(i, dir)| {
                if dir.join("tma-hook").exists() {
                    self.mirror_without_wrapper(&dir, i)
                } else {
                    dir
                }
            })
            .collect();
        std::env::join_paths(kept)
            .expect("rejoin PATH")
            .to_string_lossy()
            .into_owned()
    }

    /// A scratch directory symlinking every entry of `dir` except `tma-hook`. Falls back to the
    /// original path if the mirror cannot be built — better a test that fails loudly on a stray
    /// real `tma-hook` than one that silently loses `tmux`.
    fn mirror_without_wrapper(&self, dir: &Path, index: usize) -> PathBuf {
        let mirror = self.workdir.join(format!("pathmirror/{index}"));
        if std::fs::create_dir_all(&mirror).is_err() {
            return dir.to_path_buf();
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return dir.to_path_buf();
        };
        for entry in entries.flatten() {
            if entry.file_name() == "tma-hook" {
                continue;
            }
            let _ = std::os::unix::fs::symlink(entry.path(), mirror.join(entry.file_name()));
        }
        mirror
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The non-panicking kill: this runs while a failed assertion is unwinding, where a panic
        // would abort the binary and leave every other scratch server behind.
        common::kill_scratch_server(&self.socket);
        common::cleanup_scratch_socket(&self.socket);
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[test]
fn install_uninstall_round_trip_and_check_detects_wipe() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    // A scratch server with a pane (a hook target).
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // A canonical original settings file with unrelated content the installer must preserve.
    let original = "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\n      \"Bash\"\n    ]\n  }\n}\n";
    std::fs::write(s.settings(), original).unwrap();

    // --- install ---
    let out = s.install_hooks(&["claude", "--yes"]);
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Wrapper written, executable.
    assert!(s.wrapper().exists(), "wrapper installed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(s.wrapper()).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "wrapper is executable");
    }

    // settings.json references the wrapper for the mapped events, preserving prior content.
    let installed = std::fs::read_to_string(s.settings()).unwrap();
    assert!(
        installed.contains("\"model\": \"opus\""),
        "preserved user content"
    );
    let wrapper_cmd = format!("{} claude Notification", s.wrapper().display());
    assert!(
        installed.contains(&wrapper_cmd),
        "wired Notification via wrapper"
    );
    assert!(installed.contains("claude SessionStart"));

    // tmux hooks installed on the scratch server.
    for hook in ["after-select-pane", "after-select-window"] {
        let shown =
            String::from_utf8_lossy(&s.tmux(&["show-hooks", "-g", hook]).stdout).to_string();
        assert!(
            shown.contains("clear-attention"),
            "{hook} should run clear-attention, got: {shown}"
        );
    }
    // The per-server keyed hooks-state file recorded the indexes (Fix B: keyed by socket, since
    // tmux -g hook indexes are per-server).
    let keyed = std::fs::read_dir(s.config_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hooks-state-") && n.ends_with(".toml"))
        })
        .expect("a keyed hooks-state file exists after install");
    let state = std::fs::read_to_string(&keyed).unwrap();
    assert!(state.contains("after-select-pane"), "recorded hook index");

    // --- --check: all wired ---
    let out = s.install_hooks(&["--check"]);
    assert!(
        out.status.success(),
        "--check should pass right after install: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --- --check detects a deleted tmux hook (config-reload wipe / server restart) ---
    assert!(s
        .tmux(&["set-hook", "-gu", "after-select-pane"])
        .status
        .success());
    let out = s.install_hooks(&["--check"]);
    assert!(
        !out.status.success(),
        "--check must fail when a tmux hook was wiped"
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("after-select-pane"),
        "report names the missing hook: {report}"
    );
    // Recorded but absent server-wide is its own state: hooks are runtime server state, so the
    // report names the restart rather than reading like a never-installed hook.
    assert!(
        report.contains("likely restarted") && report.contains("tmux.conf"),
        "the wiped array names the restart and the durable fix: {report}"
    );

    // --- uninstall: byte-identical settings, tmux hooks gone ---
    let out = s.install_hooks(&["claude", "--uninstall", "--yes"]);
    assert!(
        out.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(s.settings()).unwrap();
    assert_eq!(
        after, original,
        "uninstall restores settings.json byte-for-byte"
    );

    let shown =
        String::from_utf8_lossy(&s.tmux(&["show-hooks", "-g", "after-select-window"]).stdout)
            .to_string();
    assert!(
        !shown.contains("clear-attention"),
        "tmux hook removed on uninstall: {shown}"
    );
}

/// Uninstalling the last wired agent sweeps the options tma stamped onto every pane. Nothing
/// refreshes a stamp once the wiring is gone, so a `#{@agent_state}` left in a border or status
/// format would otherwise read one frozen state for the life of the server.
#[test]
fn uninstalling_the_last_agent_sweeps_the_pane_stamps() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    let pane = String::from_utf8_lossy(
        &s.tmux(&["display-message", "-p", "-t", "s1", "#{pane_id}"])
            .stdout,
    )
    .trim()
    .to_string();

    let out = s.install_hooks(&["claude", "--yes"]);
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Stamp the pane as a producer would: the state tuple, the attention flag, the action lock,
    // and the window rollup a per-window status line reads.
    let stamped = [
        ("@agent_name", "claude"),
        ("@agent_state", "blocked"),
        ("@agent_source", "hook"),
        ("@agent_since", "1700000000000"),
        ("@agent_attention", "1"),
        ("@agent_context_pct", "80"),
        ("@agent_action", "1700000000000:nonce:1:approve"),
    ];
    for (key, value) in stamped {
        assert!(s
            .tmux(&["set-option", "-p", "-t", &pane, key, value])
            .status
            .success());
    }
    assert!(s
        .tmux(&[
            "set-option",
            "-w",
            "-t",
            &pane,
            "@agent_summary",
            "blocked:1"
        ])
        .status
        .success());

    let out = s.install_hooks(&["claude", "--uninstall", "--yes"]);
    assert!(
        out.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for (key, _) in stamped {
        assert_eq!(
            s.pane_option(&pane, key),
            "",
            "{key} must be unset after the last agent's uninstall"
        );
    }
    assert_eq!(
        s.pane_option(&pane, "@agent_summary"),
        "",
        "the window rollup is swept with the pane options"
    );

    // The status-line entry is the user's own line in their own config, so uninstall names it
    // instead of editing a file tma never wrote.
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("status-right") && report.contains("tma status"),
        "uninstall points at the line to remove by hand: {report}"
    );
}

/// A settings file tma cannot read must abort the install, not be treated as an absent one: the
/// installer diffs `old` against fresh wiring and writes the result, so an empty-object fallback
/// would replace the user's file wholesale (silently, under `--yes`).
#[test]
fn install_refuses_an_unreadable_settings_file() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // Present but not readable as text (a truncated/binary settings.json).
    let bytes: &[u8] = b"{\n  \"model\": \"\xff\xfe opus\"\n}\n";
    std::fs::write(s.settings(), bytes).unwrap();

    let out = s.install_hooks(&["claude", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "install must fail on an unreadable settings file: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("cannot read") && stderr.contains("settings.json"),
        "the error names the unreadable file: {stderr}"
    );
    assert_eq!(
        std::fs::read(s.settings()).unwrap(),
        bytes,
        "the refused install left the file byte-for-byte"
    );
}

/// A server tma cannot read is not a clean bill of health: `--check` must say so and exit non-zero
/// rather than print `hooks OK` off an empty hook list.
#[test]
fn check_fails_when_the_tmux_server_cannot_be_read() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    assert!(s.install_hooks(&["claude", "--yes"]).status.success());
    assert!(
        s.install_hooks(&["--check"]).status.success(),
        "--check passes while the server is up"
    );

    // Everything file-side stays wired; only the server goes away.
    common::kill_scratch_server(&s.socket);
    let out = s.install_hooks(&["--check"]);
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "--check must fail when the tmux server is unreachable: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        report.contains("cannot verify tmux hooks"),
        "the report names the unreadable server: {report}"
    );
}

/// A hook entry pointing at a binary that is no longer there passes an ownership substring match but
/// is dead: `--check` must call it stale and install must repoint it in place, not append a second entry.
#[test]
fn check_detects_a_moved_binary_and_install_repoints_the_hook() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();
    assert!(s.install_hooks(&["claude", "--yes"]).status.success());
    assert!(s.install_hooks(&["--check"]).status.success());

    // The binary moves: the recorded index still holds a clear-attention entry, but one that runs a
    // path that no longer exists (what a pre-late-binding install left behind).
    assert!(s
        .tmux(&[
            "set-hook",
            "-g",
            "after-select-pane[0]",
            "run-shell \"/nonexistent/tma clear-attention '#{pane_id}'\"",
        ])
        .status
        .success());
    let out = s.install_hooks(&["--check"]);
    assert!(
        !out.status.success(),
        "--check must fail on a hook that runs a different binary"
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("after-select-pane") && report.contains("stale"),
        "the report names the stale hook: {report}"
    );

    // Install rewrites it at the same index.
    assert!(s.install_hooks(&["claude", "--yes"]).status.success());
    let shown = String::from_utf8_lossy(&s.tmux(&["show-hooks", "-g", "after-select-pane"]).stdout)
        .to_string();
    assert!(
        !shown.contains("/nonexistent/tma"),
        "the dead command is gone: {shown}"
    );
    assert_eq!(
        shown
            .lines()
            .filter(|l| l.contains("clear-attention"))
            .count(),
        1,
        "repointed in place, not appended alongside: {shown}"
    );
    assert!(
        s.install_hooks(&["--check"]).status.success(),
        "--check passes once the hook is repointed"
    );
}

/// `install-hooks <agent> --check` scopes the drift report (and exit code) to the named agent: a
/// sibling's stale wiring fails the bare global `--check` but must NOT fail a scoped, correct one.
#[test]
fn check_scopes_to_named_agent() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();

    // Wire claude cleanly (wrapper + settings.json + tmux server hooks).
    assert!(s.install_hooks(&["claude", "--yes"]).status.success());

    // Introduce drift for a SIBLING agent (codex): a tma `notify` pointing at a different
    // wrapper path is `HookWiring::Incomplete` (stale), which the global check reports.
    std::fs::create_dir_all(s.codex_config().parent().unwrap()).unwrap();
    std::fs::write(
        s.codex_config(),
        "notify = [\"/nonexistent/tma-hook\", \"codex\", \"notify\"]\n",
    )
    .unwrap();

    // Bare `--check` (global) surfaces the codex drift and fails.
    let global = s.install_hooks(&["--check"]);
    assert!(
        !global.status.success(),
        "bare --check must report the sibling codex drift"
    );
    assert!(
        String::from_utf8_lossy(&global.stderr).contains("codex"),
        "global report names codex: {}",
        String::from_utf8_lossy(&global.stderr)
    );

    // `claude --check` scopes past the codex drift → passes (claude itself is fully wired).
    let scoped = s.install_hooks(&["claude", "--check"]);
    assert!(
        scoped.status.success(),
        "claude --check must ignore the sibling codex drift: {}",
        String::from_utf8_lossy(&scoped.stderr)
    );

    // `codex --check` scopes to codex and surfaces its own drift → fails.
    let codex = s.install_hooks(&["codex", "--check"]);
    assert!(
        !codex.status.success(),
        "codex --check must report codex's own drift"
    );
}

/// Uninstall removes exactly the *recorded* indexes, so a stray clear-attention entry tma never
/// recorded survives (a plain substring sweep would delete it too).
#[test]
fn uninstall_removes_only_recorded_indexes() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();

    // Install: our clear-attention hook lands at after-select-pane[0], recorded in state.
    assert!(s.install_hooks(&["claude", "--yes"]).status.success());

    // A second clear-attention entry tma did NOT record (a stray). `is_ours` matches it by substring,
    // but it is absent from hooks-state.toml, so a recorded-index uninstall must leave it untouched.
    assert!(s
        .tmux(&[
            "set-hook",
            "-ga",
            "after-select-pane",
            "run-shell 'clear-attention decoy-marker'",
        ])
        .status
        .success());

    assert!(s
        .install_hooks(&["claude", "--uninstall", "--yes"])
        .status
        .success());

    let shown = String::from_utf8_lossy(&s.tmux(&["show-hooks", "-g", "after-select-pane"]).stdout)
        .to_string();
    assert!(
        shown.contains("decoy-marker"),
        "non-recorded stray must survive uninstall: {shown}"
    );
    assert!(
        !shown.contains("hook_pane"),
        "our recorded entry (bound via #{{hook_pane}}) must be removed: {shown}"
    );
}

/// `--check` verifies our entry sits at its *recorded* index, not merely that some clear-attention
/// entry exists in the array (which a substring check would accept even after our entry was wiped).
#[test]
fn check_uses_recorded_index_not_substring() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();

    assert!(s.install_hooks(&["claude", "--yes"]).status.success());

    // A non-recorded clear-attention stray at after-select-pane[1].
    assert!(s
        .tmux(&[
            "set-hook",
            "-ga",
            "after-select-pane",
            "run-shell 'clear-attention decoy-marker'",
        ])
        .status
        .success());
    // Sanity: --check still passes (recorded index 0 is still ours).
    assert!(
        s.install_hooks(&["--check"]).status.success(),
        "--check should pass while the recorded index is intact"
    );

    // Wipe our recorded entry at index 0; the stray clear-attention remains at index 1.
    assert!(s
        .tmux(&["set-hook", "-gu", "after-select-pane[0]"])
        .status
        .success());
    let out = s.install_hooks(&["--check"]);
    assert!(
        !out.status.success(),
        "--check must fail: the recorded index no longer holds our entry (a substring \
         check would have falsely passed on the stray)"
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("after-select-pane"),
        "report names the missing hook: {report}"
    );
}

/// Per-server hooks-state: tmux `set-hook -g` indexes are PER-SERVER, so the record is keyed per
/// server. Two scratch servers share ONE config dir; uninstalling on one must leave the OTHER's hooks
/// and state file intact (a single global file would strip the wrong server's indexes).
#[test]
fn hooks_state_is_keyed_per_server() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let a = Scratch::new();
    let b = Scratch::new();
    for s in [&a, &b] {
        assert!(s
            .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
            .status
            .success());
        std::fs::write(s.settings(), "{}\n").unwrap();
    }

    // ONE shared config dir: the per-server keying (not separate dirs) is what must isolate the
    // two records — using distinct dirs would trivially isolate and prove nothing.
    let shared_cfg = a.workdir.join("shared_cfg");
    std::fs::create_dir_all(&shared_cfg).unwrap();

    let install = |s: &Scratch, extra: &[&str]| -> Output {
        let mut args: Vec<String> = vec!["install-hooks".into()];
        args.extend(extra.iter().map(|x| x.to_string()));
        args.extend([
            "--settings".into(),
            s.settings().display().to_string(),
            "--config-dir".into(),
            shared_cfg.display().to_string(),
            "--wrapper-path".into(),
            s.wrapper().display().to_string(),
            "--codex-config".into(),
            s.codex_config().display().to_string(),
            "--codex-hooks".into(),
            s.codex_hooks().display().to_string(),
            // The bare `--check` below inspects EVERY bundled agent, so every agent path has to be
            // pinned to the scratch (SAFETY): unpinned, it reads the developer's own wired
            // ~/.gemini, ~/.cursor, ~/.pi and opencode configs and reports their (correct,
            // real-install) wrapper path as drift against this test's.
            "--gemini-settings".into(),
            s.gemini_settings().display().to_string(),
            "--cursor-hooks".into(),
            s.cursor_hooks().display().to_string(),
            "--cursor-cli-config".into(),
            s.cursor_cli_config().display().to_string(),
            "--pi-extension".into(),
            s.pi_extension().display().to_string(),
            "--opencode-plugin".into(),
            s.opencode_plugin().display().to_string(),
            "--socket-name".into(),
            s.socket.clone(),
        ]);
        Command::new(env!("CARGO_BIN_EXE_tma"))
            .args(&args)
            .env("TMA_BIN", env!("CARGO_BIN_EXE_tma"))
            .env("TMA_CONFIG", common::empty_config_path())
            .output()
            .expect("spawn tma")
    };

    let keyed_files = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&shared_cfg)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.starts_with("hooks-state-") && n.ends_with(".toml"))
            .collect();
        names.sort();
        names
    };

    // Install the tmux hooks on BOTH servers, sharing the config dir.
    assert!(
        install(&a, &["claude", "--yes"]).status.success(),
        "install on A"
    );
    assert!(
        install(&b, &["claude", "--yes"]).status.success(),
        "install on B"
    );

    // Two distinct keyed state files (one per server) — no single global record.
    assert_eq!(
        keyed_files().len(),
        2,
        "one keyed hooks-state file per server: {:?}",
        keyed_files()
    );

    // Uninstall on A only.
    assert!(
        install(&a, &["claude", "--uninstall", "--yes"])
            .status
            .success(),
        "uninstall on A"
    );

    // A's hook is gone.
    let a_hooks =
        String::from_utf8_lossy(&a.tmux(&["show-hooks", "-g", "after-select-pane"]).stdout)
            .to_string();
    assert!(
        !a_hooks.contains("clear-attention"),
        "A's hook removed on its uninstall: {a_hooks}"
    );

    // B's hook SURVIVES — the clobber/wrong-index bug this test guards.
    let b_hooks =
        String::from_utf8_lossy(&b.tmux(&["show-hooks", "-g", "after-select-pane"]).stdout)
            .to_string();
    assert!(
        b_hooks.contains("clear-attention"),
        "B's hook must survive A's uninstall: {b_hooks}"
    );

    // Exactly B's keyed state file remains, and B's `--check` still passes (indexes intact).
    assert_eq!(
        keyed_files().len(),
        1,
        "only B's keyed state file remains: {:?}",
        keyed_files()
    );
    assert!(
        install(&b, &["--check"]).status.success(),
        "B --check must pass after A's uninstall"
    );
}

/// Codex: `install-hooks codex` merges the `notify` key into `config.toml` AND the hooks.json events
/// (with the trust-gate notice), format-preserving and byte-identical on both files, never clobbering
/// a user's own notify (Codex allows only one).
#[test]
fn codex_notify_install_uninstall_check_round_trip() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // An original config.toml with a comment and unrelated keys the installer must preserve.
    let original = "# my codex config\nmodel = \"gpt-5.2\"\nmodel_reasoning_effort = \"medium\"\n";
    std::fs::create_dir_all(s.codex_config().parent().unwrap()).unwrap();
    std::fs::write(s.codex_config(), original).unwrap();

    // --- install: notify merged in, everything else preserved ---
    let out = s.install_hooks(&["codex", "--yes"]);
    assert!(
        out.status.success(),
        "codex install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let installed = std::fs::read_to_string(s.codex_config()).unwrap();
    assert!(
        installed.contains("# my codex config") && installed.contains("model = \"gpt-5.2\""),
        "preserved user content + comment: {installed}"
    );
    let wrapper = s.wrapper();
    assert!(
        installed.contains(&format!("\"{}\"", wrapper.display()))
            && installed.contains("\"codex\"")
            && installed.contains("\"notify\""),
        "notify array references the wrapper: {installed}"
    );

    // hooks.json: the live-verified events are wired, notify is not (config.toml's channel),
    // and the install printed the trust-gate next step (hooks are inert until trusted in-TUI).
    let hooks = std::fs::read_to_string(s.codex_hooks()).unwrap();
    for event in ["SessionStart", "UserPromptSubmit", "SessionEnd"] {
        assert!(
            hooks.contains(&format!("{} codex {event}", wrapper.display())),
            "hooks.json wires {event}: {hooks}"
        );
    }
    assert!(
        !hooks.contains("codex notify"),
        "notify never lands in hooks.json: {hooks}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("trust"),
        "install must print the trust-gate notice"
    );

    // --check passes right after install.
    assert!(
        s.install_hooks(&["--check"]).status.success(),
        "codex --check should pass right after install"
    );

    // Re-install is idempotent (byte-identical, both files).
    let before = std::fs::read_to_string(s.codex_config()).unwrap();
    let before_hooks = std::fs::read_to_string(s.codex_hooks()).unwrap();
    assert!(s.install_hooks(&["codex", "--yes"]).status.success());
    assert_eq!(
        before,
        std::fs::read_to_string(s.codex_config()).unwrap(),
        "re-install must be byte-identical (idempotent)"
    );
    assert_eq!(
        before_hooks,
        std::fs::read_to_string(s.codex_hooks()).unwrap(),
        "hooks.json re-install must be byte-identical (idempotent)"
    );

    // --- uninstall: config restored byte-for-byte, hooks.json entries removed ---
    assert!(s
        .install_hooks(&["codex", "--uninstall", "--yes"])
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(s.codex_config()).unwrap(),
        original,
        "uninstall restores config.toml byte-for-byte"
    );
    let hooks_after = std::fs::read_to_string(s.codex_hooks()).unwrap();
    assert!(
        !hooks_after.contains("tma-hook"),
        "uninstall removes every tma hooks.json entry: {hooks_after}"
    );
}

/// Codex safety: a user's own `notify` program is never clobbered — Codex allows only one,
/// so the install refuses and leaves the config untouched.
#[test]
fn codex_install_refuses_to_clobber_foreign_notify() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    let foreign = "notify = [\"my-own-notifier\", \"--flag\"]\n";
    std::fs::create_dir_all(s.codex_config().parent().unwrap()).unwrap();
    std::fs::write(s.codex_config(), foreign).unwrap();

    let out = s.install_hooks(&["codex", "--yes"]);
    assert!(
        !out.status.success(),
        "install must refuse when a foreign notify exists"
    );
    assert_eq!(
        std::fs::read_to_string(s.codex_config()).unwrap(),
        foreign,
        "a foreign notify must be left untouched"
    );

    // Uninstall likewise leaves the foreign notify alone.
    let _ = s.install_hooks(&["codex", "--uninstall", "--yes"]);
    assert_eq!(
        std::fs::read_to_string(s.codex_config()).unwrap(),
        foreign,
        "uninstall must not remove a foreign notify"
    );
}

/// Contamination guard: `install-hooks` for a hookless agent must refuse and write NOTHING, never
/// falling back to the Claude adapter and stamping entries into `settings.json` (a latent-bug regression).
#[test]
fn install_refuses_hookless_agent_without_writing() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();

    // A hookless manifest (no [hooks] block) in an isolated manifest dir loaded via
    // `--manifest-dir`, so the agent exists but is not hook-capable.
    let manifest_dir = s.workdir.join("manifests");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::write(
        manifest_dir.join("gemini.toml"),
        "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"gemini\"]\n[capture]\nvisible = []\n",
    )
    .unwrap();
    let md = manifest_dir.display().to_string();

    let out = s.install_hooks(&["gemini", "--yes", "--manifest-dir", &md]);
    assert!(
        !out.status.success(),
        "install-hooks must refuse a hookless agent"
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("gemini") && report.to_lowercase().contains("hookless"),
        "refusal must name the agent and reason: {report}"
    );
    // The Claude settings file must be untouched (never created): the contamination path.
    assert!(
        !s.settings().exists(),
        "no config file may be written for a refused agent"
    );

    // Symmetric: uninstall likewise refuses and writes nothing.
    let out = s.install_hooks(&["gemini", "--uninstall", "--yes", "--manifest-dir", &md]);
    assert!(
        !out.status.success(),
        "uninstall must refuse a hookless agent too"
    );
    assert!(
        !s.settings().exists(),
        "uninstall must not create a config file for a refused agent"
    );
}

/// `--check` verifies the wrapper the settings entries reference exists
/// on disk — its silent absence would otherwise leave every wired hook a no-op.
#[test]
fn check_reports_missing_wrapper() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();

    assert!(s.install_hooks(&["claude", "--yes"]).status.success());
    assert!(
        s.install_hooks(&["--check"]).status.success(),
        "--check should pass right after install"
    );

    // The wrapper dies (rebuild moved it, cleanup removed it): settings still reference it.
    std::fs::remove_file(s.wrapper()).unwrap();
    let out = s.install_hooks(&["--check"]);
    assert!(
        !out.status.success(),
        "--check must fail when the referenced wrapper is missing"
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("wrapper"),
        "report names the missing wrapper: {report}"
    );
}

/// `--wrapper-ref bare` writes the NAME `tma-hook` into every mechanism, not the install-time path:
/// the shell-run command strings (Claude) and the argv arrays (Codex's `notify`) alike, which is the
/// whole point — a `$HOME`-relative string would be passed through literally by the argv ones.
/// The wrapper file still lands next to the binary; only what the configs say about it changes.
#[test]
fn bare_wrapper_ref_writes_the_name_for_shell_and_argv_agents() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();
    let path = s.path_with_wrapper_dir();
    let on_path = [("PATH", path.as_str())];

    for agent in ["claude", "codex"] {
        let out = s.install_hooks_env(&[agent, "--yes", "--wrapper-ref", "bare"], &on_path);
        assert!(
            out.status.success(),
            "{agent} install failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let abs = s.wrapper().display().to_string();
    assert!(
        s.wrapper().is_file(),
        "the file still goes next to the binary"
    );

    // Claude: a shell-run command string.
    let settings = std::fs::read_to_string(s.settings()).unwrap();
    assert!(
        settings.contains("tma-hook claude Notification") && !settings.contains(&abs),
        "claude names the wrapper: {settings}"
    );
    // Codex: `notify` is argv, spawned with no shell at all.
    let codex = std::fs::read_to_string(s.codex_config()).unwrap();
    assert!(
        codex.contains("\"tma-hook\"") && !codex.contains(&abs),
        "codex notify names the wrapper: {codex}"
    );

    // `--check` agrees with what install wrote, given the same posture.
    assert!(
        s.install_hooks_env(&["--check", "--wrapper-ref", "bare"], &on_path)
            .status
            .success(),
        "--check must pass right after a bare install"
    );

    // And it is honest when the posture disagrees: checking for absolute wiring against a bare
    // install is real drift, since the two write different strings. Re-installing repoints them.
    let out = s.install_hooks(&["--check"]);
    assert!(
        !out.status.success(),
        "an absolute --check over bare wiring is drift, not a pass"
    );
}

/// A bare reference the agent's `$PATH` cannot answer is a silent outage: the wrapper's contract is
/// to exit 0 when it cannot resolve `tma`, and an unresolvable wrapper never runs at all, so nothing
/// anywhere reports a failure. Install refuses instead of wiring hooks that will never fire.
#[test]
fn bare_wrapper_ref_refuses_when_it_is_not_on_path() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // The wrapper's directory is a fresh temp dir, and any real tma-hook the developer has
    // installed is stripped out, so the name resolves nowhere.
    let path = s.path_without_wrapper();
    let out = s.install_hooks_env(
        &["claude", "--yes", "--wrapper-ref", "bare"],
        &[("PATH", path.as_str())],
    );
    assert!(
        !out.status.success(),
        "install must refuse an unfindable reference"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PATH") && err.contains("tma-hook"),
        "the refusal names the problem: {err}"
    );
    assert!(
        !s.settings().exists(),
        "nothing may be wired to a reference that does not resolve"
    );
}

/// `--all` acts on the agents that ALREADY carry wiring, which is what makes it the repoint tool: a
/// `wrapper_ref` switch rewrites every wired config in one command. It must not touch an agent that
/// was never wired (that would create a config file for an agent you do not use), and `--uninstall`
/// over the same set unwires exactly those.
#[test]
fn all_repoints_every_wired_agent_and_leaves_the_rest_alone() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    std::fs::write(s.settings(), "{}\n").unwrap();

    // Two agents wired the default way; gemini deliberately left alone.
    for agent in ["claude", "codex"] {
        assert!(
            s.install_hooks(&[agent, "--yes"]).status.success(),
            "wiring {agent}"
        );
    }
    assert!(!s.gemini_settings().exists(), "gemini starts unwired");

    // The repoint: one command switches both to the portable reference.
    let path = s.path_with_wrapper_dir();
    let out = s.install_hooks_env(
        &["--all", "--yes", "--wrapper-ref", "bare"],
        &[("PATH", path.as_str())],
    );
    assert!(
        out.status.success(),
        "--all failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let abs = s.wrapper().display().to_string();
    let settings = std::fs::read_to_string(s.settings()).unwrap();
    let codex = std::fs::read_to_string(s.codex_config()).unwrap();
    assert!(
        settings.contains("tma-hook claude Notification") && !settings.contains(&abs),
        "claude repointed: {settings}"
    );
    assert!(
        codex.contains("\"tma-hook\"") && !codex.contains(&abs),
        "codex repointed: {codex}"
    );
    assert!(
        !s.gemini_settings().exists(),
        "--all must not wire an agent that was never wired"
    );
    assert!(
        s.install_hooks_env(
            &["--check", "--wrapper-ref", "bare"],
            &[("PATH", path.as_str())]
        )
        .status
        .success(),
        "--check passes over the repointed set"
    );

    // Symmetric: --all --uninstall clears exactly the wired set.
    let out = s.install_hooks_env(
        &["--all", "--uninstall", "--yes", "--wrapper-ref", "bare"],
        &[("PATH", path.as_str())],
    );
    assert!(
        out.status.success(),
        "--all --uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let settings = std::fs::read_to_string(s.settings()).unwrap();
    assert!(!settings.contains("tma-hook"), "claude unwired: {settings}");
    let codex = std::fs::read_to_string(s.codex_config()).unwrap();
    assert!(!codex.contains("tma-hook"), "codex unwired: {codex}");
}

/// `--all` over an unwired machine is a no-op that says so, not a failure and not a sweep that
/// wires every agent tma ships an adapter for. And it refuses to be given an agent name, which
/// would be asking for two different sets at once.
#[test]
fn all_is_a_clean_no_op_when_nothing_is_wired_and_refuses_an_agent() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    let out = s.install_hooks(&["--all", "--yes"]);
    assert!(
        out.status.success(),
        "nothing to do is not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("no agent is wired"),
        "it says why it did nothing: {report}"
    );
    assert!(
        !s.settings().exists() && !s.gemini_settings().exists(),
        "--all must never create a config for an agent that was never wired"
    );

    let out = s.install_hooks(&["claude", "--all", "--yes"]);
    assert!(
        !out.status.success(),
        "--all with an agent is a usage error"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--all"),
        "the error names the flag"
    );
}

/// `install-hooks gemini` wires the Claude-shape `hooks` block into gemini's OWN settings.json,
/// byte-identical round-trip, `gemini --check` scoped, proving it targets its own file and prints
/// the folder-trust next step.
#[test]
fn gemini_install_uninstall_round_trip_and_scoped_check() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // Claude settings.json stays empty (the gemini adapter must not write there); a pre-existing
    // gemini settings with an unrelated auth block the installer must preserve.
    std::fs::write(s.settings(), "{}\n").unwrap();
    let original =
        "{\n  \"security\": {\n    \"auth\": {\n      \"selectedType\": \"gemini-api-key\"\n    }\n  }\n}\n";
    std::fs::create_dir_all(s.gemini_settings().parent().unwrap()).unwrap();
    std::fs::write(s.gemini_settings(), original).unwrap();

    // --- install ---
    let out = s.install_hooks(&["gemini", "--yes"]);
    assert!(
        out.status.success(),
        "gemini install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The folder-trust next step is printed (installer caveat).
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("folder-trust"),
        "install must print the gemini folder-trust next step"
    );

    // gemini settings.json wired with the native event names, preserving prior content.
    let installed = std::fs::read_to_string(s.gemini_settings()).unwrap();
    assert!(
        installed.contains("\"selectedType\": \"gemini-api-key\""),
        "preserved unrelated gemini settings"
    );
    let wrapper_cmd = format!("{} gemini SessionStart", s.wrapper().display());
    assert!(installed.contains(&wrapper_cmd), "wired SessionStart");
    assert!(installed.contains("gemini AfterAgent"), "wired AfterAgent");
    assert!(installed.contains("gemini BeforeTool"), "wired BeforeTool");
    // The claude settings.json must be untouched (no cross-contamination).
    assert_eq!(
        std::fs::read_to_string(s.settings()).unwrap(),
        "{}\n",
        "gemini install must not write claude's settings.json"
    );

    // --- gemini --check passes ---
    assert!(
        s.install_hooks(&["gemini", "--check"]).status.success(),
        "gemini --check should pass right after install"
    );

    // --- uninstall: byte-identical ---
    let out = s.install_hooks(&["gemini", "--uninstall", "--yes"]);
    assert!(
        out.status.success(),
        "gemini uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(s.gemini_settings()).unwrap(),
        original,
        "uninstall restores gemini settings.json byte-for-byte"
    );
}

/// `install-hooks cursor` writes cursor's OWN flat JSON shape into `~/.cursor/hooks.json` (not the
/// Claude nested shape) AND the statusLine context shim into `~/.cursor/cli-config.json`,
/// preserving unrelated user content in both, round-tripping byte-identically, and scoping
/// `cursor --check` over both files.
#[test]
fn cursor_install_uninstall_round_trip_and_scoped_check() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());

    // Claude settings.json stays empty (no cross-contamination). A pre-existing cursor hooks.json
    // with an unrelated user hook the installer must preserve.
    std::fs::write(s.settings(), "{}\n").unwrap();
    let original =
        "{\n  \"version\": 1,\n  \"hooks\": {\n    \"afterFileEdit\": [\n      {\n        \"command\": \"my-formatter.sh\"\n      }\n    ]\n  }\n}\n";
    std::fs::create_dir_all(s.cursor_hooks().parent().unwrap()).unwrap();
    std::fs::write(s.cursor_hooks(), original).unwrap();
    // A pre-existing cli-config.json with the user's own statusLine command AND a `padding` sibling
    // the shim must chain and preserve byte-faithfully.
    let cli_original =
        "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"my-line.sh\",\n    \"padding\": 2\n  }\n}\n";
    std::fs::write(s.cursor_cli_config(), cli_original).unwrap();

    // --- install ---
    // `--statusline` because the shim is opt-in: this suite covers the round trip of BOTH cursor
    // channels, so it asks for the one a plain install now leaves alone.
    let out = s.install_hooks(&["cursor", "--yes", "--statusline"]);
    assert!(
        out.status.success(),
        "cursor install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let installed = std::fs::read_to_string(s.cursor_hooks()).unwrap();
    // The user's own hook survives, and the flat cursor entry shape is used (NOT Claude's nested
    // {"hooks":[{"type":"command",...}]}).
    assert!(
        installed.contains("my-formatter.sh"),
        "preserved unrelated user hook"
    );
    assert!(
        !installed.contains("\"type\": \"command\""),
        "cursor uses the flat {{command}} entry, not the Claude nested shape"
    );
    let wrapper_cmd = format!("{} cursor sessionStart", s.wrapper().display());
    assert!(installed.contains(&wrapper_cmd), "wired sessionStart");
    assert!(installed.contains("cursor stop"), "wired stop");
    assert!(installed.contains("cursor preToolUse"), "wired preToolUse");
    // The claude settings.json must be untouched.
    assert_eq!(
        std::fs::read_to_string(s.settings()).unwrap(),
        "{}\n",
        "cursor install must not write claude's settings.json"
    );

    // The statusLine shim landed in cli-config.json: cursor forward, chained user command, padding kept.
    let cli_installed = std::fs::read_to_string(s.cursor_cli_config()).unwrap();
    assert!(
        cli_installed.contains("event --agent cursor --kind context"),
        "the statusLine shim forwards to the cursor context intake: {cli_installed}"
    );
    assert!(
        cli_installed.contains("my-line.sh"),
        "the user's statusLine command is chained, not replaced"
    );
    assert!(
        cli_installed.contains("\"padding\": 2"),
        "the unknown padding key survives byte-faithfully: {cli_installed}"
    );

    // --- cursor --check passes (both hooks.json and the cli-config.json shim) ---
    // `--check --statusline` mirrors the install: a check has to state the same intent, since a bare
    // one reports a shim it was not told to expect.
    assert!(
        s.install_hooks(&["cursor", "--check", "--statusline"])
            .status
            .success(),
        "cursor --check should pass right after install"
    );

    // A clobbered statusLine (the forward overwritten) fails a check that asked for the shim. Bare,
    // the same file is simply a statusline of the user's own, which is not drift.
    std::fs::write(s.cursor_cli_config(), cli_original).unwrap();
    assert!(
        !s.install_hooks(&["cursor", "--check", "--statusline"])
            .status
            .success(),
        "a clobbered statusLine shim must fail `--check --statusline`"
    );
    // And a bare check catches it too, because the `--statusline` install recorded the opt-in: this
    // agent asked for the shim, so its disappearance is drift without having to restate the flag.
    assert!(
        !s.install_hooks(&["cursor", "--check"]).status.success(),
        "an opted-in agent's clobbered shim is drift for a bare --check"
    );
    // Disowning it explicitly passes, which is the escape hatch for someone who changed their mind.
    assert!(
        s.install_hooks(&["cursor", "--check", "--no-statusline"])
            .status
            .success(),
        "--check --no-statusline is satisfied by the user's own statusLine"
    );
    // Reinstall to restore the shim before uninstall.
    assert!(s
        .install_hooks(&["cursor", "--yes", "--statusline"])
        .status
        .success());

    // --- uninstall: both files byte-identical to their originals ---
    let out = s.install_hooks(&["cursor", "--uninstall", "--yes"]);
    assert!(
        out.status.success(),
        "cursor uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(s.cursor_hooks()).unwrap(),
        original,
        "uninstall restores cursor hooks.json byte-for-byte"
    );
    assert_eq!(
        std::fs::read_to_string(s.cursor_cli_config()).unwrap(),
        cli_original,
        "uninstall restores cursor cli-config.json byte-for-byte"
    );
}

/// The pi extension round-trip end-to-end through the real binary: `install-hooks pi` honors the
/// `--pi-extension` override, drops the JS extension referencing the wrapper, `pi --check` passes,
/// and uninstall removes exactly our file. pi has no JSON hook block, so the extension IS the wiring.
#[test]
fn pi_install_uninstall_round_trip_and_scoped_check() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    assert!(s
        .tmux(&["new-session", "-d", "-s", "s1", "exec sleep 100000"])
        .status
        .success());
    // Claude settings.json stays empty (no cross-contamination); pi does not touch it.
    std::fs::write(s.settings(), "{}\n").unwrap();

    // --- install: PiAdapter writes the extension at the --pi-extension override path ---
    let ext = s.pi_extension();
    assert!(!ext.exists(), "no extension before install");
    let out = s.install_hooks(&["pi", "--yes"]);
    assert!(
        out.status.success(),
        "pi install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The printed next-step confirms the install landed (the shared post-install line).
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("installed hooks for pi"),
        "install prints the pi next-step: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let installed = std::fs::read_to_string(&ext).expect("extension written at the override path");
    // The extension shells out to the wrapper as `tma-hook pi <event>` (the JS bridge).
    assert!(
        installed.contains(&s.wrapper().display().to_string()),
        "extension references the wrapper path"
    );
    assert!(
        installed.contains("pi"),
        "extension carries the pi agent token"
    );
    // Claude settings.json untouched.
    assert_eq!(
        std::fs::read_to_string(s.settings()).unwrap(),
        "{}\n",
        "pi install must not write claude's settings.json"
    );

    // --- pi --check passes (marker present + wrapper current) ---
    assert!(
        s.install_hooks(&["pi", "--check"]).status.success(),
        "pi --check should pass right after install"
    );

    // --- uninstall: our extension file is removed (symmetric) ---
    let out = s.install_hooks(&["pi", "--uninstall", "--yes"]);
    assert!(
        out.status.success(),
        "pi uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !ext.exists(),
        "uninstall removes exactly our extension file"
    );
}
