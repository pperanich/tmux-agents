//! `tma install-keys`: write tma's tmux keybindings to a managed file and `source-file` it from the
//! user's tmux config via a single marked line. Mirrors `install-hooks`'s ethos (idempotent,
//! diff-before-write, `--check`, symmetric `--uninstall`, `--yes`), reusing its file plumbing.
//!
//! Which config gets the line is resolved, not assumed (`resolve_tmux_conf`): the first of tmux's
//! own config files that exists, so `~/.config/tmux/tmux.conf` users are not handed a fresh
//! `~/.tmux.conf`.
//!
//! The bindings live ONLY in the managed file (`~/.config/tma/tmux.conf`), so uninstall removes that
//! file plus the one marked `source-file` line and never touches the user's other bindings. There is
//! no snapshot/restore of the user's config: tma writes nothing there but the one grep-able line.
//! That line names the managed file through tmux's own `$XDG_CONFIG_HOME`/`$HOME` expansion, so a
//! tmux config committed to a dotfiles repo keeps working on the next machine (see [`source_line`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::install::{apply_file, confirm, print_diff, resolve_config_dir, resolve_tmux_conf};

/// The grep-able marker on the `source-file` line tma adds to the user's tmux config. Every
/// managed line carries it, so idempotency and uninstall can find (and never duplicate) tma's line.
const KEYS_MARKER: &str = "# tma keys";

/// The banner opening the managed `tmux.conf`. Uninstall removes the managed file only when it
/// carries this, so a hand-written file at the same path is never clobbered.
const MANAGED_MARKER: &str = "tma keybindings";

/// One default binding: the prefix key, and the exact command string emitted after it. The command
/// strings are the fiddly part (the `--client "#{client_name}"` quoting), asserted verbatim in tests.
struct Binding {
    key: &'static str,
    command: &'static str,
}

/// The default bindings tma writes. Popup geometry is hardcoded 80% x 60% (tma-runtime's `[picker]`
/// config exposes glyphs/colors only, no geometry). `run-shell` format-expands its shell-command, so
/// `--client "#{client_name}"` resolves to the client that pressed the key; `display-popup` and
/// `split-window` do NOT (verified on 3.6a: the popup receives the literal `#{client_name}`), so the
/// picker and `watch` bindings pass no `--client` and let tma resolve the acting client itself —
/// from inside a popup or a pane, the targetless resolution finds the invoking client.
const BINDINGS: &[Binding] = &[
    Binding {
        key: "a",
        command: "display-popup -E -w 80% -h 60% 'tma'",
    },
    Binding {
        // `G` (uppercase): the persistent watcher, in a window of its own (`g` is taken by
        // `jump --blocked`; `G` is free in the stock prefix table). A new window rather than a split,
        // so the table gets the full terminal width; `new-window` does not format-expand, so no
        // `--client`.
        key: "G",
        command: "new-window 'tma watch --table'",
    },
    Binding {
        key: "j",
        command: "run-shell 'tma jump --attention --client \"#{client_name}\"'",
    },
    Binding {
        key: "g",
        command: "run-shell 'tma jump --blocked --client \"#{client_name}\"'",
    },
    Binding {
        key: "b",
        command: "run-shell 'tma jump --back --client \"#{client_name}\"'",
    },
    Binding {
        key: "h",
        command: "run-shell 'tma jump --home --client \"#{client_name}\"'",
    },
    Binding {
        // `A` (uppercase): open the action menu for the active pane. `run-shell` format-expands
        // `#{pane_id}`, so the menu targets the pane the key was pressed on.
        key: "A",
        command: "run-shell 'tma act --menu --pane \"#{pane_id}\"'",
    },
];

/// The opt-in mouse group (`--mouse`), bound in tmux's ROOT table (`-n`) so a click needs no prefix.
/// Both the `Status` and `StatusRight` key of each button are bound: a `#[range=user|…]` in
/// status-right resolves to the `Status` key, and the `StatusRight` one covers the rest of that area.
/// The dispatch is `if-shell -F`, a FORMAT conditional (no shell runs); `#{mouse_status_range}` holds
/// the clicked range's name, which `tma status` writes as `tma:<class>`.
///
/// Fall-through: tmux has no "now do what you would have done" command, so a bound key owns the
/// click. The left chain therefore ends with `switch-client -t=`, tmux's own stock
/// `MouseDown1Status`, which keeps clicking a window name working. The right chain ends with no
/// command at all (tmux's stock `MouseDown3Status` menu is a ~500-character version-specific
/// command this file will not fork), so a right-click outside a tma segment does nothing here;
/// tmux's own `M-MouseDown3Status` (alt + right-click) is untouched and still opens that menu.
const MOUSE_BINDINGS: &[Binding] = &[
    Binding {
        key: "MouseDown1Status",
        command: "if-shell -F '#{==:#{mouse_status_range},tma:blocked}' \
                  { run-shell 'tma jump --blocked --client \"#{client_name}\"' } \
                  { if-shell -F '#{m:tma:*,#{mouse_status_range}}' \
                  { display-popup -E -w 80% -h 60% 'tma' } { switch-client -t= } }",
    },
    Binding {
        key: "MouseDown1StatusRight",
        command: "if-shell -F '#{==:#{mouse_status_range},tma:blocked}' \
                  { run-shell 'tma jump --blocked --client \"#{client_name}\"' } \
                  { if-shell -F '#{m:tma:*,#{mouse_status_range}}' \
                  { display-popup -E -w 80% -h 60% 'tma' } }",
    },
    Binding {
        key: "MouseDown3Status",
        command: "if-shell -F '#{m:tma:*,#{mouse_status_range}}' \
                  { run-shell 'tma jump --menu --client \"#{client_name}\"' }",
    },
    Binding {
        key: "MouseDown3StatusRight",
        command: "if-shell -F '#{m:tma:*,#{mouse_status_range}}' \
                  { run-shell 'tma jump --menu --client \"#{client_name}\"' }",
    },
];

