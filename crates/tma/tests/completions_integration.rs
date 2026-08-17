//! `tma completions <shell>` end to end. What the tree it generates from must and must not contain
//! is pinned by the unit tests in `src/completions.rs`; this covers the subcommand itself — that it
//! runs with no tmux server, no config, and no manifests, and writes a script to stdout.

use std::process::Command;

fn completions(shell: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tma"))
        .args(["completions", shell])
        // A completion script is generated from the argument tree alone. Point the config and the
        // manifests at paths that do not exist, so a developer's own `~/.config/tma` cannot make
        // this pass or fail.
        .args(["--config", "/nonexistent/tma.toml"])
        .args(["--manifest-dir", "/nonexistent/manifests"])
        .env("TMA_SOCKET_PATH", "/nonexistent/tma-completions.sock")
        .output()
        .expect("tma runs")
}

#[test]
fn every_shell_generates_a_script_without_touching_tmux_or_config() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = completions(shell);
        assert!(
            out.status.success(),
            "{shell}: exit {:?}",
            out.status.code()
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("install-hooks"),
            "{shell}: the script names no tma subcommand"
        );
        assert!(
            out.stderr.is_empty(),
            "{shell}: wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The shell is a required positional out of a closed set, so both mistakes are usage errors
/// (exit 2) rather than an empty script someone sources without noticing.
#[test]
fn a_missing_or_unknown_shell_is_a_usage_error() {
    for args in [vec!["completions"], vec!["completions", "nushell"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_tma"))
            .args(&args)
            .output()
            .expect("tma runs");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?} still wrote a script");
    }
}
