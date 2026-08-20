# Changelog

Notable changes to tma, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and tma follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — while the major version is 0, a
breaking change bumps the minor.

Every release ships prebuilt tarballs and a `SHA256SUMS` file; see
[Install tma](docs/how-to/install-tma.md).

## [Unreleased]

## [0.4.2] - 2026-08-20

### Changed

- The done mark's clear-on-departure rule now says what it covers: a pane or a window, never a
  whole session. Switching to another session with `switch-client` leaves the mark standing on the
  pane you were watching, deliberately, and coming back and typing takes it down as it always has.
  If you were thinking of wiring the session clear yourself, the docs now say what each candidate
  hook actually does: `client-session-changed` cannot tell a real switch from
  `switch-client -t <the session you are already on>`, and `pane-focus-out` — which can — also
  fires when you detach and when any popup or menu opens over the pane, and stays silent whenever
  another client (a tma daemon's included) is attached to the session you left. No behaviour
  changed; nothing new is installed.

## [0.4.1] - 2026-08-20

### Added

- `episode_ms` on every JSON agent row (`ls --json`, `wait --json`), alongside `since_ms`. It is
  the instant `wait --since` actually compares against: the later of the state transition and the
  last turn end. The two agree until a pane completes a second turn without leaving `idle`, and
  from there `since_ms` is a floor the row already clears — a supervisor loop feeding it back would
  re-satisfy on every lap instead of blocking for the next completion. The loop recipes now read
  `episode_ms`; `since_ms` is unchanged and still `@agent_since`.

  **If you have a supervisor loop that feeds `since_ms` back into `wait --since`, switch it to
  `episode_ms`.** It keeps working as-is on every pane that never completes twice inside one idle
  run, and on one that does it spins rather than blocks. Before this release the same loop silently
  missed that second completion instead, so neither reading was correct — this is the version where
  the right one exists.

### Fixed

- A second completion now raises the done mark again. Once you had seen the first one and it came
  down, a pane that finished a second turn without tma ever observing it *working* in between
  raised nothing at all, and nothing recovered it — you simply got no signal for the second turn.
  The knowledge was there and being thrown away: the hook that fires means "a turn ended", and the
  intake was re-deriving the mark from the previous state instead, which is all the screen fold can
  do and exactly what it cannot decide (an idle→idle edge looks the same as a pane sitting still).
  The manifests now name their turn-end event, and the intake raises on it. Most exposed was a
  Codex pane whose `hooks.json` entries were never trusted in the TUI: its `notify` channel needs no
  trust and reports only turn ends, so *every* completion of that pane's life was this edge.
  `tma subscribe --events` reports the re-raise as an `idle` → `done` edge.
- The desktop notification follows the mark. Its per-episode dedup keyed on `@agent_since`, which is
  write-once while the state is unchanged and so could not move for a second completion on a pane
  that never left `idle`; the new `@agent_turn_at` carries that instant and the dedup reads the
  later of the two. `tma wait --until done --since <T>` sees the second completion for the same
  reason, so a supervisor loop can act on one and wait for the next. One turn end reported on two
  channels (Codex sends both `Stop` and `notify`) still marks and rings once.
- The notification a second completion fires reports its own age again. The payload's `since_ms`
  (and `TMA_SINCE_MS`) is documented as how long the episode had been standing when the hook ran,
  which for the daemon is its dispatch latency; on a re-raised mark it was reading back to the start
  of the idle run instead, so a hook that logs or thresholds on it saw hours where it expected
  milliseconds.
- A pane whose agent is replaced no longer inherits the old one's last turn. The episode reset
  already cleared the mark and the notification marker but left `@agent_turn_at` standing, which a
  backward wall-clock step could have let decide the new episode's instant.

## [0.4.0] - 2026-08-20

### Changed

- The done mark now clears on your next input at a pane you are already sitting on. Leaving is
  covered by the tmux hooks; this is the case they cannot see, because it involves no navigation at
  all: the agent finishes under your eyes and you just keep typing at it. The poll cycle now asks
  tmux which pane each attached client is displaying and when that client was last typed into, and
  takes the mark down only when that input came *after* the mark went up. The whole invariant, in
  one line: **the done mark survives until your next input while that pane is on screen, or until
  you navigate off it.**

  It is an ordering, not a timeout, and that distinction is the design. "Cleared if you typed in
  the last thirty seconds" would erase the mark for the very thing it exists for — type a prompt,
  walk away, come back to a check mark. Since an absent person types nothing, a mark raised after
  your last keystroke stands for as long as you are gone. Two limits are worth knowing. Under a
  control-mode client (iTerm2's `-CC`) tmux freezes the client's input clock at attach, so there
  the hooks remain the only clear. And someone who reads a pane without touching the keyboard looks
  exactly like someone who is not there, so their mark waits for them. One nuance the other way: if
  you run with `focus-events on`, your terminal reports focus changes as input, so switching to
  another application also takes the mark down — the same rule as walking off the pane. `tma subscribe --events`
  gains `done` → `idle` edges from this, meaning "the user saw it".

- The done mark now clears when you leave an agent's pane, not only when you arrive at one. The
  case it was missing is the ordinary one: an agent finishes while you are sitting there watching
  it, you move to another window, and the flag stays up on the pane you were just looking at —
  counted by `tma status` and offered by `prefix-j` for as long as you stay away. The two always-on
  tmux hooks now tell `tma clear-attention` which of them fired, and it clears the pane you left as
  well as the one you moved to. Walking away is untouched, and not by a timeout: leaving an agent
  running and going to lunch means you never navigate, so no hook fires and nothing clears. A pane
  switch in some other window clears only that window's departed pane, and navigation that moves
  nothing — selecting the pane or the window you are already in — clears nothing anywhere.
  **Re-run `tma install-hooks <agent>` to pick this up** — the hooks live in tmux server state, so
  upgrading the binary does not rewrite them; the old command reads as drift and install replaces
  it in place.

  The window half hangs off tmux's `session-window-changed` notification rather than
  `after-select-window`, and `tma install-hooks` removes the latter if an earlier build wired it.
  tmux runs `after-select-window` even for a `select-window` onto the window you are already in,
  where the "window you left" it reports is whatever window you left however long ago — so on that
  hook the departure clear could take the mark off a pane you had not looked at since. `tma jump`
  and the picker also stop issuing that no-op selection at all when the pane you asked for is in
  the window you are already in.

### Fixed

- A codex, cursor, gemini, opencode or pi pane no longer stays stuck on the spinner after its turn
  ends. Each of those manifests described what a working screen looks like and said nothing about
  an idle one, so the moment the streaming chrome scrolled away there was no claim on the screen at
  all — and with nothing to weigh, every later cycle held the previous verdict. A pane that finished
  half an hour ago still read `working`, and no amount of waiting moved it, because waiting was the
  problem. All five now carry an idle rule anchored on their composer: codex's `›` arrow, cursor's
  input frame, gemini's composer box edge, opencode's `ctrl+p commands`, pi's
  input rules plus context gauge. Each anchor is backed by real captures at two widths, including a
  freshly driven post-turn cursor screen — its previous idle fixture was a fresh session, whose
  prompt text turns out not to survive the first turn. The composer is on screen mid-turn too, so
  these rules deliberately co-render with the working ones and lose to them, exactly as claude's
  `⏵⏵` rule already did; and idle stays out of each manifest's `[capture].visible`, so working
  chrome still cannot argue a hook's idle claim away.

- A pane whose screen merely changed is no longer reported as `working`. Every cycle hashes the
  viewport, and a hash differing from the stamped one used to be pushed as working evidence in its
  own right. It carried the same timestamp as the manifest's own rules and was appended after them,
  so on a tie it won, and `@agent_source` then blamed `activity` for verdicts a screen rule had
  actually made. Worse, it outranked real idle chrome: typing at a finished prompt corroborated the
  stuck `working` claim and postponed the decay that would have recovered it, so the check mark you
  were waiting for arrived only once you stopped touching the pane. The hash keeps its real job,
  deciding whether a cycle can skip the capture and reuse the stored stamp, and no longer claims
  anything about state. Panes stamped `@agent_source=activity` by an older build still decode, and
  the next verdict re-sources them.
- The attention flag is cleared again when you select an agent's pane. The two always-on tmux hooks
  ran `tma clear-attention '#{hook_pane}'`, but tmux populates `hook_pane` only on the
  notify-pane-style hooks; on the hooks tma installs it expands empty, and an
  empty pane argument is a no-op. So the default install cleared attention never, and the done check
  mark stayed on a pane you had already visited. The hooks now pass `#{pane_id}`, which resolves in
  all three. An existing install is repaired by re-running `tma install-hooks` — the old shape reads
  as drift and is rewritten in place.

## [0.3.6] - 2026-08-20

### Fixed

- Nix-installed agents are detected again. `wrapProgram` renames a binary to `.<name>-wrapped`, and
  tmux reports that name verbatim as `#{pane_current_command}` — truncated to `.opencode-wrapp` on
  macOS, which is 15 characters. No manifest matched it, so tma decided the agent was not what owned
  the pane's screen and capped the pane at `unknown`: a bare `?` in the status bar. The screen tier
  went with it, since that check runs before any screen rule, so even a permission dialog could not
  be seen. tma now strips the wrapper decoration before matching, which covers every agent at once.
  Claude, OpenCode and pi installs from a Nix profile are all affected; Claude hid it best, because a
  live hook claim kept the state right while its screen fallback was dead.
- The pane's foreground is now settled against the terminal's own foreground process group rather
  than the command name alone. A name can only answer that question while the executable is called
  what the manifest expects, which is what Nix broke. The process-group check is a veto — it cannot
  tell a launcher's child from its parent — but it removes a false positive the name never could:
  Cursor, Gemini and pi all match a bare `node`, so an unrelated `node` in the foreground (a dev
  server, a build watcher) used to read as that agent being on screen.
- An OpenCode pane no longer sits at `?` until you send it a message. OpenCode fires
  `session.created` only for a brand-new session, so a TUI waiting at its prompt and
  `opencode --continue` announced nothing, and OpenCode's only screen rule was for `blocked`. The
  plugin now registers when it loads, which marks the pane idle — the honest state for a waiting
  prompt. Re-run `tma install-hooks opencode` to pick this up.

### Added

- OpenCode's `working` state is now detected on screen, not only through its plugin, anchored on the
  in-flight status row's `esc interrupt` hint. A pane whose hooks are not wired, or whose hook claim
  has aged out, reads `working` during a turn instead of `unknown`. Its `idle` remains hook-only:
  idle is the absence of that row rather than any chrome of its own, so as with pi a hookless pane
  holds `working` after a turn ends until a hook moves it.

## [0.3.5] - 2026-08-18

### Fixed

- An absolute wrapper reference is no longer a package-store path. `install-hooks` writes the
  wrapper's own path into each agent config, and under Nix on Linux that path is
  `/nix/store/<hash>-tma-<version>/bin/tma-hook`: it names one build, so the hooks broke at the next
  `nix flake update`, when that path was collected. tma now writes the stable path that reaches the
  same file, found by walking `$PATH` for a `tma-hook` outside any store that resolves to it (your
  profile's `bin`), so the substitution happens only when the two are provably the same install.
  Running from a store with no profile install (`nix run`) has no such path, and install says so
  rather than wiring one that will vanish. `/gnu/store` counts too.
- `tma doctor` no longer reports a wrapper as `bare` on the strength of the reference and the file
  differing, which is now also what a store path's stable alias looks like. It carries the answer
  from the resolver instead, and prints the file as an aside in both cases.

## [0.3.4] - 2026-08-18

### Fixed

- `tma init` and `tma install-hooks` work from a read-only install prefix. Both write the `tma-hook`
  wrapper next to the binary, which under Nix is the store, so the first step of a wizard run died on
  `Permission denied` for `/nix/store/…/bin/tma-hook` (or the per-user profile path macOS reports
  instead). The Nix package now installs that wrapper itself, and the write is skipped when the file
  on disk is already the script this build would produce, so nothing needs writing. A write that does
  fail for permissions now names `--wrapper-path`, which was documented as a Nix prerequisite and
  mentioned nowhere near the error.
- The bare-wrapper shadow warning stops firing on a symlinked `$PATH` entry that answers with the
  very file install wrote (a Nix profile's `bin`, a stow tree). The two were compared as directories,
  so the profile and the store path it points into read as two competing installs.

## [0.3.3] - 2026-08-17

### Added

- `tma ls` plain output carries the git labels its `--json` rows already did: `repo`, `branch` and
  a `worktree` marker, appended as columns ten through twelve so a pipeline reading `$1`-`$9` is
  unaffected. All three are empty for a pane in no checkout, the one case the plain form cannot
  distinguish from a failed resolve (`--json` still nulls them). `tma wait`'s matched rows share the
  renderer, so they gained the same columns.

### Changed

- Repo/branch resolution spawns its `git rev-parse` calls as one batch instead of one after
  another, and polls for their exit on a 1 ms backoff rather than a flat 10 ms sleep. Each pane used
  to pay a sleep longer than git's own runtime, serially: `tma ls` over six agents in five checkouts
  measured 100 ms before and 57 ms after. The bounded 3 s deadline is now shared by the batch, and a
  cwd repeated across panes is spawned once.

### Fixed

- The picker paints branch labels on its first frame. It seeds from tmux pane options for an instant
  open, and only the refresh a second later annotated repos, so the branch column arrived visibly
  after the rest of the row. The seed is annotated now, which the batching above makes cheap enough
  to sit in the open path. That path runs before the terminal is in raw mode, so it carries a 250 ms
  budget rather than the resolver's full 3 s: a git slow enough to hit it costs a bare column on the
  first frame, never a late window.
- `tma watch --repo`/`--branch` no longer opens on an empty frame. The selector ran against seed
  rows that carried no repo label yet, so it matched nothing until the first refresh replaced them.
- A `git rev-parse` killed at its deadline no longer caches "this pane is in no repo" for the memo's
  five seconds. The resolver collapsed a timeout and a real answer into the same unresolved result,
  so on a filesystem slow enough to hit the bound, one timeout suppressed the retries that would
  have succeeded. A timeout now leaves the memo untouched and the next refresh resolves again.

## [0.3.2] - 2026-08-17

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

[Unreleased]: https://github.com/pperanich/tmux-agents/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/pperanich/tmux-agents/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/pperanich/tmux-agents/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/pperanich/tmux-agents/compare/v0.3.6...v0.4.0
[0.3.6]: https://github.com/pperanich/tmux-agents/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/pperanich/tmux-agents/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/pperanich/tmux-agents/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/pperanich/tmux-agents/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/pperanich/tmux-agents/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/pperanich/tmux-agents/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/pperanich/tmux-agents/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/pperanich/tmux-agents/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/pperanich/tmux-agents/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/pperanich/tmux-agents/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pperanich/tmux-agents/releases/tag/v0.1.0