/// The prefix every [`MOUSE_BINDINGS`] line starts with, and the grep-able trace of an install that
/// took the group: `tma doctor` reads it to pair the bindings against the server's `mouse` option.
const MOUSE_LINE_PREFIX: &str = "bind-key -n Mouse";

/// The daemon launcher, written unless `--no-daemon`. The managed file is sourced from the user's
/// tmux config, so this fires once per server start. `#{socket_path}` (expanded by `run-shell` at
/// load time)
/// pins the server doing the sourcing, so a `tmux -L work` server daemonizes itself rather than the
/// ambient default one; `-b` keeps the spawn off the config-load path, and the redirect keeps a
/// missing binary from surfacing as a tmux message. `--ensure` is idempotent, so a re-source is a
/// no-op rather than a second daemon.
const DAEMON_LINE: &str =
    "run-shell -b 'tma --socket-path \"#{socket_path}\" daemon --ensure >/dev/null 2>&1'";

/// Whether the managed keybindings file carries the mouse group. File-based, like every other
/// `install-keys` predicate (the bindings live only in that file), and read-only, for `tma doctor`.
pub(crate) fn mouse_bindings_installed(config_dir: Option<&Path>) -> bool {
    let managed = resolve_config_dir(config_dir).join("tmux.conf");
    std::fs::read_to_string(managed).is_ok_and(|text| text.contains(MOUSE_LINE_PREFIX))
}

/// Where the managed `tmux.conf` came from, which decides how the `source-file` line names it: a
/// dir the user pinned (`--config-dir`, `TMA_CONFIG_DIR`) is spelled out literally, the default one
/// through tmux's own variable expansion.
#[derive(Clone, Copy)]
enum ConfigDir {
    Default,
    Pinned,
}

/// Options for `tma install-keys` (parsed from the CLI in `main`).
pub(crate) struct InstallKeysOpts {
    pub uninstall: bool,
    pub check: bool,
    /// Also write the [`MOUSE_BINDINGS`] group (opt-in: it claims tmux's status-line mouse keys and
    /// needs `set -g mouse on`, which tma never sets for you). With `--check`, require them.
    pub mouse: bool,
    /// Write the [`DAEMON_LINE`]. On by default (`--no-daemon` clears it), and with `--check` a
    /// file missing the line is drift for the same reason.
    pub daemon: bool,
    /// Skip the interactive diff confirmation (tests, scripted installs).
    pub assume_yes: bool,
    /// The tmux config to mark with the `source-file` line. Defaults to the first of tmux's own
    /// config files that exists (see [`resolve_tmux_conf`]).
    pub conf: Option<PathBuf>,
    /// Override the tma config dir holding the managed `tmux.conf` (default `~/.config/tma`;
    /// env `TMA_CONFIG_DIR`). Keeps tests off the real config.
    pub config_dir: Option<PathBuf>,
}

/// Whether the managed dir was pinned (flag or env) or is the default one. An empty
/// `TMA_CONFIG_DIR` counts as unset here, as it does in [`resolve_config_dir`].
fn config_dir_kind(config_dir: Option<&Path>) -> ConfigDir {
    if config_dir.is_some() || std::env::var_os("TMA_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        ConfigDir::Pinned
    } else {
        ConfigDir::Default
    }
}

pub(crate) fn run(opts: InstallKeysOpts) -> ExitCode {
    let managed = resolve_config_dir(opts.config_dir.as_deref()).join("tmux.conf");
    let dir = config_dir_kind(opts.config_dir.as_deref());
    // Same resolution for install, --check, and --uninstall, so the latter two look where the
    // install put the line.
    let conf = resolve_tmux_conf(opts.conf.as_deref());

    if opts.check {
        return check(&managed, &conf, dir, opts.mouse, opts.daemon);
    }
    if opts.uninstall {
        uninstall(&managed, &conf, opts.assume_yes)
    } else {
        install(
            &managed,
            &conf,
            dir,
            opts.assume_yes,
            opts.mouse,
            opts.daemon,
        )
    }
}

/// The managed `tmux.conf` content: a banner then one `bind-key` line per [`BINDINGS`] entry, plus
/// the root-table [`MOUSE_BINDINGS`] when `mouse` and the [`DAEMON_LINE`] when `daemon`. Both
/// opt-in groups are appended, never woven in. Deterministic, so a re-install with the same paths
/// is byte-identical (idempotent).
fn render_managed(mouse: bool, daemon: bool) -> String {
    let mut out = format!(
        "# {MANAGED_MARKER}, managed by `tma install-keys`. Do not hand-edit; re-run to update,\n\
         # or `tma install-keys --uninstall` to remove.\n"
    );
    for b in BINDINGS {
        out.push_str(&format!("bind-key {} {}\n", b.key, b.command));
    }
    if mouse {
        out.push_str(
            "# Clickable status segments (--mouse). Needs `set -g mouse on`, which tma leaves to you.\n",
        );
        for b in MOUSE_BINDINGS {
            out.push_str(&format!("bind-key -n {} {}\n", b.key, b.command));
        }
    }
    if daemon {
        out.push_str(
            "# Event-hub daemon, started once per tmux server start (omit with --no-daemon). Idempotent.\n",
        );
        out.push_str(DAEMON_LINE);
        out.push('\n');
    }
    out
}

