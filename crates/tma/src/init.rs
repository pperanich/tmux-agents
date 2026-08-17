//! `tma init`: the first-run wizard. It runs the setup a new user otherwise assembles by hand from
//! three files and five commands — detect which supported agents are actually installed, wire each
//! one's hooks ([`crate::install`]), install the keybindings ([`crate::install_keys`]), optionally
//! start the daemon — and ends with the [`crate::doctor`] report so the resulting posture is
//! visible rather than assumed.
//!
//! It chains those commands rather than reimplementing them, so every write keeps its own
//! diff-before-write confirmation and stays idempotent. The one step it does NOT perform is the
//! status line: `status-right` is the user's own format string, living in whichever config set it,
//! so init prints the line to add and where, exactly as `install-keys` has always done.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::config::Config;
use crate::install::{self, resolve_tmux_conf};
use crate::install_keys;
use crate::manifests::LoadedManifest;
use crate::tmux::Tmux;
use crate::{cli_support, doctor};

/// Options for `tma init` (parsed from the CLI in `main`).
pub(crate) struct InitOpts {
    /// Skip every interactive diff confirmation (scripted setup, tests).
    pub assume_yes: bool,
    /// Also bring up the event-hub daemon for THIS server now (`tma daemon --ensure`); the
    /// launcher for future servers rides the keybindings install either way.
    pub daemon: bool,
    /// Wire no daemon at all: no server-start launcher in the managed file, and none started here.
    pub no_daemon: bool,
    /// Override the tma config dir holding the managed `tmux.conf` and the per-server
    /// `hooks-state-<server>.toml` (env `TMA_CONFIG_DIR`), forwarded to both install steps.
    pub config_dir: Option<PathBuf>,
    /// The tmux config to mark with the keybindings `source-file` line, and the file the
    /// status-line instructions name. Defaults to `install-keys`' own resolution.
    pub conf: Option<PathBuf>,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    /// The loaded config: `[[agent]]` overrides and `[focus] events` for the install steps,
    /// `[telemetry]`/`[api]` for the closing doctor report, and the whole of it for the daemon.
    pub config: Config,
    /// Where that config came from, forwarded to a daemon this wizard spawns so it re-reads the
    /// same file.
    pub config_path: Option<PathBuf>,
    /// The idempotent daemon launcher, injected by `main`: tier 3 is reachable only from the
    /// `tma daemon` dispatch site (tests/tier_boundary.rs), so `--daemon` calls this instead of
    /// naming the daemon crate here.
    pub ensure_daemon: EnsureDaemon,
}

/// The `tma daemon --ensure` launcher `main` hands to [`InitOpts::ensure_daemon`]: config, target
/// server, manifest dir, config path.
pub(crate) type EnsureDaemon =
    fn(&Config, tma_tmux::tmux::Server, Option<PathBuf>, Option<PathBuf>) -> ExitCode;

/// A binary name that identifies a runtime rather than an agent. Several unrelated programs run
/// under each of these, so finding one on `$PATH` is no evidence that the agent is installed.
const GENERIC_COMMANDS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "ruby", "perl", "sh", "bash", "zsh", "fish",
    "agent",
];

/// One supported agent's detection verdict.
struct Detected {
    agent: String,
    /// The binary that matched on `$PATH`, `None` when none did.
    binary: Option<PathBuf>,
    /// Every candidate name was generic, so there is nothing specific to look for: the agent is
    /// reported as a manual wiring instead of as absent.
    undetectable: bool,
}

/// The binaries whose presence on `$PATH` means this agent is installed, derived from its manifest:
/// the manifest name (the token `install-hooks` takes, and the launcher name for every bundled
/// agent) followed by its `process_names`, minus the generic ones. A name that is only ever a
/// `comm` spelling (`codex-aarch64-a`, `opencode.exe`) stays in the list and simply never matches.
fn candidate_binaries(name: &str, process_names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for candidate in std::iter::once(name).chain(process_names.iter().map(String::as_str)) {
        if GENERIC_COMMANDS.contains(&candidate) || out.iter().any(|c| c == candidate) {
            continue;
        }
        out.push(candidate.to_string());
    }
    out
}

