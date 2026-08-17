# Changelog

Notable changes to tma, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and tma follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — while the major version is 0, a
breaking change bumps the minor.

Every release ships prebuilt tarballs and a `SHA256SUMS` file; see
[Install tma](docs/how-to/install-tma.md).

## [Unreleased]

### Added

- `tma install-hooks --all` acts on every agent that already carries tma wiring, so one command
  repoints them all after an `[install] wrapper_ref` change or a moved binary, and
  `--all --uninstall` unwires every one. It is deliberately not "every agent tma supports": it only
  rewrites configs that already hold tma's wiring, so it cannot create a `~/.gemini/settings.json`
  for an agent you have never run. That also makes it broader than a `tma init` re-run, which wires
  only the agents whose launcher it finds on `$PATH`.
- `tma completions <shell>` writes a completion script for bash, zsh, fish, elvish, or powershell to
  stdout. It is generated from tma's own argument tree, so it covers every subcommand and flag and
  cannot go stale — worth something on a CLI where `install-hooks` alone carries sixteen. Fixed
  value sets complete too (`--wrapper-ref`, `--format`, and now `--state`/`--until`, whose parser
  reported no possible values to clap, so the state vocabulary reached you only by being rejected —
  it is in `--help` now as well). Runtime values are not completed: `--agent`, `--session`,
  `--repo`, `--branch`, and an action name need the tmux server and your config, which a static
  script cannot read. The internal verbs (`event`, `clear-attention`, `supervise`) and `daemon`'s
  five test hooks are left out; clap_complete's generators do not skip hidden items on their own, so
  tma prunes the tree before handing it over.
- Release tarballs ship the four Unix scripts in `completions/`, `scripts/install.sh` installs them
  for whichever of bash, zsh, and fish is on the machine (`TMA_NO_COMPLETIONS=1` skips it, and zsh
  gets told about the one `fpath` line its per-user directory needs), and the nix package installs
  them itself. `cargo install` still places a binary and nothing else.

### Fixed

- Re-installing after the wrapper path changes now repoints the existing hook entry instead of
  adding a second one beside it. Install, uninstall and `--check` all matched an entry by the exact
  command the running build would write, so wiring installed from another path (a moved binary, a
  `cargo install` over a `~/.local/bin` copy, a `wrapper_ref` switch) was invisible to all three:
  install left the stale entry in place and every event fired twice, uninstall walked past it, and
  `--check` reported a wholly-stale config as simply not installed. An entry is now recognised as
  tma's by its shape (`tma-hook <agent> <event>`, any path), and `--check` reports a stale one as
  stale. Claude, Gemini, Cursor and Codex's `hooks.json`; Codex's `notify` already matched this way.

## [0.3.1] - 2026-08-17

### Added

