# Changelog

Notable changes to tma, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and tma follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — while the major version is 0, a
breaking change bumps the minor.

Every release ships prebuilt tarballs and a `SHA256SUMS` file; see
[Install tma](docs/how-to/install-tma.md).

## [Unreleased]

## [0.5.8] - 2026-09-01

### Changed

- **An upgraded `tma` now replaces the older daemon it finds, instead of leaving it running until
  the tmux server restarts.** `[daemon] restart_on_upgrade` defaults to `true`, and the check no
  longer rides only on `tma daemon --ensure`: it runs before every user surface
  (`ls`/`status`/`jump`/picker/`watch`/`wait`/`subscribe`) and from `tma event`, independent of
  `[daemon] autostart`. A daemon keeps the detection code it started with, and a package upgrade
  touches no running process, so the old arrangement could leave a build from days ago serving a
  CLI you upgraded this morning, with nothing to tell you. The rule that makes this safe to leave
  on is unchanged and one-directional: strictly newer replaces older, equal never restarts, both
  versions must parse, the recorded pid must be alive, and no automatic restart may have fired for
  this server in the last 60 seconds. It only ever REPLACES: with no daemon running it does
  nothing, since starting one unasked is still `[daemon] autostart`'s job and that is still off by
  default. On the `tma event` path it is completely silent, because a hook's stderr can surface
  inside the agent's own UI. When versions already match it costs one file read and one liveness
  probe. Opt out with `[daemon] restart_on_upgrade = false`.

- **`[install] wrapper_ref` defaults to `"bare"`,** so `tma install-hooks` writes `tma-hook` into
  your agent configs rather than the wrapper's absolute path, and one `~/.claude/settings.json`
  works on every machine you sync it to. The two postures fail very differently, which is what
  decided this: a bare name that `$PATH` cannot answer is caught at install time, where install
  refuses and tells you how to fix it, while an absolute path pointing at another machine's home
  produces no error anywhere and simply never fires. Loud beats silent. Choose
  `[install] wrapper_ref = "absolute"` (or `--wrapper-ref absolute` for one run) when an agent is
  launched with a `$PATH` you cannot widen, such as an editor started from the desktop.

  Existing installs are unaffected and need no action. `tma install-hooks --check` and `tma doctor`
  now judge drift by what a reference RESOLVES to rather than by how it is spelled, so
  absolute-path entries that still reach the wrapper read as installed, not as stale. Only a
  reference that resolves to a different file, or to nothing, is reported. If you do run
  `tma install-hooks --all` to move to the bare form, note that codex pins its `hooks.json` trust
  to the exact command string: rewritten entries stay inert until you open codex, run `/hooks`, and
  trust them again. Codex's `notify` channel and every other agent are unaffected.

## [0.5.7] - 2026-09-01

### Changed

- **`prefix G` no longer leaves a TMA window behind after a jump.** The managed binding now opens
  the full-width watcher in a dedicated temporary tmux session. Enter jumps and exits the watcher;
  quitting exits it too, so either path destroys the one-use session. The pane where G was pressed
  remains the `jump --back` origin. A directly launched `tma watch` is unchanged and stays
  persistent unless `--temporary-session` is requested.

## [0.5.6] - 2026-08-30

### Fixed

- **The tmux-required CI job could hang until GitHub's six-hour job timeout.** Integration tests
  outside the `tma` package located its binary by running `cargo build -p tma` from inside the
  active `cargo test`. If the outer Cargo invocation still held the target directory lock, the
  nested Cargo waited for its parent while three identity tests waited behind a shared `OnceLock`.
  The test support crate now uses the binary the workspace build already produced and fails fast
  when a single-package test has not built it. The daemon-test gate also has a ten-minute deadline,
  and both tmux-required CI steps have a 20-minute timeout so a future harness hang cannot occupy a
  runner for six hours.

## [0.5.5] - 2026-08-30

### Fixed

- **A freshly started Codex pane sat at `?` until its first turn finished.** Codex draws inline
  rather than on the alternate screen, so a session that has not yet run a turn renders its welcome
  box and composer at the top of the pane and leaves the rest empty — on a 149x35 pane, 16 blank
  rows below a composer sitting 19 rows up. The idle rule read the last six rows of the capture, so
  it read six blanks and matched nothing; no other rule matched either, and with no evidence at all
  the pane held `unknown` until the first turn scrolled the composer to the bottom. Manifests can
  now scope a rule with `bottom_non_empty_lines(N)`, which drops trailing blank rows before taking
  its window, and the Codex idle rule uses it. The window is still six rows — the size is what keeps
  the transcript echoes and the approval dialog's `› 1. Yes, proceed` rows out — it just ends on the
  last row with content now.