/// The first candidate that is an executable file in one of `dirs`, in candidate order. Split from
/// the `$PATH` read so it is testable without touching the environment.
fn find_in_dirs(candidates: &[String], dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for candidate in candidates {
        for dir in dirs {
            let path = dir.join(candidate);
            let executable = std::fs::metadata(&path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if executable {
                return Some(path);
            }
        }
    }
    None
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Detect every agent `install-hooks` could wire, in manifest order.
fn detect(manifests: &[LoadedManifest]) -> Vec<Detected> {
    let dirs = path_dirs();
    manifests
        .iter()
        .filter(|lm| install::is_installable(lm))
        .map(|lm| {
            let candidates = candidate_binaries(&lm.name, &lm.manifest.identity.process_names);
            Detected {
                agent: lm.name.clone(),
                binary: find_in_dirs(&candidates, &dirs),
                undetectable: candidates.is_empty(),
            }
        })
        .collect()
}

fn report_detection(detected: &[Detected]) {
    println!("tma: supported agents on your PATH:");
    for d in detected {
        match (&d.binary, d.undetectable) {
            (Some(path), _) => println!("  {:<9} found ({})", d.agent, path.display()),
            (None, true) => println!(
                "  {:<9} runs under a generic command name, so tma cannot detect it; \
                 run `tma install-hooks {}` if you use it",
                d.agent, d.agent
            ),
            (None, false) => println!(
                "  {:<9} not found (skipped; run `tma install-hooks {}` if you have it)",
                d.agent, d.agent
            ),
        }
    }
}

/// The status-line step, which is a report and not an edit. `status-right` is one option holding
/// the user's own format string, set from whichever config (or none) they use: composing
/// `#(tma status)` into it means rewriting a value tma does not parse, in a file it cannot know
/// wrote it. So init names the line, the file, and the reload — the same hands-off rule
/// `install-keys` and the uninstall sweep already follow.
fn report_status_line(tmux: &Tmux, conf: &Path) {
    let current = match tmux.get_global_option("status-right") {
        Ok(v) => v.unwrap_or_default(),
        Err(err) => {
            eprintln!("tma: cannot read `status-right` ({err}); add `#(tma status)` to it by hand");
            return;
        }
    };
    if current.contains("tma status") {
        println!("tma: status-right already runs `tma status` ({current})");
        return;
    }
    println!(
        "tma: status-right does NOT run `tma status`, so nothing keeps pane state fresh without \
         the daemon."
    );
    if current.is_empty() {
        println!("     Add the driver to {}:", conf.display());
        println!("       set -g status-right '#(tma status) %H:%M'");
    } else {
        println!(
            "     Your status-right is currently: {current}\n     \
             Keep it and add the driver to it in {}, e.g.:",
            conf.display()
        );
        println!("       set -g status-right '#(tma status) {current}'");
    }
    println!(
        "     Then reload: tmux source-file {}\n     \
         (tma never edits status-right: it is your format string, and merging into it would mean \
         rewriting a value tma does not parse.)",
        conf.display()
    );
}

/// The `install-hooks` options for one agent: the wizard's own flags plus config, with every
/// per-agent config path left to its env/default ladder (`TMA_*`), exactly as a bare
/// `tma install-hooks <agent>` resolves them.
fn install_opts(opts: &InitOpts, agent: &str) -> install::InstallOpts {
    install::InstallOpts {
        // The wizard never touches a statusline: the context shim composes into a command the user
        // owns, so it stays an explicit `install-hooks <agent> --statusline`.
        statusline: install::Statusline::Keep,
        agent: Some(agent.to_string()),
        uninstall: false,
        check: false,
        assume_yes: opts.assume_yes,
        server: opts.server.clone(),
        manifest_dir: opts.manifest_dir.clone(),
        settings: None,
        gemini_settings: None,
        config_dir: opts.config_dir.clone(),
        wrapper_path: None,
        wrapper_ref: opts.config.install.wrapper_ref,
        opencode_plugin: None,
        codex_config: None,
        codex_hooks: None,
        cursor_hooks: None,
        cursor_cli_config: None,
        pi_extension: None,
        focus_events: opts.config.focus.events,
        agents: opts.config.agent_overrides.clone(),
    }
}

/// Wait for a just-launched daemon to answer on its socket, reporting whether it did. `--ensure`
/// returns as soon as the detached child is spawned, so without this the closing doctor report
/// beats the daemon to its socket and says "not running" about the one init just started.
fn await_daemon(tmux: &Tmux) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if tma_runtime::ipc::daemon_status(tmux).is_some_and(|d| d.alive) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Whether a chained step succeeded. The steps hand back an `ExitCode`, which carries no
/// comparison of its own.
fn ok(code: ExitCode) -> bool {
    matches!(code, ExitCode::SUCCESS)
}

pub(crate) fn run(opts: InitOpts) -> ExitCode {
    let manifests = match cli_support::load_manifests_or_exit(
        opts.manifest_dir.as_deref(),
        &opts.config.agent_overrides,
    ) {
        Ok(m) => m,
        Err(code) => return code,
    };
    // Every write below confirms on stdin, and a confirmation read from a closed stdin declines.
    // Saying so up front beats letting the first step abort with no explanation.
    if !opts.assume_yes && !std::io::stdin().is_terminal() {
        println!(
            "tma: stdin is not a terminal, so every confirmation will decline; re-run with \
             --yes to apply."
        );
    }

    let tmux = Tmux::connect(&opts.server);
    let conf = resolve_tmux_conf(opts.conf.as_deref());
    let mut failed = false;

    let detected = detect(&manifests);
    report_detection(&detected);

    for d in detected.iter().filter(|d| d.binary.is_some()) {
        println!("\ntma: wiring {} ...", d.agent);
        if !ok(install::run(install_opts(&opts, &d.agent))) {
            failed = true;
        }
    }
    if detected.iter().all(|d| d.binary.is_none()) {
        println!(
            "\ntma: no supported agent found on PATH, so no hooks were wired; the steps below \
             still apply."
        );
    }

    println!();
    report_status_line(&tmux, &conf);

    println!();
    if install_keys::keys_current(
        opts.config_dir.as_deref(),
        opts.conf.as_deref(),
        !opts.no_daemon,
    ) {
        println!("tma: keybindings already installed and current; skipping");
    } else if !ok(install_keys::run(install_keys::InstallKeysOpts {
        uninstall: false,
        check: false,
        mouse: false,
        // The launcher rides the default install, so every future server gets a daemon; `--daemon`
        // additionally starts one for the server running this wizard.
        daemon: !opts.no_daemon,
        assume_yes: opts.assume_yes,
        conf: opts.conf.clone(),
        config_dir: opts.config_dir.clone(),
    })) {
        failed = true;
    }

    if opts.daemon {
        println!();
        if ok((opts.ensure_daemon)(
            &opts.config,
            opts.server.clone(),
            opts.manifest_dir.clone(),
            opts.config_path.clone(),
        )) {
            // The launch is fire-and-forget, so a daemon that never answers is worth naming — but
            // not a failed setup: everything above works without it (the daemon is additive).
            if !await_daemon(&tmux) {
                eprintln!(
                    "tma: the daemon was launched but has not answered on its socket yet; the \
                     report below may still say it is not running"
                );
            }
        } else {
            failed = true;
        }
    }

    // The closing report, informational: it describes the posture init just produced, and a
    // warning in it (no daemon, nothing polling yet) is not a failed setup.
    println!("\ntma: doctor:\n");
    let _ = doctor::run(doctor::DoctorOpts {
        json: false,
        exit_code: false,
        server: opts.server.clone(),
        manifest_dir: opts.manifest_dir.clone(),
        focus_events: opts.config.focus.events,
        windows: opts.config.telemetry.windows.clone(),
        api: opts.config.api.clone(),
        agents: opts.config.agent_overrides.clone(),
        wrapper_ref: opts.config.install.wrapper_ref,
    });

    if failed {
        eprintln!("tma: init did not finish: a step above failed or was declined");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The candidates come out of the manifests, so a bundled agent whose launcher is renamed is
    /// caught here rather than by a stale hand-written list. The two interesting shapes: an agent
    /// running under a generic interpreter (gemini, cursor, pi all report `node`), which must
    /// leave only the manifest name behind, and an agent whose `comm` spelling is not a command
    /// (codex's 15-char truncation, opencode's resolved `.exe`), which rides along harmlessly.
    #[test]
    fn bundled_manifests_derive_their_launcher_binaries() {
        let manifest = |file: &str, src: &str| {
            let m = tma_core::Manifest::parse(src, file).unwrap();
            candidate_binaries(file.trim_end_matches(".toml"), &m.identity.process_names)
        };
        assert_eq!(
            manifest(
                "claude.toml",
                include_str!("../../tma-core/manifests/claude.toml")
            ),
            names(&["claude"])
        );
        assert_eq!(
            manifest(
                "codex.toml",
                include_str!("../../tma-core/manifests/codex.toml")
            ),
            names(&["codex", "codex-aarch64-a"])
        );
        assert_eq!(
            manifest(
                "opencode.toml",
                include_str!("../../tma-core/manifests/opencode.toml")
            ),
            names(&["opencode", "opencode.exe"]),
            "the manifest name leads; the resolved `.exe` comm follows"
        );
        for (file, src) in [
            (
                "gemini.toml",
                include_str!("../../tma-core/manifests/gemini.toml"),
            ),
            (
                "cursor.toml",
                include_str!("../../tma-core/manifests/cursor.toml"),
            ),
            ("pi.toml", include_str!("../../tma-core/manifests/pi.toml")),
        ] {
            let stem = file.trim_end_matches(".toml");
            assert_eq!(
                manifest(file, src),
                names(&[stem]),
                "{stem} runs as `node`, which identifies no agent"
            );
        }
    }

    /// A manifest whose every name is generic leaves nothing to look for. That is the
    /// "skip detection, say so" case, not a silent absence.
    #[test]
    fn an_all_generic_manifest_has_no_candidates() {
        assert!(candidate_binaries("node", &names(&["node", "bash"])).is_empty());
        // A specific process name rescues a generic manifest NAME (a user manifest can be called
        // anything), so the two sources are read independently.
        assert_eq!(
            candidate_binaries("node", &names(&["node", "my-agent"])),
            names(&["my-agent"])
        );
    }

    /// Only an executable file counts, and the candidate order decides the winner, not the
    /// `$PATH` order: a directory or a non-executable file of the right name is not the agent.
    #[test]
    fn only_an_executable_file_on_the_path_counts() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "tma_init_path_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let dirs = vec![first.clone(), second.clone()];

        assert_eq!(find_in_dirs(&names(&["claude"]), &dirs), None);

        // A non-executable file of the right name is not a binary.
        std::fs::write(first.join("claude"), "").unwrap();
        assert_eq!(find_in_dirs(&names(&["claude"]), &dirs), None);
        std::fs::set_permissions(first.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            find_in_dirs(&names(&["claude"]), &dirs),
            Some(first.join("claude"))
        );

        // A directory named like the binary is not one either.
        std::fs::create_dir_all(first.join("codex")).unwrap();
        std::fs::write(second.join("codex"), "").unwrap();
        std::fs::set_permissions(second.join("codex"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            find_in_dirs(&names(&["codex"]), &dirs),
            Some(second.join("codex")),
            "the search walks on past a directory of the same name"
        );

        // Candidate order wins over directory order.
        assert_eq!(
            find_in_dirs(&names(&["codex", "claude"]), &dirs),
            Some(second.join("codex"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