- `[install] wrapper_ref = "bare"` (or `tma install-hooks --wrapper-ref bare`) writes the wrapper's
  name, `tma-hook`, into your agent configs instead of its absolute path, so one
  `~/.claude/settings.json` works on both a Mac and a Linux box rather than naming a home directory
  that exists on only one of them. `$HOME` is not offered because three of the six wiring mechanisms
  spawn the wrapper as argv with no shell (Codex's `notify`, the OpenCode plugin, the pi extension)
  and would pass it through literally; a bare name is resolved by all six. Since a wrapper an agent
  cannot find fails silently, install refuses when `tma-hook` is not on the `$PATH` it can see, and
  `tma doctor` now reports the reference and whether `$PATH` still answers it (`wrapper: tma-hook ✓
  on $PATH (…)`, plus a `"reference"` key in `--json`). The default is unchanged.

### Fixed

- `hooks_state_is_keyed_per_server` no longer fails for anyone who has tma wired for real: its
  `--check` inspects every bundled agent, but only Claude's and Codex's paths were pinned to the
  test's scratch dir, so the developer's own `~/.gemini`, `~/.cursor`, `~/.pi` and OpenCode configs
  were read and their (correct) wrapper paths reported as drift. Test-only.

## [0.3.0] - 2026-08-17

### Breaking

- **The statusline context shim is opt-in.** `tma install-hooks <agent>` and `tma init` no longer
  touch your `statusLine` command; wire it with `tma install-hooks <agent> --statusline`, or remove
  an existing one with `--no-statusline`. It was the only wiring that edited a value you already own
  rather than adding tma's keys beside it, and it composed your statusline into a generated shell
  one-liner. The choice is recorded per agent, so you state it once: after `--statusline`, a later
  plain `install-hooks` keeps the shim current and `--check` stays quiet. A shim installed before
  this release has no record, so `--check` and `tma doctor` report it until you say which way you
  want it. What the shim buys is the context-window gauge (`@agent_tokens`) and the
  compact action that gates on it; hooks cover everything else.

### Changed

- Releases are gated on the test suite. The release workflow used to build straight from the tag
  without running a test, which is how v0.2.0 shipped over a red Linux lane; the build now waits on
  the same suite CI runs.

## [0.2.1] - 2026-08-17

### Fixed

- The daemon's status file no longer reports a `tma wait` subscriber that has already exited. A push
  to a departed waiter drops it, and that drop did not mark the status dirty, so the
  `wait_subscribers` gauge could overcount for up to one sweep (45 s). Introspection only — the
  daemon's own subscriber set, and therefore every push, was already correct.

## [0.2.0] - 2026-08-17

### Breaking

- **The managed sidebar is gone.** tma no longer splits, moves, or toggles a pane for you: it was a
  second layout owner competing with yours. `tma watch` stays as a surface you place yourself
  (`prefix G` opens it in a window, or split one wherever you like), and the popup picker remains the
  modal half. What this removes: `tma watch --toggle` (now exit 2, unknown argument), the `prefix W`
  binding, the `☰` status segment, and the `[status] sidebar` config key — which is a hard parse
  error now, not an ignored key, so delete it before upgrading. `tma doctor --json` renames
  `watch.sidebars` to `watch.watchers`. Re-run `tma install-keys` (add `--mouse` if you use it) to
  drop the stale `W` binding and the sidebar mouse arm from the managed file.
- **`tma install-keys` writes a daemon launcher by default.** The managed file gains one `run-shell`
  line that starts the event-hub daemon for whichever tmux server sources it, so a new server runs at
  tier 3 without being asked. `--no-daemon` omits it, and that is a standing choice rather than a
  one-off: a plain `--check` reports the missing line as drift, so pair `--check --no-daemon` in any
  script that verifies your install.

### Added

- Mouse support in the picker and `tma watch`. Hover underlines the row under the pointer, a click
  selects it, a second click on that row jumps (closing the picker, leaving `watch` open, exactly as
  Enter does in each), and the wheel moves the selection three rows without wrapping. Needs
  `set -g mouse on`; tmux scopes the grab to the pane or popup that asked for it, so no other pane
  changes behaviour.
- `tma install-keys --no-daemon` and `tma init --no-daemon`, for a setup that wires no daemon at all.
  `tma init --daemon` still also starts one for the server running the wizard.
- `programs.tma.daemon.autostart` in the Home Manager module, following `keybindings.enable` so the
  declarative and imperative install paths agree.

### Changed

- Every printable key now types into the picker's query. `a` opened the action menu and `1`-`9`/`0`
  jumped to a row whenever the query was empty, so an agent named `auth` or a branch named `2fa`
  could not be searched for at all. The action menu moves to `tab`, the digit quick-select is gone,
  and the row index numbers go with it (a number beside a row promised a key that no longer does
  anything). `tma watch` keeps `a` and now takes `tab` too.
- Releases carry real notes. This file is the source: the release workflow reads the section for the
  tag and refuses to build one that has no entry.

### Fixed

- One row is marked at a time in the picker and `tma watch`. Hover was a dim reversed block beside
  the selection's bright one, so two rows read as two selections, and hovering your own selection
  dimmed it. Hover is an underline now, and any keypress drops it.
- The `tma-hook` wrapper stays silent when `dirname` is not on `PATH`, instead of letting the shell
  write `dirname: not found` to stderr, which an agent's hook runner surfaces. It also no longer
  falls back to `$PWD` when resolving the binary, which could exec a stray `tma` from whatever
  directory the agent happened to run in.
- The test harness no longer turns a leaked scratch tmux server into a permanent orphan. A
  cooperative `kill-server` that never finished tearing down was followed by an unconditional
  unlink of the socket file, leaving a server that no later run could find or reap. Contributors
  only; it does not affect a tma install.

## [0.1.1] - 2026-08-17

### Fixed

- `tma doctor` keeps reporting when the process walk fails, rather than giving up the whole report.

### Documentation

- CI and release badges on the README.

Otherwise a packaging release: the CI tmux pin and the release workflow, with no change to tma
itself.

## [0.1.0] - 2026-08-16

Initial release. An agent state monitor for tmux: it watches the panes your coding agents run in,
stamps each one's state (working, blocked, idle) onto tmux pane options, and gives you a picker, a
live dashboard, jump bindings, and a status-line segment over the result. Detection works with zero
setup by walking the process tree, gets faster and more precise when you wire the agent's own hooks,
and becomes push-based with the optional daemon.

[Unreleased]: https://github.com/pperanich/tmux-agents/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/pperanich/tmux-agents/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/pperanich/tmux-agents/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/pperanich/tmux-agents/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/pperanich/tmux-agents/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/pperanich/tmux-agents/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pperanich/tmux-agents/releases/tag/v0.1.0
