# Changelog

Notable changes to tma, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and tma follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — while the major version is 0, a
breaking change bumps the minor.

Every release ships prebuilt tarballs and a `SHA256SUMS` file; see
[Install tma](docs/how-to/install-tma.md).

## [Unreleased]

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

[Unreleased]: https://github.com/pperanich/tmux-agents/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/pperanich/tmux-agents/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/pperanich/tmux-agents/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pperanich/tmux-agents/releases/tag/v0.1.0