/// The single marked line tma adds to the user's tmux config. Paths are double-quoted so one
/// containing a space still parses (tmux strips the quotes) and, in the default form, so tmux
/// expands the `$VAR`s at load time rather than baking in this machine's absolute path.
///
/// The default form hands tmux both candidates in the order [`resolve_config_dir`] uses; `-q` makes
/// the first a quiet miss when `XDG_CONFIG_HOME` is unset (it expands to `/tma/tmux.conf`) so the
/// `$HOME` fallback loads. A pinned dir keeps the literal path: the variables would resolve
/// elsewhere at load time, and without `-q` a pinned path gone stale still errors loudly.
fn source_line(managed: &Path, dir: ConfigDir) -> String {
    match dir {
        ConfigDir::Default => format!(
            "source-file -q \"$XDG_CONFIG_HOME/tma/tmux.conf\" \"$HOME/.config/tma/tmux.conf\" {KEYS_MARKER}"
        ),
        ConfigDir::Pinned => format!("source-file \"{}\" {KEYS_MARKER}", managed.display()),
    }
}

/// True when `line` is one of tma's marked `source-file` lines. A line qualifies only when, after
/// trimming, it starts with `source-file ` AND ends with the exact `# tma keys` marker, so a
/// user's own line that merely contains the marker text mid-line (e.g.
/// `bind-key k next-window # tma keys handy`) is never mistaken for ours and destroyed. Every form
/// tma has emitted matches (the `-q` variable one, the quoted absolute one, the older unquoted one),
/// so uninstall and the drift self-heal still find a line an earlier build wrote.
fn is_keys_source_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("source-file ") && trimmed.ends_with(KEYS_MARKER)
}

/// Ensure the user's tmux config holds exactly one marked `source-file` line (`want`): drop every existing
/// marked line, then append `want`. Returns the new text, or `None` when it already matches (no-op).
fn mark_conf(old: &str, want: &str) -> Option<String> {
    let kept: Vec<&str> = old.lines().filter(|l| !is_keys_source_line(l)).collect();
    // Already exactly one marked line equal to `want`, and no stray marked lines: nothing to do.
    let marked: Vec<&str> = old.lines().filter(|l| is_keys_source_line(l)).collect();
    if marked.len() == 1 && marked[0] == want && old.ends_with('\n') {
        return None;
    }
    let mut new = String::new();
    for line in &kept {
        new.push_str(line);
        new.push('\n');
    }
    new.push_str(want);
    new.push('\n');
    Some(new)
}

/// Remove tma's marked `source-file` line(s) from the user's tmux config, leaving every other line intact.
/// Returns the new text, or `None` when no marked line is present (no-op).
fn unmark_conf(old: &str) -> Option<String> {
    if !old.lines().any(is_keys_source_line) {
        return None;
    }
    let kept: Vec<&str> = old.lines().filter(|l| !is_keys_source_line(l)).collect();
    let mut new = String::new();
    for line in &kept {
        new.push_str(line);
        new.push('\n');
    }
    Some(new)
}

fn install(
    managed: &Path,
    conf: &Path,
    dir: ConfigDir,
    assume_yes: bool,
    mouse: bool,
    daemon: bool,
) -> ExitCode {
    // 1. Write the managed bindings file (diff + confirm, idempotent). Refuse to overwrite a
    //    non-empty file at the managed path that we did not write (no banner), even with --yes:
    //    it holds someone's content. This mirrors uninstall, which likewise spares a foreign file.
    let old = std::fs::read_to_string(managed).unwrap_or_default();
    if !old.is_empty() && !old.contains(MANAGED_MARKER) {
        eprintln!(
            "tma: {} exists but is not a tma keybindings file (no `{MANAGED_MARKER}` banner); \
             move or remove it, then re-run",
            managed.display()
        );
        return ExitCode::FAILURE;
    }
    let new = render_managed(mouse, daemon);
    if !apply_file(managed, &old, &new, assume_yes, "tma keybindings") {
        return ExitCode::FAILURE;
    }

    // 2. Ensure the resolved tmux config sources it via exactly one marked line.
    if !conf.exists() {
        println!(
            "tma: no tmux config found; creating {} (tmux loads it)",
            conf.display()
        );
    }
    let conf_old = std::fs::read_to_string(conf).unwrap_or_default();
    let want = source_line(managed, dir);
    match mark_conf(&conf_old, &want) {
        None => println!("tma: {} already sources the keybindings", conf.display()),
        Some(conf_new) => {
            if !apply_file(
                conf,
                &conf_old,
                &conf_new,
                assume_yes,
                "tma keys source-file",
            ) {
                return ExitCode::FAILURE;
            }
        }
    }

    println!(
        "tma: installed keybindings ({}). Reload with `tmux source-file {}`.",
        managed.display(),
        conf.display()
    );
    if mouse {
        println!(
            "tma: the clickable status segments also need `set -g mouse on` (tma does not set it: \
             it changes selection and copy/paste for every pane)."
        );
    }
    if daemon {
        println!(
            "tma: the daemon starts with the next tmux server; run `tma daemon --ensure` to start \
             it for this one now."
        );
    }
    println!(
        "tma: reminder: add `#(tma status)` to your `status-right` for the ambient state driver \
         (tma does not edit status-right)."
    );
    ExitCode::SUCCESS
}

