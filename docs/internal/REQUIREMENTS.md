# tmux-agents — Design Requirements

Status: draft for review
Date: 2026-07-20

Everything here is written to be designed against: numbered, testable where possible, and
ranked. `MUST` / `SHOULD` / `MAY` carry RFC-2119 meaning. Implementation approaches come
later and should cite these IDs. Mentions of "the PRD" cite the retired
product-requirements draft, whose surviving argument is now
`docs/explanation/why-tma.md`.

This project is a clean-room design. It takes architectural *ideas* from herdr (process
identify + screen-rule detection, evidence-based manifest authoring) but ports no herdr
code and no herdr manifest files. All detection rules are authored from first-party
captured evidence (see D10, X3).

## 0. Definitions

- **Agent pane**: a tmux pane whose process tree contains a recognized coding-agent
  process (claude, codex, gemini, cursor-agent, ...).
- **State**: one of `idle` (prompt shown, nothing running), `working` (processing),
  `blocked` (waiting on human input), `unknown` (recognized agent, unreadable evidence).
- **Poll cycle**: one pass of discover → capture → detect → publish.
- **Stamping**: writing detected state into tmux pane/window user options.
- **Surface**: any user-facing output (picker, watch pane, status line, JSON).

## 1. Functional requirements

### Discovery and identification

- **F1** MUST enumerate all panes in all sessions of the current tmux server, attached
  or detached, in one `list-panes -a` call per poll cycle.
- **F2** MUST identify which agent runs in a pane from process evidence: `#{pane_current_command}`
  as the cheap filter, full process-tree walk (single `ps` per cycle) as the authority.
  Rationale: direct launches show the agent binary as the pane command (verified live,
  appendix A), but wrappers (`npx`, `node`, shims, `nvim` terminals, nested shells) hide it.
- **F3** MUST support the initial agent set: Claude Code, Codex CLI, Gemini CLI,
  Cursor CLI. SHOULD make adding an agent a manifest-plus-table change, no core code.
- **F4** MUST re-evaluate identity every cycle: agent exits → pane reverts to non-agent
  and all published state for it is removed (see F16); a new agent later in the same
  pane is picked up fresh.
- **F5** MUST handle the wrapper-TUI case (agent process found in tree, but screen owned
  by e.g. nvim or another multiplexer). The mechanism is a *process fact*, not a screen
  inference (round-2 fix — "contradiction detection" had no operational trigger, and
  hold-previous would otherwise swallow it): when the pane's foreground process /
  `#{pane_current_command}` is not the identified agent, all screen evidence for that
  pane is capped at `unknown`, overriding hold-previous. This also kills the false
  positive where agent chrome *displayed as file content* in an editor matches screen
  rules. Never let process identity alone assert idle/working/blocked.
- **F6** MAY treat panes running ssh/containers as out of scope for v1 (remote process
  trees are invisible). MUST NOT misclassify them as local agents.

### State detection

- **F7** MUST detect state from these evidence sources: live-viewport text
  (`capture-pane -p -e`), pane title (`#{pane_title}`, carries agent OSC titles —
  verified: Claude Code publishes `✳ <task>` idle / `⠐ <task>` braille-spinner working),
  `#{window_activity}`, and pane flags (`#{alternate_on}`, `#{scroll_position}`).
  The viewport content-hash delta between cycles is a **capture-scheduling input**, not a
  detection source: an unchanged hash lets a cycle reuse the stored stamp, but a changed one
  makes no state claim of its own (amended; it used to claim `working`).
