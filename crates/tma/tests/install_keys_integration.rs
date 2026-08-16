//! `tma install-keys` acceptance on TEMP config + a scratch tmux server.
//!
//! Split from `install_integration.rs`: these exercise the tmux-config write site (the marked
//! `source-file` line and the bindings file it points at), not the per-agent config plumbing, so
//! they need only the scratch server + a temp config dir. Everything writes to temp paths and a
//! scratch `tmux -L` server, never the user's real `~/.config/tma` or the default tmux server
//! (SAFETY).

use std::path::PathBuf;
use std::process::{Command, Output};

use common::{scratch_tmux, unique_id};
use tma_test_support as common;

/// The trimmed harness: a scratch socket plus a temp workdir, which is all the install-keys write
/// site needs (the agent-config paths and the `install_hooks` builder stay with that suite).
struct Scratch {
    socket: String,
    workdir: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        common::reap_orphan_scratch_servers();
        let unique = unique_id();
        let workdir = std::env::temp_dir().join(format!("tma_install_keys_{unique}"));
        std::fs::create_dir_all(&workdir).unwrap();
        Scratch {
            socket: format!("tma_test_install_keys_{unique}"),
            workdir,
        }
    }

    fn tmux(&self, args: &[&str]) -> Output {
        scratch_tmux(&self.socket, args)
    }

    fn config_dir(&self) -> PathBuf {
        self.workdir.join("cfg")
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

/// `install-keys --mouse` writes a file real tmux accepts: sourcing it on the scratch server must
/// succeed and land all four mouse bindings in the ROOT table. The unit test pins the bytes; only a
/// live tmux proves the nested `if-shell -F` / brace-block quoting parses, and that the `#{...}`
/// conditionals survive into the binding instead of being expanded away at source time.
#[test]
fn install_keys_mouse_bindings_source_into_the_root_table() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new();
    // The scratch server exists only once something runs on it.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "home", "exec sleep 100000"])
        .status
        .success());

    let managed = s.config_dir().join("tmux.conf");
    let conf = s.workdir.join(".tmux.conf");
    let out = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["install-keys", "--mouse", "--yes"])
        .arg("--config-dir")
        .arg(s.config_dir())
        .arg("--conf")
        .arg(&conf)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma install-keys");
    assert!(
        out.status.success(),
        "install-keys failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let sourced = s.tmux(&["source-file", &managed.display().to_string()]);
    assert!(
        sourced.status.success(),
        "tmux rejected the managed file: {}",
        String::from_utf8_lossy(&sourced.stderr)
    );

    let root = String::from_utf8_lossy(&s.tmux(&["list-keys", "-T", "root"]).stdout).to_string();
    for key in [
        "MouseDown1Status ",
        "MouseDown1StatusRight",
        "MouseDown3Status ",
        "MouseDown3StatusRight",
    ] {
        assert!(root.contains(key), "{key} not bound in the root table");
    }
    // The dispatch survived as a live format conditional, not a value expanded at source time.
    assert!(
        root.contains("mouse_status_range"),
        "the range conditional was expanded away: {root}"
    );
    // The sidebar arm parsed as its own branch and sits BEFORE the generic `tma:*` picker arm, so
    // clicking the icon toggles instead of opening the picker.
    let left = root
        .lines()
        .find(|l| l.contains("MouseDown1Status "))
        .expect("the left-click binding is listed");
    assert!(
        left.contains("watch --toggle"),
        "the sidebar arm did not survive sourcing: {left}"
    );
    assert!(
        left.find("tma:sidebar") < left.find("m:tma:*"),
        "the sidebar arm must be matched before the generic tma:* arm: {left}"
    );
    // Without --mouse none of it is written, so a plain install cannot claim the mouse keys.
    let plain = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["install-keys", "--yes"])
        .arg("--config-dir")
        .arg(s.config_dir())
        .arg("--conf")
        .arg(&conf)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma install-keys");
    assert!(plain.status.success());
    assert!(!std::fs::read_to_string(&managed)
        .unwrap()
        .contains("MouseDown"));
}

/// `install-keys` marks the tmux config that already exists rather than an unconditional
/// `~/.tmux.conf`: with an XDG config in place, a freshly created dotfile would sit AHEAD of it in
/// tmux's load order (and shadow it outright on tmux before 3.6). `--check` and `--uninstall`
/// resolve the same file, and the diff prompt names it.
#[test]
fn install_keys_marks_the_existing_xdg_config_and_creates_no_dotfile() {
    let s = Scratch::new();
    let home = s.workdir.join("home");
    let xdg = s.workdir.join("xdg");
    let xdg_conf = xdg.join("tmux/tmux.conf");
    std::fs::create_dir_all(xdg_conf.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let user_conf = "set -g mouse on\n";
    std::fs::write(&xdg_conf, user_conf).unwrap();

    let keys = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tma"))
            .arg("install-keys")
            .args(args)
            .arg("--config-dir")
            .arg(s.config_dir())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("TMA_CONFIG", common::empty_config_path())
            .output()
            .expect("spawn tma install-keys")
    };

    let out = keys(&["--yes"]);
    assert!(
        out.status.success(),
        "install-keys failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let marked = std::fs::read_to_string(&xdg_conf).unwrap();
    assert!(
        marked.contains("# tma keys") && marked.contains("set -g mouse on"),
        "the XDG config gained the line and kept the user's: {marked}"
    );
    assert!(
        !home.join(".tmux.conf").exists(),
        "no shadowing dotfile was created"
    );
    // The prompt names the file being marked, so a user sees which config before confirming.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&xdg_conf.display().to_string()),
        "the resolved config is named in the output: {stdout}"
    );

    assert!(
        keys(&["--check"]).status.success(),
        "--check resolves alike"
    );

    assert!(keys(&["--uninstall", "--yes"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&xdg_conf).unwrap(),
        user_conf,
        "uninstall found the line where install put it"
    );

    // The upgrade path: a `~/.tmux.conf` from the old unconditional default outranks the XDG file,
    // so an install (and later a `--check`/`--uninstall`) keeps using it.
    let dotfile = home.join(".tmux.conf");
    std::fs::write(&dotfile, "bind-key x kill-pane\n").unwrap();
    assert!(keys(&["--yes"]).status.success());
    assert!(std::fs::read_to_string(&dotfile)
        .unwrap()
        .contains("# tma keys"));
    assert_eq!(
        std::fs::read_to_string(&xdg_conf).unwrap(),
        user_conf,
        "the XDG config is left alone once a dotfile exists"
    );
}

/// With no tmux config anywhere, install-keys creates the XDG one (never a `~/.tmux.conf` that
/// could shadow a config added later), and says so.
#[test]
fn install_keys_creates_the_xdg_config_when_none_exists() {
    let s = Scratch::new();
    let home = s.workdir.join("home");
    let xdg = s.workdir.join("xdg");
    std::fs::create_dir_all(&home).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["install-keys", "--yes"])
        .arg("--config-dir")
        .arg(s.config_dir())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma install-keys");
    assert!(
        out.status.success(),
        "install-keys failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let created = xdg.join("tmux/tmux.conf");
    assert!(
        std::fs::read_to_string(&created)
            .unwrap()
            .contains("# tma keys"),
        "the created config carries the marked line"
    );
    assert!(!home.join(".tmux.conf").exists(), "no dotfile created");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no tmux config found"),
        "the creation is announced"
    );
}