fn uninstall(managed: &Path, conf: &Path, assume_yes: bool) -> ExitCode {
    // 1. Remove the managed bindings file, but only when it is one tma wrote (carries the banner).
    match std::fs::read_to_string(managed) {
        Err(_) => println!(
            "tma: keybindings file already absent ({})",
            managed.display()
        ),
        Ok(text) if !text.contains(MANAGED_MARKER) => {
            eprintln!(
                "tma: {} is not a tma keybindings file (no marker); leaving it untouched",
                managed.display()
            );
        }
        Ok(text) => {
            println!("tma: proposed change to {} (remove):", managed.display());
            print_diff(&text, "");
            if !assume_yes && !confirm() {
                println!("tma: aborted; no changes written");
                return ExitCode::FAILURE;
            }
            if let Err(err) = std::fs::remove_file(managed) {
                eprintln!("tma: cannot remove {}: {err}", managed.display());
                return ExitCode::FAILURE;
            }
        }
    }

    // 2. Remove the marked `source-file` line from the resolved tmux config, touching nothing else.
    let conf_old = std::fs::read_to_string(conf).unwrap_or_default();
    match unmark_conf(&conf_old) {
        None => println!("tma: {} has no keys source-file line", conf.display()),
        // A declined confirm or a failed write here would leave the config sourcing a file that is
        // now gone, so it fails the command rather than reporting an uninstall that did not happen.
        Some(conf_new) => {
            if !apply_file(
                conf,
                &conf_old,
                &conf_new,
                assume_yes,
                "tma keys source-file",
            ) {
                eprintln!(
                    "tma: {} still sources the (now removed) keybindings file; \
                     remove the `source-file` line by hand or re-run",
                    conf.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

    println!("tma: uninstalled keybindings");
    ExitCode::SUCCESS
}

/// Whether the keybindings are installed and current, read-only: the `--check` verdict without its
/// output. `tma init` asks before offering to install, so an already-wired setup is a clean skip
/// rather than a re-run of the whole diff flow. `require_daemon` carries `init --daemon` through, so
/// a file installed without the launcher is not read as current when this run wants one.
pub(crate) fn keys_current(
    config_dir: Option<&Path>,
    conf: Option<&Path>,
    require_daemon: bool,
) -> bool {
    let managed = resolve_config_dir(config_dir).join("tmux.conf");
    let dir = config_dir_kind(config_dir);
    drift(
        &managed,
        &resolve_tmux_conf(conf),
        dir,
        false,
        require_daemon,
    )
    .is_empty()
}

/// Verify the install. The two extra groups differ by default: the mouse group is opt-in, so its
/// absence is never drift and `--check --mouse` is how a script asks "are the clickable segments
/// wired?"; the daemon launcher is written by default, so its absence IS drift unless the check
/// waives it with `--no-daemon`.
fn check(
    managed: &Path,
    conf: &Path,
    dir: ConfigDir,
    require_mouse: bool,
    require_daemon: bool,
) -> ExitCode {
    let drift = drift(managed, conf, dir, require_mouse, require_daemon);
    if drift.is_empty() {
        println!("tma: keybindings OK");
        ExitCode::SUCCESS
    } else {
        eprintln!("tma: keybindings incomplete:");
        for d in &drift {
            eprintln!("  - {d}");
        }
        eprintln!("run `tma install-keys` to (re)install");
        ExitCode::FAILURE
    }
}

/// Everything about the current install that differs from what a fresh one would write: an empty
/// list is the whole `--check` verdict, so the printing above and [`keys_current`] cannot diverge.
fn drift(
    managed: &Path,
    conf: &Path,
    dir: ConfigDir,
    require_mouse: bool,
    require_daemon: bool,
) -> Vec<String> {
    let mut drift = Vec::new();

    match std::fs::read_to_string(managed) {
        // Which of the four renderings this file is, if any: matching none is staleness, matching
        // one without a required group is a missing group, and the two are different repairs.
        Ok(text) => {
            let found = [(false, false), (false, true), (true, false), (true, true)]
                .into_iter()
                .find(|&(mouse, daemon)| text == render_managed(mouse, daemon));
            match found {
                Some((mouse, daemon)) => {
                    if require_mouse && !mouse {
                        drift.push(format!(
                            "keybindings file {} has no mouse bindings; re-run \
                             `tma install-keys --mouse`",
                            managed.display()
                        ));
                    }
                    if require_daemon && !daemon {
                        drift.push(format!(
                            "keybindings file {} has no daemon launcher; re-run \
                             `tma install-keys` (or `--check --no-daemon` if that is deliberate)",
                            managed.display()
                        ));
                    }
                }
                None => drift.push(format!(
                    "keybindings file {} is stale (differs from what tma would write); reinstall",
                    managed.display()
                )),
            }
        }
        Err(_) => drift.push(format!(
            "keybindings file {} missing (never installed)",
            managed.display()
        )),
    }

    let conf_text = std::fs::read_to_string(conf).unwrap_or_default();
    let want = source_line(managed, dir);
    let marked: Vec<&str> = conf_text
        .lines()
        .filter(|l| is_keys_source_line(l))
        .collect();
    match marked.as_slice() {
        [line] if *line == want => {}
        [] => drift.push(format!(
            "{} has no `source-file` line for the keybindings",
            conf.display()
        )),
        [_] => drift.push(format!(
            "{} sources a different keybindings path; reinstall",
            conf.display()
        )),
        _ => drift.push(format!(
            "{} has more than one tma keys `source-file` line; reinstall to dedup",
            conf.display()
        )),
    }
    drift
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitted binding lines, verbatim. The `--client "#{client_name}"` quoting is the point:
    /// single quotes wrap the whole command so tmux passes it as one arg, and the inner double
    /// quotes group the format-expanded client name. It rides only on the `run-shell` bindings,
    /// the ones tmux expands. A change here is a change users must re-run for.
    #[test]
    fn managed_file_emits_exact_binding_lines() {
        let got = render_managed(false, false);
        let want = "\
# tma keybindings, managed by `tma install-keys`. Do not hand-edit; re-run to update,
# or `tma install-keys --uninstall` to remove.
bind-key a display-popup -E -w 80% -h 60% 'tma'
bind-key G new-window 'tma watch --table'
bind-key j run-shell 'tma jump --attention --client \"#{client_name}\"'
bind-key g run-shell 'tma jump --blocked --client \"#{client_name}\"'
bind-key b run-shell 'tma jump --back --client \"#{client_name}\"'
bind-key h run-shell 'tma jump --home --client \"#{client_name}\"'
bind-key A run-shell 'tma act --menu --pane \"#{pane_id}\"'
";
        assert_eq!(got, want);
        assert!(
            !got.contains("Mouse"),
            "the mouse group is opt-in and absent by default"
        );
    }

    /// The managed file may be sourced TWICE per config load: the default `source-file -q` line
    /// names both the XDG and `$HOME` candidates, and when `XDG_CONFIG_HOME` is `~/.config` they
    /// are the same file. `bind-key` and comments are idempotent under a re-source; anything
    /// accumulative (`set -ga`, a `run-shell` that stacks state) would silently double, so the file
    /// must hold nothing else. The one `run-shell` allowed is [`DAEMON_LINE`], whose `--ensure`
    /// takes the single-instance flock and exits 0 rather than starting a second daemon.
    #[test]
    fn managed_file_stays_idempotent_under_double_source() {
        for mouse in [false, true] {
            for daemon in [false, true] {
                for line in render_managed(mouse, daemon).lines() {
                    assert!(
                        line.starts_with("bind-key ")
                            || line.starts_with("bind-key -n ")
                            || line == DAEMON_LINE
                            || line.starts_with('#'),
                        "non-idempotent line in the managed file: {line}"
                    );
                }
            }
        }
    }

    /// The opt-in mouse group, verbatim. Four root-table bindings whose dispatch is a FORMAT
    /// conditional (`if-shell -F`, no shell); the `tma:<class>` names come from the range markers
    /// `tma status` emits, `run-shell` expands `#{client_name}` and `display-popup` does not (so the
    /// popup passes no `--client`), and the left chain ends with tmux's own stock
    /// `MouseDown1Status` so clicking a window name still switches to it.
    #[test]
    fn mouse_group_emits_exact_binding_lines() {
        let got = render_managed(true, false);
        let base = render_managed(false, false);
        assert!(
            got.starts_with(&base),
            "the mouse group is appended, not woven in"
        );
        let want = "\
# Clickable status segments (--mouse). Needs `set -g mouse on`, which tma leaves to you.
bind-key -n MouseDown1Status if-shell -F '#{==:#{mouse_status_range},tma:blocked}' { run-shell 'tma jump --blocked --client \"#{client_name}\"' } { if-shell -F '#{m:tma:*,#{mouse_status_range}}' { display-popup -E -w 80% -h 60% 'tma' } { switch-client -t= } }
bind-key -n MouseDown1StatusRight if-shell -F '#{==:#{mouse_status_range},tma:blocked}' { run-shell 'tma jump --blocked --client \"#{client_name}\"' } { if-shell -F '#{m:tma:*,#{mouse_status_range}}' { display-popup -E -w 80% -h 60% 'tma' } }
bind-key -n MouseDown3Status if-shell -F '#{m:tma:*,#{mouse_status_range}}' { run-shell 'tma jump --menu --client \"#{client_name}\"' }
bind-key -n MouseDown3StatusRight if-shell -F '#{m:tma:*,#{mouse_status_range}}' { run-shell 'tma jump --menu --client \"#{client_name}\"' }
";
        assert_eq!(&got[base.len()..], want);
        // The doctor probe greps for this prefix; it must be what the group actually renders.
        assert!(got.contains(MOUSE_LINE_PREFIX) && !base.contains(MOUSE_LINE_PREFIX));
    }

    /// The home-manager module writes the same prefix bindings declaratively, from a hand-copied
    /// block, and nothing but this test stops the two from drifting (it drifted once already: the
    /// module was missing `G` and `A` and still carried a `--client` on the popup binding). Both
    /// directions are checked, so a key added here without a module edit fails, and so does a key
    /// left in the module after being dropped here. The opt-in [`MOUSE_BINDINGS`] are deliberately
    /// out of scope: the module writes no mouse group, and `mouse on` is the user's call.
    #[test]
    fn hm_module_matches_the_prefix_bindings() {
        // CARGO_MANIFEST_DIR is crates/tma; the workspace root is two levels up.
        let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nix/hm-module.nix");
        let text = std::fs::read_to_string(&module)
            .unwrap_or_else(|e| panic!("read {}: {e}", module.display()));

        // Scope the scan to the module's tma block, so an unrelated `bind-key` a user example grows
        // later is not read as drift.
        let block = text
            .split_once("# tma keybindings (programs.tma.keybindings.enable)")
            .expect("the module's tma keybindings block")
            .1
            .split_once("'')")
            .expect("the block's closing delimiter")
            .0;
        let module_lines: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("bind-key "))
            .collect();

        for b in BINDINGS {
            let want = format!("bind-key {} {}", b.key, b.command);
            assert!(
                module_lines.contains(&want.as_str()),
                "{} is missing `{want}`; the module block must match BINDINGS verbatim",
                module.display()
            );
        }
        let ours: Vec<String> = BINDINGS
            .iter()
            .map(|b| format!("bind-key {} {}", b.key, b.command))
            .collect();
        for line in &module_lines {
            assert!(
                ours.iter().any(|w| w == line),
                "{} carries `{line}`, which no BINDINGS entry writes",
                module.display()
            );
        }
        assert_eq!(
            module_lines.len(),
            BINDINGS.len(),
            "the module block must hold each binding exactly once"
        );
        assert!(
            !block.contains("bind-key -n Mouse"),
            "the module deliberately writes no mouse group"
        );
        // The module's own daemon option writes the same launcher, so the two cannot drift either.
        assert!(
            !block.contains(DAEMON_LINE),
            "the daemon launcher belongs to its own option, not the keybindings block"
        );
        assert!(
            text.contains(DAEMON_LINE),
            "{} must carry `{DAEMON_LINE}` verbatim for programs.tma.daemon.autostart",
            module.display()
        );
    }

    /// A pinned config dir is named literally: the variables in the default form would resolve
    /// somewhere else when tmux loads the config.
    #[test]
    fn source_line_carries_the_marker() {
        let line = source_line(
            Path::new("/home/u/.config/tma/tmux.conf"),
            ConfigDir::Pinned,
        );
        assert_eq!(
            line,
            "source-file \"/home/u/.config/tma/tmux.conf\" # tma keys"
        );
        assert!(line.contains(KEYS_MARKER));
        assert!(is_keys_source_line(&line));
    }

    /// The default line holds no machine-specific path: tmux expands both `$VAR`s at load time and
    /// `-q` swallows the XDG miss, so the same tmux config works on a machine with a different home.
    #[test]
    fn default_source_line_names_no_absolute_path() {
        let line = source_line(
            Path::new("/home/u/.config/tma/tmux.conf"),
            ConfigDir::Default,
        );
        assert_eq!(
            line,
            "source-file -q \"$XDG_CONFIG_HOME/tma/tmux.conf\" \"$HOME/.config/tma/tmux.conf\" # tma keys"
        );
        assert!(!line.contains("/home/u"), "no install-time path baked in");
        assert!(is_keys_source_line(&line));
    }

    /// The upgrade path at the text level: a line an older build wrote with the absolute path is
    /// still recognized as ours, so a reinstall replaces it and an uninstall strips it.
    #[test]
    fn an_old_absolute_form_line_is_still_ours() {
        let old = "source-file \"/home/u/.config/tma/tmux.conf\" # tma keys\n";
        assert!(is_keys_source_line(old.trim_end()));

        let want = source_line(
            Path::new("/home/u/.config/tma/tmux.conf"),
            ConfigDir::Default,
        );
        let healed = mark_conf(old, &want).expect("the old form is not the wanted line");
        assert_eq!(healed, format!("{want}\n"), "replaced, not duplicated");
        assert_eq!(
            unmark_conf(old).expect("the old form is removable"),
            "",
            "uninstall still strips it"
        );
    }

    /// Marking an empty conf adds exactly one line; marking again is a no-op (never a duplicate).
    #[test]
    fn mark_conf_is_idempotent_single_line() {
        let want = source_line(Path::new("/x/tmux.conf"), ConfigDir::Pinned);
        let once = mark_conf("", &want).expect("empty conf gains the line");
        assert_eq!(once, format!("{want}\n"));
        assert_eq!(once.matches(KEYS_MARKER).count(), 1);
        // Re-mark the already-marked file: no change.
        assert!(mark_conf(&once, &want).is_none(), "re-mark is a no-op");
    }

    /// Marking preserves the user's other lines and repoints a stale/duplicated marked line to one.
    #[test]
    fn mark_conf_preserves_others_and_dedups() {
        let want = source_line(Path::new("/new/tmux.conf"), ConfigDir::Pinned);
        let old = "set -g mouse on\n\
                   source-file /old/tmux.conf # tma keys\n\
                   bind-key x kill-pane\n\
                   source-file /stray/tmux.conf # tma keys\n";
        let new = mark_conf(old, &want).expect("stale marked lines force a rewrite");
        assert!(new.contains("set -g mouse on"), "user line kept");
        assert!(new.contains("bind-key x kill-pane"), "user line kept");
        assert!(!new.contains("/old/tmux.conf"), "stale marked line dropped");
        assert!(
            !new.contains("/stray/tmux.conf"),
            "duplicate marked line dropped"
        );
        assert_eq!(
            new.matches(KEYS_MARKER).count(),
            1,
            "exactly one marked line"
        );
        assert!(
            new.trim_end().ends_with(&want),
            "the wanted line is present"
        );
    }

    /// Uninstall symmetry at the text level: unmark removes exactly the marked line(s) and leaves
    /// the user's other lines byte-for-byte; an unmarked conf is a clean no-op.
    #[test]
    fn unmark_conf_removes_only_marked_lines() {
        let old = "set -g mouse on\n\
                   source-file /x/tmux.conf # tma keys\n\
                   bind-key x kill-pane\n";
        let new = unmark_conf(old).expect("marked line present");
        assert_eq!(new, "set -g mouse on\nbind-key x kill-pane\n");
        assert!(
            unmark_conf(&new).is_none(),
            "no marked line left is a no-op"
        );
    }

    /// A user's own line that merely contains the marker text mid-line is NOT tma's line: it must
    /// survive both mark and unmark. Only a real `source-file ... # tma keys` line is ours.
    #[test]
    fn user_line_with_mid_line_marker_survives_mark_and_unmark() {
        let want = source_line(Path::new("/x/tmux.conf"), ConfigDir::Pinned);
        // A hand-written binding whose trailing comment happens to contain the marker substring.
        let user_line = "bind-key k next-window # tma keys handy";
        let old = format!("{user_line}\n");

        // Marking keeps the user's line verbatim and appends exactly our one line.
        let marked = mark_conf(&old, &want).expect("empty of our line, so we add ours");
        assert!(
            marked.contains(user_line),
            "user line preserved through mark"
        );
        assert!(marked.trim_end().ends_with(&want), "our line appended");

        // Unmarking removes only our line, leaving the user's line byte-for-byte.
        let unmarked = unmark_conf(&marked).expect("our line present to remove");
        assert_eq!(unmarked, old, "user line preserved through unmark");
    }

    /// A managed path containing a space renders as a quoted, round-trippable marked line.
    #[test]
    fn source_line_quotes_a_spaced_path() {
        let line = source_line(
            Path::new("/home/a b/.config/tma/tmux.conf"),
            ConfigDir::Pinned,
        );
        assert_eq!(
            line,
            "source-file \"/home/a b/.config/tma/tmux.conf\" # tma keys"
        );
        assert!(is_keys_source_line(&line));
    }

    /// Full install → uninstall on temp paths: the managed file is created then gone, the conf gains
    /// then loses exactly the marked line, and the user's own conf line survives untouched.
    #[test]
    fn install_then_uninstall_leaves_conf_as_found() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();
        let user_conf = "set -g mouse on\n";
        std::fs::write(&conf, user_conf).unwrap();

        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        assert!(managed.exists(), "managed file written");
        let after_install = std::fs::read_to_string(&conf).unwrap();
        assert!(after_install.contains("set -g mouse on"), "user line kept");
        assert_eq!(after_install.matches(KEYS_MARKER).count(), 1);

        // Re-install is a clean no-op on both files.
        let managed_bytes = std::fs::read_to_string(&managed).unwrap();
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        assert_eq!(managed_bytes, std::fs::read_to_string(&managed).unwrap());
        assert_eq!(after_install, std::fs::read_to_string(&conf).unwrap());

        assert!(matches!(
            uninstall(&managed, &conf, true),
            ExitCode::SUCCESS
        ));
        assert!(!managed.exists(), "managed file removed");
        assert_eq!(
            std::fs::read_to_string(&conf).unwrap(),
            user_conf,
            "conf restored to exactly the user's original line"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--check` verdicts: FAILURE before install (missing file, no source line), SUCCESS after,
    /// FAILURE again once the managed file drifts.
    #[test]
    fn check_reports_missing_then_ok_then_stale() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_check_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, false, false),
                ExitCode::FAILURE
            ),
            "nothing installed → drift"
        );
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, false, false),
                ExitCode::SUCCESS
            ),
            "post-install → OK"
        );
        // Corrupt the managed file: check must flag the drift.
        std::fs::write(&managed, "# tma keybindings\nbind-key a detach\n").unwrap();
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, false, false),
                ExitCode::FAILURE
            ),
            "a stale managed file → drift"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The upgrade path end to end: a conf still holding the absolute-path line reads as drift, and
    /// a plain reinstall rewrites it to the portable line without duplicating or losing anything.
    #[test]
    fn check_flags_an_old_absolute_line_and_reinstall_heals_it() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_upgrade_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &conf,
            format!(
                "set -g mouse on\nsource-file \"{}\" # tma keys\n",
                managed.display()
            ),
        )
        .unwrap();

        assert!(matches!(
            install(&managed, &conf, ConfigDir::Default, true, false, false),
            ExitCode::SUCCESS
        ));
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Default, false, false),
                ExitCode::SUCCESS
            ),
            "the reinstall healed the drift"
        );
        let healed = std::fs::read_to_string(&conf).unwrap();
        assert!(healed.contains("set -g mouse on"), "user line kept");
        assert_eq!(healed.matches(KEYS_MARKER).count(), 1);
        assert!(
            !healed.contains(&managed.display().to_string()),
            "the install-time path is gone: {healed}"
        );

        // Put the old form back: --check must call it drift rather than pass it.
        std::fs::write(
            &conf,
            format!("source-file \"{}\" # tma keys\n", managed.display()),
        )
        .unwrap();
        assert!(matches!(
            check(&managed, &conf, ConfigDir::Default, false, false),
            ExitCode::FAILURE
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mouse group is opt-in in both directions: an install without `--mouse` writes none of it
    /// and is NOT drift, `--check --mouse` is how you ask whether it is wired, and an install with
    /// `--mouse` satisfies a plain `--check` too (both renderings are current).
    #[test]
    fn mouse_group_is_opt_in_and_only_checked_when_asked() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_mouse_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        let plain = std::fs::read_to_string(&managed).unwrap();
        assert!(
            !plain.contains("MouseDown1Status"),
            "not installed by default"
        );
        assert!(matches!(
            check(&managed, &conf, ConfigDir::Pinned, false, false),
            ExitCode::SUCCESS
        ));
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, true, false),
                ExitCode::FAILURE
            ),
            "--check --mouse insists on the group"
        );

        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, true, false),
            ExitCode::SUCCESS
        ));
        let with_mouse = std::fs::read_to_string(&managed).unwrap();
        assert_eq!(with_mouse.matches("bind-key -n Mouse").count(), 4);
        assert!(matches!(
            check(&managed, &conf, ConfigDir::Pinned, true, false),
            ExitCode::SUCCESS
        ));
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, false, false),
                ExitCode::SUCCESS
            ),
            "a mouse install is current for a plain --check too"
        );
        // Re-installing with the group is a byte-identical no-op, like the plain install.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, true, false),
            ExitCode::SUCCESS
        ));
        assert_eq!(with_mouse, std::fs::read_to_string(&managed).unwrap());

        // Dropping back to the plain set rewrites the file, and uninstall still removes all of it.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        assert_eq!(plain, std::fs::read_to_string(&managed).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon launcher, verbatim. `#{socket_path}` is the whole point: `run-shell` expands it at
    /// load time to the socket of the server doing the sourcing, so a `tmux -L work` server starts a
    /// daemon for ITSELF rather than for the ambient default one a bare `tma daemon --ensure` would
    /// resolve. A change here is a change users must re-run `tma install-keys --daemon` for.
    #[test]
    fn daemon_line_is_emitted_verbatim_and_pins_the_sourcing_server() {
        let got = render_managed(false, true);
        let base = render_managed(false, false);
        assert!(
            got.starts_with(&base),
            "the daemon line is appended, not woven in"
        );
        let want = "\
# Event-hub daemon, started once per tmux server start (omit with --no-daemon). Idempotent.
run-shell -b 'tma --socket-path \"#{socket_path}\" daemon --ensure >/dev/null 2>&1'
";
        assert_eq!(&got[base.len()..], want);
    }

    /// The daemon line is opt-OUT, unlike the mouse group: a plain install writes it and a plain
    /// `--check` calls its absence drift, which is what makes `--no-daemon` a standing choice
    /// rather than a one-off. The two groups still compose in every combination.
    #[test]
    fn daemon_line_is_opt_in_and_composes_with_the_mouse_group() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_daemon_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();

        // The default install: launcher present, and a plain `--check` is satisfied.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, true),
            ExitCode::SUCCESS
        ));
        assert_eq!(
            std::fs::read_to_string(&managed)
                .unwrap()
                .matches(DAEMON_LINE)
                .count(),
            1,
            "the launcher is written by default"
        );
        assert!(matches!(
            check(&managed, &conf, ConfigDir::Pinned, false, true),
            ExitCode::SUCCESS
        ));

        // `--no-daemon` drops it, which a plain check calls drift and `--check --no-daemon` accepts.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        assert!(
            !std::fs::read_to_string(&managed)
                .unwrap()
                .contains("tma daemon --ensure"),
            "--no-daemon omits the launcher"
        );
        assert!(
            matches!(
                check(&managed, &conf, ConfigDir::Pinned, false, true),
                ExitCode::FAILURE
            ),
            "a missing launcher is drift by default"
        );
        assert!(matches!(
            check(&managed, &conf, ConfigDir::Pinned, false, false),
            ExitCode::SUCCESS
        ));

        // Both groups at once: current for a plain check and for either group's own check.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, true, true),
            ExitCode::SUCCESS
        ));
        let both = std::fs::read_to_string(&managed).unwrap();
        assert_eq!(both.matches("bind-key -n Mouse").count(), 4);
        assert_eq!(both.matches(DAEMON_LINE).count(), 1);
        for (mouse, daemon) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(
                matches!(
                    check(&managed, &conf, ConfigDir::Pinned, mouse, daemon),
                    ExitCode::SUCCESS
                ),
                "a both-groups install is current for --check mouse={mouse} daemon={daemon}"
            );
        }
        // Re-installing is a byte-identical no-op, so a re-run never doubles the line.
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, true, true),
            ExitCode::SUCCESS
        ));
        assert_eq!(both, std::fs::read_to_string(&managed).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Uninstall never removes a foreign file at the managed path (no banner), and leaves the conf
    /// untouched when there is no marked line.
    #[test]
    fn uninstall_spares_a_foreign_managed_file() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_foreign_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, "# someone else's file\n").unwrap();
        std::fs::write(&conf, "set -g mouse on\n").unwrap();

        assert!(matches!(
            uninstall(&managed, &conf, true),
            ExitCode::SUCCESS
        ));
        assert!(managed.exists(), "foreign managed file preserved");
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "set -g mouse on\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A conf the source-line removal cannot write fails the uninstall instead of reporting one:
    /// the managed file is already gone, so a success here would leave tmux sourcing a dead path.
    #[cfg(unix)]
    #[test]
    fn uninstall_fails_when_the_conf_line_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "tma_keys_unwritable_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&conf, "set -g mouse on\n").unwrap();
        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::SUCCESS
        ));
        let sourcing = std::fs::read_to_string(&conf).unwrap();

        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o400)).unwrap();
        // Root ignores the mode bit, so the write would succeed and prove nothing.
        if std::fs::write(&conf, &sourcing).is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        assert!(matches!(
            uninstall(&managed, &conf, true),
            ExitCode::FAILURE
        ));
        assert!(!managed.exists(), "the managed file was still removed");
        assert_eq!(
            std::fs::read_to_string(&conf).unwrap(),
            sourcing,
            "the conf is untouched, so the reported failure names real work left to do"
        );

        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Install refuses to overwrite a non-tma file sitting at the managed path (no banner), even
    /// with `--yes`, and leaves both the foreign file and the conf untouched. Symmetric to the
    /// uninstall guard above.
    #[test]
    fn install_refuses_to_overwrite_a_foreign_managed_file() {
        let dir = std::env::temp_dir().join(format!(
            "tma_keys_install_foreign_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let managed = dir.join("tma/tmux.conf");
        let conf = dir.join(".tmux.conf");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        let foreign = "# someone else's tmux.conf\nset -g mouse on\n";
        std::fs::write(&managed, foreign).unwrap();
        std::fs::write(&conf, "").unwrap();

        assert!(matches!(
            install(&managed, &conf, ConfigDir::Pinned, true, false, false),
            ExitCode::FAILURE
        ));
        assert_eq!(
            std::fs::read_to_string(&managed).unwrap(),
            foreign,
            "foreign file untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&conf).unwrap(),
            "",
            "conf never reached (install bailed at step 1)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