- **F8** MUST arbitrate sources in a fixed, documented order. Baseline: fresh agent hook
  event (F26) beats everything; then visible blocker chrome; then visible working
  chrome ⇒ working; then visible idle chrome ⇒ idle; else hold previous state (stateful) or
  report `unknown` (one-shot). Hook-event authority MUST decay, but decay is
  coverage-aware: a stale hook claim is expired by *process* evidence (pid gone) for
  any state, and by *screen* evidence only for states the agent's manifest declares
  capture-visible (`[capture].visible`, AD8). Screen evidence MUST NOT expire a hook
  claim for a hook-covered, capture-invisible state — letting the sweep flip it
  produces D1's worst failure. Round-2 carve-out, round-3 precision: **visible
  blocker chrome overrides a `working`/`idle` hook claim iff the stamped
  `@agent_evidence_at` predates the capture's timestamp** (evidence ordering — a
  hook claim newer than the capture wins, closing the answered-prompt false-blocked
  race; a hook claim older loses with no decay wait). Stated identically in AD4 and
  DAEMON.md. Coverage gating protects only the flips D1 tolerates.
  Freshness-window defaults (config-overridable, previously unspecified):
  `working`/`idle` hook claims decay after 60 s without corroborating evidence;
  `blocked` hook claims decay only after 300 s, and only against positive contrary
  chrome on a manifest that declares `blocked` capture-visible (silence never expires
  one). Process evidence and a fresh hook event expire any claim at any age.
- **F9** MUST only ever match against the *live viewport*, never scrollback history.
  When a pane's viewport is above the live screen (`#{scroll_position}` > 0), freeze the
  last known state instead of matching. Copy-mode at offset 0 still shows the live
  screen, so it does not freeze.
- **F10** MUST support screens that show history rather than live state (transcript
  viewers, pickers, help overlays) via an explicit skip mechanism in the rule format —
  such screens freeze state, not reset it.
- **F11** MUST work identically for panes in detached sessions and non-visible windows
  (tmux maintains their screens; verified capture works). Agents running while the user
  is away are the core use case.
- **F12** SHOULD apply hysteresis before *publishing* a transition (notification,
  stamped option) so stream pauses don't flap working↔idle. Dwell state MUST NOT
  require producer-local memory or shared counters. Precise rule (round-2 fix — the
  earlier wording was ambiguous about *whose* evidence age, and one reading
  livelocked): `@agent_evidence_at` records the most recent evidence *consistent
  with the stamped state* (AD4 writes-on-hold rule); dwell is **asymmetric**,
  applying only to working→idle: publish idle only when `now - @agent_evidence_at`
  (age of the last working-consistent evidence) exceeds the dwell window
  (configurable, default ~3 s). `blocked` (direct evidence per D2) and idle→working
  publish immediately (D3). Computable by any producer from stamped values alone. A
  producer suppressing a transition refreshes `@agent_stamped_at` only. The picker
  MAY show raw state immediately.

### Publication and tmux integration

- **F13** MUST stamp detected state onto tmux as pane user options and per-window
  rollups, per the consolidated schema in ARCHITECTURE.md AD4 — including evidence
  provenance (`@agent_source`, `@agent_evidence_at`) and per-pane freshness
  (`@agent_stamped_at`): without persisted provenance, a stateless producer cannot
  rank a hook stamp above its own capture verdict and will clobber `blocked` (the
  central defect found in adversarial review). Formats/hooks reading these options
  are the public integration API.
- **F13a** Writes MUST follow the AD4 write-ownership rules, enforced by
  **server-side conditional writes** (`set-option -pF` guards evaluated atomically in
  the tmux server — verified on 3.6a; probe at the 3.2 floor per N10), never by
  producer-side read-then-write (round-2 review: advisory rules are TOCTOU and the
  clobber race survives them). Hook-sourced state is protected by the guard;
  `@agent_since` and `@agent_notified_at` are write-once per episode via
  guard-before-state chaining; pid mismatch vs `@agent_pid` is an episode boundary;
  all other stamped values MUST be deterministic functions of persisted inputs so
  concurrent writers converge. Writers MUST stamp `@agent_stamped_at` last; the
  read-consistency rule (a `stamped_at` older than `state` marks an in-progress
  write) is part of the public API documentation.