## [0.5.4] - 2026-08-29

### Fixed

- **A working Claude pane read `idle` whenever its hooks went quiet.** Claude Code animated a
  braille spinner in its OSC title, and that was the only working evidence the bundled manifest
  could see on capture tier. At 2.1.246 the title is a static `✳ <task>` in every state, so that
  rule went permanently silent while the `✳` idle rule kept matching — and a pane thinking for four
  minutes without a tool call, which fires no hook, showed as idle for the whole stretch. Working is
  now read from the body spinner instead (`· Actioning… (4m 16s · ↓ 16.8k tokens)`, and the
  gerund-less `✻ Waiting for 1 background agent to finish`). The completion line reuses the same
  glyphs, so the ellipsis is what separates them, tested per line because a pane routinely shows a
  finished line above a live spinner. The title rule is kept for older Claude builds.

## [0.5.3] - 2026-08-25

### Fixed

- **One slow `list-panes` could cost a blocked pane a full sweep of latency.** The post-attach look
  added in 0.5.1 took its queue of just-covered sessions *before* the `list-panes` that drives it,
  so a read that hit the daemon's 3-second per-command cap discarded the look for good — and a pane
  that printed its prompt during the attach window then waited out the 45-second reconciliation
  sweep, which is the exact latency the post-attach look exists to remove. That cap is reached
  routinely on a saturated machine (on a 3-core CI runner, process spawn alone measures a 3.8-second
  median), so this was not a rare path. The seed is now retried, promptly: the queue is taken only
  after the read returns, and the daemon shortens its next wake to 250 ms while one is owed.
- **A push probe whose session-create call timed out leaked that session into the pool.** The
  timeout means only that the call did not return in three seconds; tmux had usually created the
  probe session anyway, and its id was lost with the error, so the daemon attached a control client
  to its own throwaway session and held it for the rest of its life. The probe now tears down by the
  name it chose, and the normal teardown retries by name if the id-keyed kill fails.

### Changed

- The daemon's `--status-file` gained `pending_seeds`, `seed_retries`, and `dropped_edges`. All
  three count work the daemon could not do because a tmux command timed out; a nonzero value is the
  signature of a machine slow enough that state updates are arriving on the sweep cadence rather
  than the near-instant quiet edge.

## [0.5.2] - 2026-08-25

### Fixed

- **A SIGTERM aimed the instant the daemon's socket appeared could kill it without cleanup.** The
  socket was bound before the signal handlers were installed, and the socket file is exactly the
  "daemon is up" signal `tma daemon --stop` and supervisors key on — so a TERM landing in that gap
  hit the default disposition, and the dead daemon left its socket file behind. Handlers are now
  installed before the bind, so a daemon whose socket exists always shuts down cleanly.

## [0.5.1] - 2026-08-25

### Fixed

- **A pane that printed while its control client was still attaching went undetected.** The daemon
  counted a control-mode client as coverage the moment it was spawned, but tmux streams `%output`
  only from the attach onward and never replays — so output produced in that gap (seconds wide on a
  loaded box) raised no quiet edge, and a hookless blocked prompt, being the absence of further
  output, never raised one later either. The pool now marks a session's panes active when its client
  actually attaches, so each gets one on-demand look a quiet threshold later. This also covers
  daemon start, new sessions, and client respawns after a dropped connection.
- **The reconciliation sweep drifted a full cadence late.** The serve loop re-armed a fresh sweep
  interval on every wake instead of waiting out the remaining time to the sweep deadline, so the
  45-second default could first fire at ~83 seconds — late exactly when the attach gap above needed
  the rescue. The poll now waits to the deadline.

