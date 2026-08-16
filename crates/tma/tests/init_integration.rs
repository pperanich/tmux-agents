//! `tma init` acceptance: the whole wizard driven once, end to end, against a scratch tmux server
//! and a fully isolated environment.
//!
//! SAFETY: the child runs with a cleared environment (so no ambient `TMUX`/`TMUX_PANE`/`TMA_*`
//! reaches it) and a private `HOME` **and** `XDG_CONFIG_HOME` — both, because the config paths
//! init writes resolve through XDG first and `HOME` second. Its `PATH` starts with a temp dir
//! holding fake `claude`/`codex` binaries, which is what the detection step is meant to find.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tma_test_support as common;

/// The isolated environment one `tma init` run happens in.
struct Env {
    scratch: common::Scratch,
}

impl Env {
    fn new() -> Env {
        let scratch = common::Scratch::new("init");
        let env = Env { scratch };
        for dir in [env.home(), env.xdg(), env.bin()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // The agents the detection step must find. Contents are irrelevant: init only asks whether
        // an executable of that name is on PATH.
        for agent in ["claude", "codex"] {
            let path = env.bin().join(agent);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        // tmux itself has to stay reachable: the child spawns it for every server read.
        if let Some(tmux) = which("tmux") {
            std::os::unix::fs::symlink(tmux, env.bin().join("tmux")).unwrap();
        }
        env
    }

    fn home(&self) -> PathBuf {
        self.scratch.workdir.join("home")
    }
    fn xdg(&self) -> PathBuf {
        self.scratch.workdir.join("xdg")
    }
    fn bin(&self) -> PathBuf {
        self.scratch.workdir.join("bin")
    }

    /// `tma init …` against the scratch server, with the environment cleared down to the isolated
    /// HOME/XDG/PATH (plus `TMUX_TMPDIR`, which is where the scratch socket lives when it is set).
    fn init(&self, extra: &[&str]) -> Output {
        let mut cmd = Command::new(common::tma_bin());
        cmd.env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.xdg())
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            // The hook wrapper's default site is next to the tma binary, i.e. the repo's target
            // dir; keep even that write inside the scratch (SAFETY).
            .env("TMA_WRAPPER_PATH", self.bin().join("tma-hook"))
            .env("TMA_CONFIG", common::empty_config_path())
            .arg("init")
            .args(extra)
            .arg("--socket-name")
            .arg(&self.scratch.socket);
        if let Some(tmpdir) = std::env::var_os("TMUX_TMPDIR") {
            cmd.env("TMUX_TMPDIR", tmpdir);
        }
        cmd.output().expect("spawn tma init")
    }
}

/// The first executable `name` on the test process's own PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// One `tma init --yes` run does the whole setup: it names the agents it found, wires each one's
/// hooks, records the tmux server hooks, installs the keybindings and the `source-file` line, and
/// leaves `status-right` alone while printing what to add to it.
#[test]
fn init_wires_the_detected_agents_the_keys_and_nothing_else() {
    if !common::tmux_available() {
        return;
    }
    let env = Env::new();
    assert!(env
        .scratch
        .tmux(&["new-session", "-d", "-s", "s1"])
        .status
        .success());

    let out = env.init(&["--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "init failed: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Detection: the two fake binaries are found, and an agent with no binary is reported as
    // skipped rather than silently dropped.
    assert!(
        stdout.contains(&format!(
            "claude    found ({}",
            env.bin().join("claude").display()
        )),
        "the found binary is named: {stdout}"
    );
    assert!(stdout.contains("codex     found ("), "{stdout}");
    assert!(
        stdout.contains("gemini    not found (skipped"),
        "an absent agent is skipped with its manual command: {stdout}"
    );

    // Hooks: each detected agent's own config, plus the shared wrapper and the per-server record.
    let claude_settings = read(&env.home().join(".claude/settings.json"));
    assert!(
        claude_settings.contains("tma-hook claude SessionStart"),
        "claude's hooks are wired: {claude_settings}"
    );
    assert!(read(&env.home().join(".codex/hooks.json")).contains("tma-hook codex"));
    assert!(env.bin().join("tma-hook").is_file(), "wrapper written");
    let state: Vec<PathBuf> = std::fs::read_dir(env.xdg().join("tma"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("hooks-state-"))
        })
        .collect();
    assert_eq!(state.len(), 1, "one per-server hooks record: {state:?}");
    assert!(read(&state[0]).contains("after-select-pane"));

    // Keys: the managed file and exactly one marked `source-file` line in the resolved config.
    let managed = read(&env.xdg().join("tma/tmux.conf"));
    assert!(managed.contains("bind-key a display-popup"), "{managed}");
    let conf = read(&env.xdg().join("tmux/tmux.conf"));
    assert_eq!(conf.matches("# tma keys").count(), 1, "{conf}");

    // The status line: printed, never written. Neither the server option nor the user's config
    // gained anything, which is the whole point of the step.
    assert!(
        stdout.contains("status-right does NOT run `tma status`")
            && stdout.contains("set -g status-right '#(tma status)"),
        "the exact line and where it goes: {stdout}"
    );
    assert!(
        !conf.contains("status-right"),
        "init wrote no status-right line: {conf}"
    );
    let status_right = env.scratch.get("", "#{status-right}");
    assert!(
        !status_right.contains("tma status"),
        "the server's status-right is untouched: {status_right}"
    );

    // The closing doctor report ran, and the server hooks it reports are the ones just installed.
    assert!(
        stdout.contains("hooks:   after-select-pane \u{2713}"),
        "the doctor report is part of the output: {stdout}"
    );

    // Re-running is idempotent: every write is already current, and it still exits 0.
    let again = env.init(&["--yes"]);
    let stdout = String::from_utf8_lossy(&again.stdout).to_string();
    assert!(again.status.success(), "second init failed: {stdout}");
    assert!(
        stdout.contains("keybindings already installed and current; skipping"),
        "the keys step is a clean skip the second time: {stdout}"
    );
    assert_eq!(read(&env.xdg().join("tmux/tmux.conf")), conf, "conf stable");
    assert_eq!(
        read(&env.home().join(".claude/settings.json")),
        claude_settings,
        "the agent config is byte-identical on a re-run"
    );
}

/// Without `--yes` and with no terminal behind stdin, every confirmation declines. init says so up
/// front, writes nothing, and reports the failure rather than claiming a setup it did not do.
#[test]
fn init_without_yes_declines_and_writes_nothing_when_stdin_is_not_a_tty() {
    if !common::tmux_available() {
        return;
    }
    let env = Env::new();
    assert!(env
        .scratch
        .tmux(&["new-session", "-d", "-s", "s1"])
        .status
        .success());

    let out = env.init(&[]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!out.status.success(), "a declined setup is a failure");
    assert!(
        stdout.contains("stdin is not a terminal"),
        "the reason comes before the first aborted write: {stdout}"
    );
    assert!(
        !env.home().join(".claude/settings.json").exists(),
        "nothing was written"
    );
    assert!(!env.xdg().join("tma/tmux.conf").exists());
}