- **F14** MUST keep the stamped-option names and value grammar stable once released;
  changes are breaking changes. Exception: the `@agent_detail` vocabulary is
  explicitly unstable until 1.0 and documented as such (consumers warned against
  glob-matching detail tokens in tmux conditionals before then).
- **F15** MUST expose the same data as line-oriented and JSON output (`tma ls`,
  `tma ls --json`) with a stable schema, so users can compose with fzf/scripts without
  the built-in picker.
- **F16** MUST remove or refresh stale published state: pane options die with panes
  (tmux handles), but window summaries and any daemon-side records MUST be recomputed
  each cycle; a dead daemon MUST NOT leave permanently stale "blocked" flags that a
  later `tma status`/picker run would repeat. Freshness is judged per pane
  (`@agent_stamped_at`); the server-scoped `@tma_last_poll` is a hint only — in a
  mixed fleet a server-wide marker kept fresh by hook-active panes would mask a dead
  hookless pane's stale state indefinitely.

### Navigation

- **F17** MUST jump to a chosen agent pane across sessions in one action
  (`switch-client` + `select-window` + `select-pane`), from picker and from
  `tma jump --blocked|--next` for direct keybindings.
- **F18** SHOULD remember the jump origin and offer a "return to where I was"
  (`tma jump --back`), since cross-session jumps lose tmux's own last-window affordances.
  Origin MUST be resolved via client queries (`#{client_session}`, active pane) — a
  popup process's `$TMUX_PANE` is a hidden internal popup pane, invisible to
  `list-panes`, and useless as an origin (verified round 2). Storage: a
  server-scoped option keyed by sanitized client name
  (`@tma_origin_<client>` = `session:window.pane`), single-level (last jump only) —
  tmux has no client-scoped user options.

### Surfaces

- **F19** Picker (`tma`, run under `display-popup -E`): fuzzy-filterable list of agent
  panes; default sort blocked → working → idle, then by time-in-state; each row shows
  state glyph, agent, `session:window.pane`, and title text (titles carry the task
  summary — verified, appendix A); preview shows live tail of highlighted pane;
  refreshes while open; Enter jumps, Esc closes.
- **F20** Status one-liner (`tma status`): counts by state with glyphs and tmux
  `#[fg=]` styling; prints empty string when no agents (so the status line collapses);
  never blocks the status line (see N2).