- **`tma act` blamed the pane when an OpenCode permission reply came back `404`.** Re-firing
  `approve` at a request the server had already answered or withdrawn printed
  `tma: pane %0 vanished (exit 3)` for a pane that was plainly still there. The `vanished` outcome
  and exit 3 do not move (the act's target really did disappear), but the line now says what
  happened: ``tma: `approve` found nothing to answer on %0: the request was already answered or
  withdrawn (exit 3)``. `--json` `reason`, previously `null` on every `vanished` result, now carries
  which target went away: `request-gone` for the API 404, `pane-gone` for tmux's own
  `can't find pane` / `no such pane`.
- **`@agent_permission_request` outlived the reply that spent it.** After a 2xx from the API lane,
  the pane kept the answered request id until the OpenCode plugin's next `permission.replied` event,
  so anything reading the stamp as "a request is pending" read a spent id. The broker now clears it
  under the same held lock. A 404 still leaves the option alone, since it may already name a newer
  request.

## [0.5.0] - 2026-08-25

### Fixed

- **`tma act approve` fired `1` into Claude's plan-approval and trust dialogs.** All three of
  Claude's selection dialogs matched the same blocked rule and stamped `blocked/permission`, so a
  blind approve on a plan dialog pressed "Yes, and use auto mode" — silently switching the session
  into auto-approve — and on a trust dialog granted trust to the folder. Two new manifest rules now
  stamp them `blocked/plan` and `blocked/trust`, and `approve`/`deny` (which gate on
  `detail = ["permission"]`) refuse both with exit 4. Deny is co-gated: `tma act deny` also stops
  firing on plan and trust dialogs.
- **`tma act approve` on Codex confirmed whatever the dialog's cursor was on.** The approve key was
  `Enter`, which submits the current selection; driven live with the cursor moved, it denied. The
  key is now `y`, the accelerator Codex itself prints, which approves regardless of position.

### Changed

- **The notification payload no longer carries the pane title** (schema 1 → 2). A pane title is
  attacker-influenced text, and the payload flows to `[notify] command` sinks, third-party push
  carriers, and the 0644 notify log. `title` is now empty with `title_redacted: true` beside it;
  set `[notify] include_title = true` to restore the old behavior. The payload also gains
  `episode_ms`, the absolute episode instant `tma ls --json` already exports, so consumers can key
  and collapse notifications per episode instead of deriving it from the `since_ms` age.

## [0.4.6] - 2026-08-24

### Fixed

- **`tma wait --until done` could retract the very mark it was waiting for and then block to its
  timeout.** `done` is idle plus `@agent_attention`, and the ordered-input clear (the pass that
  retires a marker on a pane you are sitting at and typing into) ran *inside* the poll cycle, before
  the goal was evaluated against its rows. So a completion raised by some earlier cycle — the
  daemon's sweep, a status-line `tma status` — that you had typed past was taken down by the
  waiter's own first cycle and never seen. `wait` now defers the clear and applies it after
  evaluating the goal, the ordering the daemon already used around its notification dispatch. The
  clear still happens, so a marker on a pane its owner is typing into is retired even on a box where
  a long `wait` is the only thing running cycles.
- **`tma subscribe --events` could swallow a `done` edge entirely** for the same reason, and worse:
  with the clear landing before the row diff, the completion produced no `idle` → `done` edge at
  all, rather than merely being followed by its `done` → `idle` retraction. The stream now emits
  from the rows as it read them and clears afterwards, so both edges are reported in order.
- **`tma act --state done` and `tma mute --state done` could resolve to no target** for the same
  reason: their target-resolution cycle retracted the marker before the selector matched on it, so
  the pane you asked for failed with "no agent pane matched" (exit 3), and on `mute` that is the one
  pane most worth silencing. Both now defer the clear to after the selector has read the rows, so `done` means
  the same thing on every surface.

## [0.4.5] - 2026-08-24

### Added

- `tma daemon --stop` stops the daemon for this server and leaves it stopped, the counterpart to
  `--restart` for when you want it gone rather than replaced. Detection falls back to the poll tier,
  which is strictly additive, so nothing breaks — captures just wait for a surface to run. Nothing
  running is a clean exit 0 that says so.

- `tma daemon --restart` stops the daemon running for a server and starts one from the binary you
  ran it with, waiting until it answers. This is how an upgraded `tma` takes effect: a daemon keeps
  the detection code it started with, and `tma reload` re-reads config and manifests, not the
  binary. Until now there was no way to stop a daemon at all — the docs said "stop it and start it
  again" and left you to find the pid. It works in both directions, so running it from the older
  binary is how you deliberately go back. The stop is SIGTERM and is never escalated to SIGKILL: the
  daemon reaps its `tmux -C` control clients only on a clean exit, so a killed one would leave a
  control client behind per monitored session.
- `tma init` and `tma install-hooks` offer that restart when they find a resident daemon of another
  build, on the same show-it-then-confirm terms as their config writes (`--yes` accepts). Both
  rewire hooks to point at the new binary, which until now silently left the old build answering
  them.
- `[daemon] restart_on_upgrade` (default `false`) does it without being asked, on the `--ensure`
  that the keybindings launcher and `autostart` already run. **Strictly newer replaces older**:
  equal never restarts and an older `tma` never touches a newer daemon, so two installs sharing one
  tmux server cannot take turns evicting each other's daemon. An unparseable version on either side,
  a lock file whose pid is no longer alive, and a restart inside the last 60 seconds each veto it.

### Changed

- **`tma doctor --exit-code` now counts a daemon whose build differs from the CLI's.** It was
  reported but not gated, so a CI check could pass with a daemon running detection
  code from another release. The skew is not merely a latency cost: an event the old daemon maps to
  the *old* verdict is acknowledged, so the firing hook skips its own stamp and the transition is
  wrong rather than late. A lock file predating version recording still has nothing to compare and
  stays green. Pin the daemon build, or run `tma daemon --restart` in the job that upgrades `tma`.

### Fixed

- A daemon that had just taken the single-instance lock could be described by its predecessor's
  pid and build for the instant before it stamped its own. The lock file keeps its body when a
  daemon exits — only the flock is released — so a reader arriving in that window read a version
  belonging to a dead, possibly recycled, pid. The lock is now emptied the moment the flock is
  acquired, so that window reads as "unknown", which every reader already leaves alone.

## [0.4.4] - 2026-08-22

### Fixed

- A second completion's done mark is no longer taken down the instant it appears. When an agent
  finishes twice without tma seeing it go back to work — a codex pane whose `hooks.json` channel is
  not trusted yet is the common way in — the second completion re-raises the mark and records its
  own instant, because the state has not changed and the state's timestamp cannot move. The
  clear-on-input check was still reading that state timestamp, so it measured your keystrokes
  against the start of the whole idle run rather than against the completion. A prompt you typed
  before the agent finished counted as having seen the mark it raised afterwards, and the mark came
  down on the next cycle, before any surface had shown it. The notification still fired, which is
  how you could be pinged for something `tma status` never showed. It now reads the same instant
  every other consumer does.


## [0.4.3] - 2026-08-21

### Fixed

- A pane no longer gets stuck reading `unknown` after the agent briefly hands over the terminal.
  When something other than the agent owns the pane's screen — an editor, a pager, a shell command,
  or just the shell still sourcing its rc at startup — tma caps the pane at `unknown`, because the
  screen on display is not the agent's to read. That cap turns on a *process* fact, and a process
  fact flips back with no output at all. The freshness shortcut that skips a re-read asked only
  whether the window had been written to since the last stamp, so on a pane that then sat still
  there was nothing to notice: the agent had the terminal back, the verdict saying otherwise was
  void, and nothing would ever free it. `tma ls` and `tma wait` held `unknown` until something
  happened to write to that window. The shortcut now also refuses to reuse a stamp whose
  foreground fact no longer matches the one behind it, in both directions. Hook claims are
  unaffected — the cap holds those rather than replacing them.


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

[Unreleased]: https://github.com/pperanich/tmux-agents/compare/v0.5.8...HEAD
[0.5.8]: https://github.com/pperanich/tmux-agents/compare/v0.5.7...v0.5.8
[0.5.7]: https://github.com/pperanich/tmux-agents/compare/v0.5.6...v0.5.7
[0.5.6]: https://github.com/pperanich/tmux-agents/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/pperanich/tmux-agents/compare/v0.5.4...v0.5.5
[0.5.4]: https://github.com/pperanich/tmux-agents/compare/v0.5.3...v0.5.4
[0.5.3]: https://github.com/pperanich/tmux-agents/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/pperanich/tmux-agents/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/pperanich/tmux-agents/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/pperanich/tmux-agents/compare/v0.4.6...v0.5.0
[0.4.6]: https://github.com/pperanich/tmux-agents/compare/v0.4.5...v0.4.6
[0.4.5]: https://github.com/pperanich/tmux-agents/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/pperanich/tmux-agents/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/pperanich/tmux-agents/compare/v0.4.2...v0.4.3
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