- **F21** Watch (`tma watch`): persistent live dashboard designed to run in a normal
  pane, wherever the user puts it; non-modal by requirement — popups are modal.
  Deferred to phase 3 (SHOULD, not v1): tmux-agent-sidebar already serves this need
  maturely, and picker + status + jump deliver the core loop; descoped per
  adversarial review with user approval 2026-07-20. *(Shipped H6a `ec37512` + H6b
  `d0942b0`, 2026-07-22: a persistent `[picker]`-styled pane — first frame from
  stamps (N3), 1 s guarded-poll refresh, H3 hot-reload; Enter jumps the acting
  client (F18) and clears the target's attention but keeps the sidebar open; the
  middle-tier SIGUSR1 nudge (`@tma_watch_pid`, advertised pane-scoped on the
  sidebar's own pane, fired by the `after-select-*` attention hook) refreshes it at
  focus-change latency, ~200 ms worst case. H9 `7acb753`, 2026-07-25: on a
  wide-enough pane (>=76 columns) the body splits to carry a live ANSI preview of
  the highlighted agent's pane beside the list, re-captured on selection change and
  every refresh tick; below the threshold the single-list body is unchanged, and
  a narrow pane adds zero tmux calls. Width-driven, no toggle key, no config knob.
  2026-08-17: the sidebar framing is withdrawn. `tma watch --toggle` (which
  split/moved/killed a managed pane), the follow-the-jump pane move, and the `☰`
  status segment that drove them are removed; tma places no pane and moves none.
  What remains is the surface itself, run wherever the user wants it — `prefix G`'s
  own window, a hand-written split, or a terminal outside tmux. Rationale: a
  managed pane is a second layout owner competing with the user's, and the popup
  picker already covers the modal half of the loop.)*
- **F22** Notifications on transition into `blocked` (daemon phase): tmux
  `display-message` as baseline; user-configurable hook command receiving structured
  env/JSON (enables desktop notifications, ntfy, etc.); MUST respect hysteresis (F12)
  and MUST NOT re-notify for the same continuous blocked episode. Episode dedup MUST
  use the persisted `@agent_notified_at` marker (written only by the notifier;
  DAEMON.md "Notification dedup") — never daemon memory, never the attention flag,
  never a key derived from observation-time values. Round-2 precision: the marker is
  written **before** firing (at-most-once; a crash between write and fire drops one
  notification, accepted and documented); "predates" is strict
  (`notified_at < since` fires, equal does not); a daemon starting up MUST treat a
  pre-existing `notified_at >= since` as already-notified for the current episode.
  H2 extension: the `notify.on` config array (F23) widens the *trigger set* only —
  `"done"` adds the working→idle completion (an idle landing carrying
  `@agent_attention`, the H1 done surface) — while this dedup mechanism is reused
  unchanged. `@agent_since` is write-once per state, so a blocked-then-done
  sequence in one agent episode re-arms at the state transition and fires once for
  each trigger; the payload carries the trigger word (`blocked` / `done`), never
  the raw landing state token. Default `["blocked"]`: this requirement's original
  behavior is unchanged unless the user opts in.

### Agent hook integration

- **F26** MUST provide an event bridge from agent hooks to tma: resolves its pane
  from `$TMUX_PANE`, delivers the event to the daemon when one is running, and
  **stamps tmux options directly when no daemon is running** (daemonless mode stays
  event-driven for hook-capable agents); in daemonless mode it also recomputes its
  window's `@agent_summary` (deterministic rollup). The frozen public interface is
  the *wrapper's* argument contract (`<agent> <event-name>`, payload on stdin) — the
  underlying `tma event` CLI is internal and unstable, so it can evolve as the
  Codex/Cursor payload audits land without breaking users' agent configs.
- **F27** MUST guard pane ownership for hook events: subagents share the parent's
  `$TMUX_PANE` and fire hooks with foreign session ids/cwds (bug class documented in
  tmux-agent-sidebar). Pane-scoped identity writes MUST be gated so a subagent event
  cannot clobber the parent pane's registration; subagent lifecycle events are tracked
  but do not change top-level pane state.
- **F28** MUST install hooks via a stable wrapper script, not a direct binary path
  (late-binding pattern from tmux-agent-sidebar's `hook.sh`): agent settings reference
  the wrapper; the wrapper resolves the binary at fire time and exits 0 silently when
  the binary is absent, so the agent never observes a hook failure.
- **F29** MUST provide `tma install-hooks <agent>` and `tma install-hooks --check`:
  idempotent, additive, diff-before-write, symmetric uninstall. Hook *installation*
  requires a small per-agent installer adapter in core (agent config formats are not
  uniform: Claude Code JSON hooks vs Codex `notify` in config.toml); the manifest
  declares the event-to-claim mapping, the adapter knows where to wire the wrapper
  (X1 honest split, ARCHITECTURE AD8).
- **F30** tma writes outside its own domain in exactly two places, both requiring
  explicit invocation and symmetric removal: (a) the user's agent config (F29), and
  (b) tmux server hooks — the attention auto-clear and SIGUSR1 nudge hooks, which
  MUST use `after-select-pane`/`after-select-window` (NOT `pane-focus-in`, which is
  gated on `focus-events`, default off — a focus-hook auto-clear silently never
  fires; verified round 2). Hook commands MUST bind their pane via `#{hook_pane}`
  format expansion, never `$TMUX_PANE` (`run-shell` inherits the server's startup
  environment; verified stale/foreign). Installation: **unindexed** `set-hook -ga`
  append (tmux assigns the next free index — verified safe; explicit indexes
  silently overwrite occupants), then record the assigned index from `show-hooks`.
  Known hazard, detected not prevented: a user's own unindexed `set-hook -g`
  replaces the entire hook array on config reload, deleting tma's entry;
  `install-hooks --check` MUST detect missing hooks and offer reinstall. Uninstall
  removes exactly the recorded entries.
- **F31** MUST provide `tma wait`: block until a targeted agent pane reaches one of a
  set of states, then print the pane's row and exit with a contract exit code, so a
  script can *wait* on an agent (agent-to-agent coordination) rather than poll `tma ls
  --json` or react to a `notify.on` push. Targeting is exactly one of `--pane <id>` /
  `--agent <name>` / `--any`, with `--session <name>` filtering the latter two (rejected
  with `--pane`, whose id is already unique); an `--agent` matching more than one pane is
  a deterministic error naming the candidates, never a silent first-match. `--until` is a
  comma-separated set of the four closed state tokens (`idle|working|blocked|unknown`,
  F14) plus `done` (idle + `@agent_attention`, H1); at least one is required. Semantics
  MUST be level-triggered and cycle-authoritative — a tier-2 poll over the shared
  detection cycle (immediate first tick, then ~1 s ticks with config + manifest
  hot-reload), returning as soon as a cycle *observes* the target, never from a raw stamp
  read. Exit codes are the interface: `0` observed (the row on stdout, tab-separated like
  `ls`, or `--json` for the schema-1 object with the `ls --json` row keys), `124` timeout
  (`--timeout <secs>`; absent waits forever, composing with `timeout(1)`), `3` the
  targeted pane vanished while waiting (distinct from timeout; a `--pane`, or a pinned
  `--agent` — since H18b `--agent` pins to the first pane it observes and then behaves as
  `--pane`, including vanish; only `--any` keeps waiting on a vanish), `2` usage error, `1`
  generic (an `--agent` ambiguous at its FIRST observation, server gone). A daemon-assisted
  push path is deferred. *(Shipped H10, commit 89806a8, 2026-07-25; `--agent` pin H18b.)*

### Configuration and tooling

- **F23** Config file (`~/.config/tma/config.toml`): poll interval, hysteresis, glyphs/
  colors, notification hook, agent enable/disable, custom process-name mappings. All
  settings MUST have working defaults; zero-config MUST work. `[notify]` keys:
  `command` (the F22 hook command), `from_event` (daemonless direct-fire opt-in), and
  `on` (H2) — the array of transitions that notify, `"blocked"` and/or `"done"`,
  default `["blocked"]` (F22 unchanged); `on = ["blocked", "done"]` also fires when
  an agent finishes a turn (working→idle). An unknown `on` value is a loud config
  error, like any mistyped key (never silently ignored).
- **F24** Manifest authoring tools MUST ship in the binary: `tma debug capture <pane>`
  (print exactly what the detector saw: viewport, title, flags) and
  `tma debug explain <pane> [--json]` (which rules matched/failed and the verdict).
  These are how first-party evidence is collected (D10) and how users debug their own
  manifests.
- **F25** User manifest overrides at `~/.config/tma/agents/<agent>.toml` MUST shadow
  bundled manifests, reloadable without a daemon restart. *(Shipped H3, widened to
  config + manifests: the daemon reloads on SIGHUP / `tma reload` and the picker on
  its refresh tick — by re-reading on the reload signal / tick, not by mtime watching.
  One-shots reload per invocation.)*

## 2. Detection quality requirements

- **D1** Error asymmetry is the design driver: a blocked agent shown as idle/working is
  the worst failure (user never comes back); a working agent briefly shown as idle is
  acceptable. When evidence is ambiguous between blocked and anything else, prefer
  blocked... but see D2.
- **D2** Blocked MUST be asserted only from direct evidence: a blocked-class agent hook
  event (F26) or live-viewport chrome (visible prompt/menu/question). Never inferred
  from silence or from title/activity alone. False blocked alarms train users to
  ignore the signal.
- **D3** Latency targets: state change visible on surfaces within 2 poll cycles;
  blocked notification within 5 s of the prompt appearing (daemon at default interval).
- **D4** Manifest rules MUST tolerate rendering variance: pane-width line wrapping
  (verified: chrome lines wrap in narrow panes), truncated/regenerated titles, themed
  colors (match text, use `-e` styling only as auxiliary evidence), and unicode
  spinner/glyph ranges rather than single codepoints.
- **D5** Rules MUST anchor on invariant control text (key hints, prompt markers,
  question forms), not incidental transcript content. A rule that could match
  user-generated text in the conversation body is a defect.
- **D6** Each agent manifest MUST cover, minimum: idle prompt, working, permission/
  confirmation prompt (blocked), and its known history-view screens (F10). Unknown is
  the mandated fallback, never a guess.
- **D7** The detection core MUST be a pure function of a snapshot struct (no tmux
  calls, no I/O, no clock) so the full manifest corpus is testable offline.
- **D8** Every bundled manifest rule MUST have at least one fixture test derived from a
  real capture; regressions on agent UI updates are caught by re-capturing, not by hand-
  editing fixtures.
- **D9** Fixtures MUST pass through a redaction step before entering the repo (captures
  contain real conversation text, paths, and potentially secrets — see N8).
- **D10** Evidence-first authoring is process, enforced by review: drive the real agent
  into the target state, record via `tma debug capture`, derive the rule from what is
  invariant, keep the redacted capture as the fixture. No rules written from memory or
  screenshots of other tools.
- **D11** Version the manifest schema (`min_engine_version` in each manifest); the
  engine MUST reject newer-schema manifests with a clear error rather than misparse.
- **D12** Hook mappings are evidence-authored like manifests (D10 extended): verify
  each agent's hook names, payload fields, and firing conditions against the real
  agent before shipping a mapping; fixture-test payload parsing with captured payloads
  (D8 analog). Verified so far: Claude Code's `Notification` hook distinguishes
  permission prompts via matcher (`permission_prompt|elicitation_dialog`, observed in
  tmux-agent's setup).
- **D13** A single registration table per agent MUST be the sole source of truth for
  hook wiring (event names, wrapper commands), with drift tests asserting the installed
  hook config, the parser arms, and the docs never diverge (pattern proven in
  tmux-agent-sidebar's `plugin_hooks_tests.rs`).
- **D14** Each agent manifest MUST declare which states its hooks can report
  (`[hooks].covers`) AND which states its screen rules reliably detect
  (`[capture].visible`, evidence-backed per D10) — the second gate is what
  coverage-aware decay (F8) reads. An agent reporting turn-complete but not
  permission prompts still needs screen evidence for `blocked`.
- **D15** *(daemon tier only — round-3 scoping: edge counting needs persistent
  memory that daemonless producers are forbidden by F12's own constraint, and a
  tmux-option counter would be the read-modify-write class AD4 outlaws)*
  Hook-capable panes MUST be subject to hook-liveness demotion: a pane registered
  hook-capable that emits zero hook events across N daemon-observed activity edges
  (default 5) has its hook coverage treated as suspect — the daemon then writes
  subsequent capture verdicts unguarded (its writes bypass the F13a source guard
  for that pane until hook events resume). Edges observed while the stored claim
  is still hook-fresh (source `hook` with `evidence_at` inside the F8 decay
  window) do not count: a live hook claim is direct evidence the wiring works, so
  a single long tool call's output pauses cannot demote a healthy pane. Demotion
  for genuinely dead wiring is therefore bounded by the decay window plus N
  non-corroborating edges. Daemonless tiers rely on
  `install-hooks --check` for broken-wiring detection. Rationale (round-2): hook
  wiring dies
  silently by design (F28 wrapper exits 0 when the binary is missing; users
  reinstall agent configs), and without demotion a broken-wiring pane holds wrong
  state indefinitely while the ownership rules discount the only evidence that
  could fix it. The daemon sweep SHOULD additionally run `install-hooks --check`
  logic and surface broken wiring to the user.

## 3. Non-functional requirements

### Performance

- **N1** Poll-cycle budget: 1 `list-panes` + 1 `ps` + ≤1 `capture-pane` per *agent*
  pane + option stamping. Skip capture when the pane's hash inputs (`window_activity`,
  title) are unchanged and state is settled. Target: cycle wall time <100 ms with 10
  agents / 40 panes; verify empirically before phase 2. Measured on a scratch
  server at that size: cold sweep ~104 ms median, a marginal ~4% overshoot; warm
  consumer cycle ~24 ms. The overshoot is retired for the daemon steady state by
  its on-demand capture, which confines full 10-capture fan-out to the
  reconciliation sweep rather than every cycle.
- **N2** `tma status` MUST return within ~50 ms when reading fresh stamped state, and
  within ~150 ms cold (own mini-cycle). It runs every `status-interval` (reference
  config: 10 s) and must never make the status line lag.
- **N3** Picker MUST paint a first usable frame within ~100 ms of invocation (popup
  feel = instant); detection refinement may arrive on the next refresh tick.
- **N4** Daemon steady-state: <1% of one core, no unbounded memory (bounded transition
  history).

### Reliability and safety

- **N5** Read-only guarantee: `tma` MUST NOT write to any pane (no `send-keys`, no
  input injection) — the only pane-affecting actions are focus changes on explicit user
  jump. This is a trust boundary; future "answer the prompt from the picker" features
  require explicit per-invocation user action and are out of v1 scope.
- **N6** Daemon failure MUST be invisible to tmux: no orphaned popups, no stuck
  options misread as fresh (freshness marker + F16), auto-restart safe (single-instance
  lock).
- **N7** CLI one-shots interact with tmux via the `tmux` binary subprocess only. The
  daemon MAY hold a pool of long-lived control-mode (`tmux -C`) clients — one per
  session, since control-mode notifications are session-scoped (DAEMON.md) — with
  reconnect handling; if the pool ever reaches zero members while sessions survive,
  recovery is subprocess re-enumeration (`list-sessions`) from the sweep. Every tmux
  interaction MUST handle tmux-server-gone gracefully; server exit terminates the
  daemon.

### Privacy and security

- **N8** Captured pane content stays in memory: never written to disk, logged, or
  transmitted, except explicit `tma debug capture` output to stdout. No telemetry, no
  network I/O at all in v1.
- **N9** The notification hook (F22) receives metadata (agent, state, pane id, title),
  not captured screen content, by default.

### Compatibility and portability

- **N10** tmux floor: 3.2 minimum (display-popup `-E`; pane user options; capture `-e`).
  Reference environment is 3.6a (verified; PRD appendix's "≥3.3" is superseded by
  this requirement). Feature-gate anything newer than 3.2 at runtime via `tmux -V` /
  format probing, with graceful degrade. Floor-probe list so far: `set-option -pF`
  conditional writes (F13a — verified 3.6a; degrade is documented advisory writes),
  control-mode `refresh-client -B` subscription semantics (DAEMON.md). Probes MUST
  test *behavior*, not command success — round 2 showed `-B` subscribe succeeding
  while silently useless cross-session.
- **N11** macOS and Linux at parity in v1 (`ps` invocation must handle both procps and
  BSD variants). Windows out of scope. BSDs best-effort.
- **N12** Rust, matching the tms toolchain profile (ratatui/crossterm/nucleo/clap/
  serde+toml); no async runtime unless a measured need appears; detection core crate
  separate from the CLI/UI crate (D7).

## 4. Extensibility requirements

- **X1** Adding an agent's *detection* = one manifest file (identity, screen rules,
  hook-event mapping, coverage declaration) — zero core code. Adding `install-hooks`
  automation for it additionally requires a per-agent installer adapter in core
  (F29); third parties can skip the adapter by wiring their agent's config to the
  wrapper themselves. Documented as a contributor path, honestly split.
- **X2** `tma ls --json` schema is versioned (`"schema": 1`) and additive-only within a
  major version.
- **X3** The manifest format is original to this project and documented well enough for
  third parties to author manifests from `tma debug` output alone. License: permissive
  (MIT or Apache-2.0 — final pick open), viable because no AGPL material is included.
- **X4** Design MUST NOT preclude phase-2/3 features already sketched in the PRD:
  daemon stamping, transition history, spawn integration, remote/ssh awareness. In
  particular, keep pane identity keyed on `#{pane_id}` (stable) not indexes.

## 5. Out of scope (v1)

Multiplexing or PTY ownership of any kind; agent orchestration (spawn, prompt, wait);
answering prompts from tma surfaces (N5); remote agents over ssh/containers (F6);
agents outside tmux; Windows; non-English agent UIs; GUI/web surfaces.

## 6. Open questions

1. ~~Whether Claude Code's `Notification` hook distinguishes permission prompts from
   idle reminders~~ — resolved: it does, via hook matchers (D12).
2. Final name/binary (`tma` placeholder).
3. ~~License: MIT vs Apache-2.0 (X3). Lean Apache-2.0 for the patent grant; either
   works.~~ — resolved: `Cargo.toml` sets `license = "Apache-2.0"`.
4. ~~Daemon lifecycle default: auto-start on first `tma` invocation vs explicit
   opt-in.~~ — resolved: explicit opt-in, no autostart. Spawn-if-absent is
   `tma daemon --ensure` (`tma/src/daemon.rs` `ensure_running`); a bare `tma`
   invocation never starts a daemon.
5. ~~Exact hysteresis default and whether blocked bypasses dwell~~ — resolved in F12
   (round 2): asymmetric dwell, working→idle only, ~3 s default; blocked and
   idle→working publish immediately.
6. Whether `tma status` should ever trigger a full cycle or only ever read stamps +
   titles (N2 cold-path cost vs staleness). Round-2 empirical inputs: tmux caches
   `#()` jobs by command string server-wide (no multi-client stampede), and a
   44-pane full option read measures ~26 ms, so the cold mini-cycle fits N2.
7. State model shape: keep the 4-state core and express permission/question/error/
   rate-limit as a `detail` dimension, or widen the state enum (tmux-agent-sidebar
   uses 6 states, tmux-agent 7)? See ARCHITECTURE.md for the exploration; current
   lean: 4-state core + open detail vocabulary.

## Appendix A: verified environment evidence (2026-07-20, tmux 3.6a, macOS)

Live checks against a running fleet of 7 Claude Code panes:

- `#{pane_current_command}` reports `claude` for directly-launched agents.
- All agent panes show `#{alternate_on} = 1`.
- `#{pane_title}` carries agent-published OSC titles including state and task summary:
  `✳ Update resume with …` (idle), `⠐ Understand coding agents state detection`
  (working — braille spinner, U+2800 block).
- `capture-pane -p` against a background, alt-screen agent pane returns the live
  viewport: prompt marker `❯` inside a bordered prompt box, horizontal-rule separators,
  status chrome line (`⏵⏵ bypass permissions on (shift+tab to cycle)`), completion line
  (`✻ Sautéed for 5m 34s`). Chrome text wraps at narrow widths (drives D4).
- Pane user options round-trip: `set-option -p @x v` / `#{@x}` / `-u` all work.
