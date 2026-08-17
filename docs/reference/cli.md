# Command-line interface

Every `tma` subcommand, its flags, and its exit codes. Transcribed from the
binary's own `--help`; run `tma <command> --help` for the same text at any time.

```
tmux-agents CLI: agent state monitor, picker, jump, and stamping for tmux.

Usage: tma [OPTIONS] [COMMAND]
```

Running `tma` with no subcommand opens the fuzzy picker. Its rows carry a dimmed
branch label (after the time column) when a listed pane resolves a git branch;
the picker itself stays a flat list, ungrouped. The pane you opened the picker
from is left out of the list (jumping to where you already are does nothing), so
opening it from your only agent shows an empty list; `ls`, `status`, and `watch`
still list every agent. Enter jumps to the highlighted
agent; `tab` opens that agent's [action menu](#tma-act) instead of jumping. Every
printable key belongs to the query — no letter or digit is reserved for a
shortcut, so an agent named `auth` is searchable from an empty prompt. A popup at
least 76 columns wide carries
a live preview of the highlighted pane beside the list, the same threshold `tma
watch` uses; below it the list takes the whole popup and nothing is captured. The
[key tables](keybindings.md#keys-inside-the-picker) list both
surfaces in full.

## Global options

These are accepted before or after any subcommand and are read from one
canonical field, so `tma --socket-name X ls` and `tma ls --socket-name X` target
the same server.

Whichever way you name the server, `tma` forwards the same flag to every child it
spawns — the daemon it launches, a detached action's supervisor, the `tma act`
entries in a `display-menu` — so nothing it starts lands on a different server
than you did. For the socket flags an explicit flag always wins over
`TMA_SOCKET_PATH`, which is consulted only when neither was given (the same
precedence `--config` has over `TMA_CONFIG`).

| option | value | meaning |
|---|---|---|
| `--manifest-dir` | `<DIR>` | Load manifests only from this directory (test isolation). |
| `--socket-name` | `<NAME>` | Target a specific tmux server socket by name (`tmux -L <name>`). |
| `--socket-path` | `<PATH>` | Target a tmux server by socket path (`tmux -S <path>`), the form tmate and a hand-placed socket need (env `TMA_SOCKET_PATH`). Mutually exclusive with `--socket-name`: passing both is a usage error (exit 2). |
| `--config` | `<PATH>` | Load config from this path instead of `~/.config/tma/config.toml` (env `TMA_CONFIG`). An absent file is the zero-config floor (all defaults). |
| `-c`, `--client` | `<NAME>` | The invoking tmux client for the picker/jump/watch Enter-jump. The `run-shell` jump bindings pass `--client "#{client_name}"` so the correct client is switched; absent, empty, or a still-unexpanded format (a binding context that does not expand, such as `display-popup`) falls back to targetless best-effort. |
| `--debug-timing` | | Print cycle timing and producer/consumer/capture counts to stderr, including `capture-skipped` (panes that reused their stamp because their window produced no output since it). Only the poll surfaces (`ls`/`status`/`jump`) act on it. |
| `-h`, `--help` | | Print help. |
| `-V`, `--version` | | Print version. |

The tmux binary itself is not a flag: it comes from `TMA_TMUX_BIN`, then
`[tmux] bin` in config, then plain `tmux`. That is what points tma at a tmate
socket or a second tmux build, whose servers refuse a mismatched client; see
[`[tmux]`](configuration.md#tmux-which-tmux-binary-to-spawn).

## Selector flags

`ls`, `status`, `jump`, `wait`, `act`, `subscribe`, and `watch` share one
vocabulary for saying *which* agents they are about. The flags are per-command
(they sit after the subcommand), and they mean the same thing everywhere.

| option | value | meaning |
|---|---|---|
| `--session` | `<NAME>` | Only agents in this tmux session. |
| `--repo` | `<NAME>` | Only agents whose pane resolves to this git repo. |
| `--branch` | `<NAME>` | Only agents on this branch (the literal `HEAD` when detached). |
| `--agent` | `<NAME>` | Only agents with this manifest name (e.g. `claude`). |
| `--state` | `<STATES>` | Only agents in one of these states, comma-separated. |

Matching is exact string equality — no globbing, no case folding. Different
flags AND together (`--repo app --state blocked` is blocked agents in `app`);
`--state`'s comma-separated tokens OR within themselves. No flags means every
agent, exactly as before.

`--state` takes the same tokens as `wait --until`: `idle`, `working`, `blocked`,
`unknown`, and the pseudo-state `done` (idle plus attention: finished with output
nobody has reviewed). `done` is the narrower half of `idle` — `--state idle`
matches a done pane too, `--state done` does not match a plain idle one. An
unknown token is a usage error (exit 2) naming the valid set.

`--repo` matches the repo label the surfaces render, which is the origin repo's
name, so it selects a repo's linked worktrees along with its main checkout;
`--branch` is what splits them apart. A pane whose cwd resolves to no git repo
matches neither flag (an unresolved repo is not a wildcard). Resolving those
labels costs one memoized `git` call per unique directory, so `status` runs it
only when `--repo`/`--branch` is actually present.

**Filtering is display-only, and happens after the cycle.** Every invocation
still runs the full poll cycle and stamps every agent pane on the server; the
selector narrows only what that invocation prints, counts, emits, jumps to, or
(for `act`) acts on. A `#(tma status --session X)` driver refreshes the panes in
your other sessions exactly as an unscoped one does.

## Commands

| command | summary |
|---|---|
| `version` | Print version and build information. |
| `ls` | List agent panes, one tab-separated line each (`--json` for the versioned schema). |
| `status` | Print the status-line one-liner: state counts with glyphs and `#[fg=]` styling. |
| `jump` | Jump focus to an agent pane across sessions (`--attention` / `--blocked` / `--next` / `--back` / `--home` / `--pane`), or menu them (`--menu`). |
| `wait` | Block until the target reaches one of `--until`'s states, then print the matched row(s). One pane, or a fleet (`--all` / `--count`). |
| `act` | Fire a guarded action into an agent pane (`--all` for every pane in scope), or enumerate/menu the fireable ones (`--list` / `--menu`). |
| `mute` | Suppress notifications for the panes in scope, for `--for <DURATION>` or until `--clear`. |
| `subscribe` | Stream the read path: one complete `ls --json` document per line, pushed when a daemon is present. |
| `watch` | Persistent live dashboard for a pane, window, or terminal of its own. |
| `daemon` | Run the event-hub daemon in the foreground; `--ensure` spawns it if absent then exits. |
| `reload` | Signal the running daemon to hot-reload its config and manifests (SIGHUP). |
| `init` | First-run setup: detect your installed agents and wire their hooks, install the keybindings, print the `status-right` line, then report with `doctor`. |
| `install-hooks` | Install, uninstall, or verify the agent and tmux hook wiring. |
| `install-keys` | Install, uninstall, or verify tma's tmux keybindings. |
| `doctor` | Diagnose each agent pane's effective tier and why. |
| `debug` | Manifest-authoring and inspection tools. |
| `event` | Internal, unstable: bridge one agent hook event to a stamp. |
| `clear-attention` | Internal: clear a pane's attention flag and nudge any resident `tma watch`; invoked by the auto-installed tmux focus hooks. |
| `supervise` | Internal: the detached-action supervisor. Spawned by the `act` broker's detach path to hold the single-flight lock for the child's lifetime, kill it at the deadline, then clear the lock and fire the completion notification. Never user-invoked. |

`event` is invoked only through the `tma-hook` wrapper an agent's config
references, never by hand. `debug stamp` is likewise internal and unstable.

`tma event` authenticates nothing and is not meant to; the security boundary is
your user account, spelled out in [The security
model](../explanation/security-model.md).

## `tma ls`

List agent panes.

```
Usage: tma ls [OPTIONS]
```

| option | meaning |
|---|---|
| `--json` | Emit JSON (`"schema": 1`) instead of tab-separated lines. |
| `--pane <ID>` | List only this pane id (e.g. `%5`), the single-row form. |
| [selector flags](#selector-flags) | Narrow the listed rows. |

`--pane` and the selector narrow the same way: no matching agent prints nothing
and exits `0`. A filtered `--json` is the same document with a shorter `agents`
array, so a consumer parses it identically.

Plain output is one tab-separated line per agent pane, in this column order:
`pane`, `agent`, `state`, `detail`, `since`, `session:window.pane`, `title`,
`attention`, `muted`. The `attention` column is `1` when the pane still carries
`@agent_attention` (finished or blocked output unreviewed), empty otherwise; the
trailing `muted` column is the same marker for a pane whose
[`tma mute`](#tma-mute) window has not expired. The
JSON schema is documented in [Pane options and JSON contracts](pane-options-and-json.md),
where the `--json` rows carry additive `repo`/`branch`/`worktree` labels (`null`
for a non-git pane); the plain output omits them.

## `tma status`

Print the status-line one-liner: state counts with glyphs and tmux `#[fg=]`
styling. As `#(tma status)` in `status-right` it is the required ambient
driver: each `status-interval` run refreshes the stamped pane options and
renders the counts.

```
Usage: tma status [OPTIONS]
```

| option | meaning |
|---|---|
| `--format <FORMAT>` | Output form: `tmux` (default), `plain`, `json`, or `prom`. |
| [selector flags](#selector-flags) | Count only the agents in scope. |

Output is the fixed order `blocked working done idle unknown`, zero-count classes
omitted, empty when there are no agents. Glyphs and colors come from `[status]`
config.

The `tmux` form also wraps each class in `#[range=user|tma:<class>]…#[norange]`,
which tmux honors on a `#()` job's output and which is what makes the counts
clickable ([Clickable status
segments](../how-to/install-the-keybindings.md#clickable-status-segments)). The
markers draw nothing and do nothing on their own; the other three formats carry
no markup at all.

The counts are over the selected rows, which is what makes a per-session status
line possible: `#(tma status --session #{session_name})`. See
[Show agents in your status line](../how-to/show-agents-in-your-status-line.md#scope-it-to-one-session)
for the caveats before you wire one.

### `--format`

One set of counts, four renderings. Every form runs the same cycle over the same
selected rows, so which one you poll never changes what gets stamped: an external
bar polling `--format plain` is as much an ambient driver as `#(tma status)` is.

| format | output |
|---|---|
| `tmux` | The default status-line one-liner, glyphs with `#[fg=]` styling plus the clickable-range markers. No trailing newline. |
| `plain` | The same glyphs and counts with the color codes dropped, for a bar that applies its own styling. No trailing newline. |
| `json` | `{"schema":1,"counts":{"working":N,"blocked":N,"idle":N,"unknown":N,"done":N}}`, one line. |
| `prom` | Prometheus text exposition, for a node_exporter textfile collector. |

`plain` honors the configured `[status]` glyphs; only the colors go away. Both
one-liners omit zero-count classes and print nothing at all when there are no
agents.

`json` is the opposite: every class is present even at zero, so a consumer never
branches on a missing key. **`done` and `idle` are disjoint counts** — an idle
pane with unreviewed output is counted under `done` and not under `idle`, so the
five always sum to the number of panes in scope. That is the split the rendered
line has always shown, and it is deliberately not the same as the `done` key on a
JSON *row* (see [`tma ls`](#tma-ls)), which is a subset of `state: "idle"`
because the row keeps its stored token.

`prom` emits two gauge families, each with its own `HELP`/`TYPE` comments:

```
# HELP tma_agents Agent panes in each state class. The classes are disjoint: ...
# TYPE tma_agents gauge
tma_agents{state="working"} 2
tma_agents{state="blocked"} 1
tma_agents{state="idle"} 0
tma_agents{state="unknown"} 0
tma_agents{state="done"} 1
# HELP tma_agent_state_seconds Seconds the pane has held its current state ...
# TYPE tma_agent_state_seconds gauge
tma_agent_state_seconds{pane="%5",agent="claude",state="blocked"} 42.000
```

`tma_agents` carries all five classes even at zero, so a series never disappears
mid-scrape. `tma_agent_state_seconds` is one series per agent pane, from the row's
`since` against now; a pane whose transition was never stamped reports `0`. Its
`state` label uses the same disjoint classes, so summing the per-pane series by
state reproduces `tma_agents` exactly. The textfile-collector recipe is in
[Drive an external bar](../how-to/drive-an-external-bar.md#export-to-prometheus).

## `tma jump`

Jump focus to an agent pane across sessions. At most one direction flag is used;
`--next` is the default when none is given.

```
Usage: tma jump [OPTIONS]
```

| option | meaning |
|---|---|
| `--attention` | Jump to the next agent that wants you: blocked first (longest-blocked first), then finished-unreviewed (idle with attention). Advances from the current pane and wraps. |
| `--blocked` | Jump to the longest-blocked agent. |
| `--next` | Jump to the next agent after the current pane (session, then window, then pane order). |
| `--back` | Return one step along the trail (the previous jump's origin). |
| `--home` | Return to the oldest recorded origin (the bottom of the trail) and clear the trail. |
| `--pane <ID>` | Jump to this pane id. Records the origin like any forward jump and clears the pane's attention flag; ignores the selector (the target is already named). A pane with no agent on it is a note on stderr and exit 0. |
| `--menu` | Render a tmux `display-menu` of every agent (the pane you invoked it from excluded), each entry firing `jump --pane` on that agent. Needs an attached client; an empty list prints "no agents" and exits 0. |
| [selector flags](#selector-flags) | Scope the candidates a forward jump may land on. |

The selector scopes triage: `tma jump --attention --repo app` walks only that
repo's waiting agents, and reports "no agents waiting for you in scope" when it
finds none. `--back`/`--home` replay the return trail and ignore it.

A forward jump (`--attention`/`--blocked`/`--next`) pushes the current location
onto a per-client return trail. `--attention` and `--blocked` also clear the
destination pane's attention flag (focusing a waiting agent reviews it); `--next`
is plain positional cycling and leaves attention untouched. `--back` pops one
entry; `--home` returns to the trail's bottom entry and empties it. The trail is a
bounded stack (cap 8, oldest dropped past the cap) held in a per-client server
option, so `--back`/`--home` are independent per client. When the trail is empty
they print a note and exit 0.

Pass `--client "#{client_name}"` (a global option) from a `run-shell` binding,
which format-expands it, so the jump switches the client that pressed the key and
keys its return trail by it.

`--menu` is the tmux-native counterpart of the picker: entries are ordered like
the picker's list (blocked, working, idle, then longest-in-state first), the first
nine carry a `1`-`9` quick-select digit, and each one runs `tma jump --pane <id>`
with the acting client and the invoking server resolved into the command. It is
what a right-click on a [clickable status
segment](../how-to/install-the-keybindings.md#clickable-status-segments) opens.

## `tma wait`

Block until the target reaches one of `--until`'s states, then print the matched
row(s) and exit. It is the scripting primitive: a tier-2 poll loop (immediate
first cycle, then roughly one-second ticks with config and manifest hot-reload),
level-triggered, so an already-in-state target returns immediately. A transient
tmux stall is ridden out as a skipped tick (a one-time stderr note flags it), not
a failure; a vanished server still ends the wait.

```
Usage: tma wait [OPTIONS] --until <STATES>
```

One target, either an explicit flag or the selector's `--agent`. The explicit
flags are mutually exclusive:

| option | meaning |
|---|---|
| `--pane <ID>` | Wait on this specific tmux pane id (e.g. `%5`). Its disappearance while waiting is exit 3. |
| `--agent <NAME>` | Wait on the agent pane with this name. Pins to the first in-scope pane observed, then behaves as `--pane` on it (a vanish is exit 3). Matching more than one in-scope pane at that first observation is an error suggesting `--pane`, never a silent first-match. |
| `--any` | Wait on any agent pane in scope; the first to reach a target state (in surface-sort order) wins. `--any` never pins and keeps waiting on a vanish. |
| `--all` | Barrier: succeed only when EVERY agent pane in scope is in a target state at once. |
| `--count <N>` | Quorum: succeed once at least N agent panes in scope are in a target state. |

Naming no target at all is a usage error (exit 2). `--agent` is a selector flag
that doubles as a target, so it combines with the other four: `--all --agent
claude` is a barrier over every Claude pane, and `--any --agent claude` is the
first Claude pane to land.

**Membership.** `--all` pins its membership at the first observation and
`--count` never pins. A barrier is over a fleet you already have: a pane that
launches mid-wait does not join it (and so cannot hold it open forever), while a
member whose pane dies ends the wait at exit 3, exactly as a `--pane` vanish
does. A quorum is over whoever shows up: it re-reads the scope every cycle, so a
pane appearing mid-wait counts toward N and one leaving is not an error. An
`--all` whose scope matches no pane at that first observation is exit 2 — a
barrier over an empty fleet would be vacuous success. `--count` stays permissive:
it waits for N matches among however many panes appear, so a scope that cannot
yet reach N simply blocks until `--timeout`.

Other options:

| option | meaning |
|---|---|
| `--until <STATES>` | Required. The state(s) to wait for, comma-separated: `idle`, `working`, `blocked`, `unknown`, and `done` (idle plus attention, the finished-and-unreviewed surface). `wait` returns as soon as a cycle observes the target in any of them. |
| `--since <EPOCH_MS>` | Only a state that BEGAN after this epoch-ms timestamp satisfies (the row's `since_ms` must be strictly greater). Works with every target. |
| [selector flags](#selector-flags) | Scope `--agent`/`--any`/`--all`/`--count`. `--agent` is both the scope and the by-name target, so the flag that names an agent is the flag that selects it. Rejected alongside `--pane`, whose id is already unique (exit 2). |
| `--timeout <SECS>` | Give up after this many seconds and exit 124 (the `timeout(1)` convention). Absent waits forever; compose with `timeout(1)` for an external belt. |
| `--json` | Emit the matched row as one schema-1 JSON object (same keys as an `ls --json` row) instead of the tab-separated line. `--all`/`--count` emit the schema-1 [`agents` document](pane-options-and-json.md#tma-wait---json) of the satisfied set instead. |

`--since` is the escape hatch from level-triggering. A supervisor loop that waits
for `blocked`, acts, and loops would otherwise re-satisfy immediately on the same
episode, because the state it waited for is still the current one; passing the
`since_ms` of the row it just handled requires a NEW transition. The recipe is in
[Block a script on agent
state](../how-to/block-a-script-on-agent-state.md#drive-a-supervisor-loop).

`--pane` on a pane that exists but is not yet an agent blocks forever by design
(the agent may launch later); a one-time stderr hint flags a likely typo without
breaking scripts. Once a watched pane HAS been seen carrying an agent, the same
situation means the opposite thing: the agent process is gone while the pane
lives, so the wait ends at exit 4 naming the pane instead of blocking to a
timeout that could not tell a crashed agent from a slow one. That applies to
`--pane`, a pinned `--agent`, and `--all` members; `--any` and `--count` ignore a
departure and keep waiting for the others.

### Exit codes

| code | meaning |
|---|---|
| `0` | A target state was observed (the row on stdout; `--json` for one schema-1 object, or the `agents` document under `--all`/`--count`). |
| `124` | Timed out (`--timeout` elapsed); nothing on stdout. |
| `3` | A watched pane vanished while waiting: a `--pane`, a pinned `--agent`, or an `--all` member. `--any` and `--count` keep waiting for the others. |
| `4` | The agent died while its pane lived on: a watched pane that HAD an agent row lost it. Same targets as `3`; the message names the pane. |
| `2` | Usage error (bad `--until` token, no target named, an invalid target combination, or `--all` whose scope matched no pane). |
| `1` | A generic runtime failure (ambiguous `--agent` at first observation, or no tmux server). |

## `tma act`

Fire a guarded action into an agent pane, or enumerate the fireable ones. One
verb, three modes: fire `<name>`, `--list`, or `--menu`. Actions are declared in
manifests (see [Action manifest schema](action-manifest-schema.md)); the broker
re-verifies the target's state, holds a single-flight pane lock, then acts. To
author one, see [Author a custom action](../how-to/custom-actions.md).

```
Usage: tma act [OPTIONS] [NAME]
```

`[NAME]` is the action to fire; omit it with `--list` / `--menu`.

| option | meaning |
|---|---|
| `--pane <ID>` | Target this pane id (e.g. `%5`); defaults to the current pane inside tmux. Rejects the selector flags (exit 2), whose narrowing a pane id has already done. |
| [selector flags](#selector-flags) | Scope the target. Alone they must resolve to exactly one pane: none is exit 3, more than one is exit 1 naming the candidates (`--agent <NAME>` is the common form). With `--all` the whole selection is the target set. |
| `--all` | Fire on EVERY selector-matched pane, one after another. |
| `--dry-run` | Print the resolved targets and each one's gate verdict; execute nothing, acquire no lock. For a single target it also prints the resolved context (with each value's age) and the would-be keys or command. |
| `--arg <VALUE>` | Repeatable. Pass a value to an `exec` action's command as environment (`TMA_ARG`, `TMA_ARG_1..N`, `TMA_ARG_COUNT`); never interpolated into the command string. A `keys` action rejects it (exit 2). Under `--all` every target gets the same values. |
| `--force` | Skip the `when` gate only, never `requires` and never the lock. |
| `--yes` | Satisfy a `confirm` action non-interactively (a non-TTY without `--yes` refuses). Under `--all` it covers the whole batch. |
| `--json` | Emit schema-1 JSON: the fire result object (the `results` envelope under `--all`), or the `--list` document. |
| `--list` | Enumerate actions; with `--pane`, include each one's fireability verdict. |
| `--menu` | Render a tmux `display-menu` of the currently-fireable actions (the keyboard-only parity surface, wired by `tma install-keys`). |

The `--json` result object, the `--all` envelope, and the `--list` document are
specified in
[Pane options and JSON contracts](pane-options-and-json.md#tma-act-json-result).

### Fan-out (`--all`)

`--all` resolves its targets from one cycle, then runs the ordinary per-pane
broker sequence on each in turn: every target takes its own single-flight lock
and re-verifies its own gate at fire time, so a fan-out is N independent fires,
never a shortcut around the guards. One target's refusal does not abort the rest.

- A `confirm` action asks once for the batch, listing the panes, rather than once
  per pane; `--yes` satisfies the batch.
- `--json` emits the `results` envelope, and it does so even when one pane
  matched, so a script's parse does not depend on the match count.
- The exit code is the WORST target's, ranked: acted, `locked` (5), a gate
  refusal (4), `vanished` (3), `timeout` (124), a failed exec child (its own
  code), a broker error (1). A fan-out exits `0` only if every target acted.
- A selector that matches no pane is exit 2, not a silent no-op.

### Exit codes

| code | meaning |
|---|---|
| `0` | Acted: keys delivered, an API-channel answer delivered (2xx), a synchronous exec child exited `0`, or a detached supervisor spawned. |
| `124` | A synchronous exec child was killed at `timeout_ms`. |
| `4` | The gate refused: state did not satisfy `when`, `requires` was unmet (including an API `permission-reply` op with no pending request id or no resolvable endpoint), the action does not apply to this agent, or the gated metric has no coverage. The refusing fact goes to stderr. |
| `5` | The pane action lock is held by another invocation. |
| `3` | The act's target disappeared mid-act: tmux reports the pane gone (`can't find pane` / `no such pane`), or an API permission was answered/withdrawn between the gate and the act (a 404). |
| `2` | Usage error (bad flag combination, selector flags alongside `--pane`, or `--all` whose selector matched no pane). |
| `1` | A runtime failure (no tmux server, a broker error, or an ambiguous selection without `--all`). A tmux command the server refused lands here, with tmux's own stderr in the message — only a pane tmux reports as gone is exit `3`. |

Under `--all` the code is the worst target's on the ladder above (see
[Fan-out](#fan-out---all)).

The reserved band (`3`, `4`, `5`, `2`) is strictly pre-spawn broker verdicts. An
exec action that did spawn passes its child's own exit code through verbatim, so
a child code can land inside that band; scripted consumers that branch beyond
success/failure read the `--json` `outcome` field, which is authoritative.

## `tma mute`

Stop a pane from notifying, without changing anything tma detects about it.

```
Usage: tma mute [OPTIONS]
```

| option | meaning |
|---|---|
| `--pane <ID>` | Mute this pane id (e.g. `%5`); defaults to the current pane inside tmux. |
| `--for <DURATION>` | Stay muted this long. Without it the mute holds until `--clear`. |
| `--clear` | Lift the mute on the matched panes. |
| [selector flags](#selector-flags) | Mute every pane in scope. |

The duration grammar is an integer plus an optional unit: `s` seconds, `m`
minutes, `h` hours, `d` days, with a bare number read as seconds (`tma mute --for
90` is 90 seconds). Anything else is a usage error, as is `0` — a mute that is
over before it starts — and `--for` alongside `--clear`.

Targets resolve the way [`tma act`](#tma-act)'s do, minus the `--all` opt-in: a
selector mutes every pane it matches, because a mute is per-pane, idempotent, and
undone by one `--clear`. `--pane` and the selector flags are mutually exclusive.

What mute changes is the *fire*, nothing else. A muted pane is still detected,
still stamped, still counted by `tma status`, still `blocked` in `tma ls` and in
the JSON — it simply rings nothing: no `display-message`, no bell or OSC, no
`[notify] command`, for both the state triggers and `context_high`. The episode's
`@agent_notified_at` marker is written as usual, so a mute that expires mid-episode
does not then ring for a transition you already muted. A detached action's
completion notification is deliberately outside the mute: you asked for that one,
and it reports once.

The deadline lives in the pane option `@agent_mute_until` (see
[Pane options](pane-options-and-json.md)), which is what makes a mute survive a
`tma` restart, a daemon stop/start, and a config reload; `--json` rows carry the
resolved `muted` boolean.

| code | meaning |
|---|---|
| `0` | The option was written (or unset) on every target. |
| `3` | The selector matched no agent pane. |
| `2` | Usage error (bad `--for` value, `--for` with `--clear`, selector flags alongside `--pane`, or no target and not inside tmux). |
| `1` | A runtime failure (no tmux server, or a tmux command the server refused). |

## `tma subscribe`

Stream the read path. One long-running process emits one complete `ls --json`
schema-1 document per line (the same document [`tma ls --json`](#tma-ls)
prints), snapshot semantics with no diffs. It replaces a consumer's own polling
timer: a Stream Deck plugin or dashboard spawns `tma subscribe --json` and
re-renders on each line, holding a connection to nothing but the `tma` binary.
The recipes are in [Stream state changes](../how-to/stream-state-changes.md).

```
Usage: tma subscribe [OPTIONS]
```

| option | meaning |
|---|---|
| `--json` | Required. JSON is the only emission today; a missing `--json` is a usage error (exit 2). |
| `--interval <SECS>` | Poll cadence when no daemon is present, and the degrade cadence when one dies (default `1`). Push mode delivers on the daemon's edge, so this only bounds the daemonless path. Must be at least `1`. |
| `--changes-only` | Skip a poll-mode emission that would repeat the last document. |
| `--events` | Emit one edge record per state transition instead of snapshots. |
| [selector flags](#selector-flags) | Emit only the agents in scope. Each line stays a complete schema-1 document with a narrower `agents` array; the emission cadence and the push/poll contract are unchanged. |

### Push, poll, and what the stream promises

With a daemon running, `subscribe` rides its edge pushes (the same
wake-hint subscription `tma wait` uses): a state change wakes the stream, which
runs its own poll cycle and emits what that cycle observed, well under
`--interval`. Wake hints arriving within a 100 ms window coalesce into one
emission, and a slower belt cycle emits only when it observes a change, so a
quiet system emits nothing after the first snapshot. Every emitted document is
built from the subscriber's own cycle, never from the socket, so push and poll
output are identical — and so are `--changes-only` and `--events`, which diff
the same cycles either way.

Degrade is invisible except as latency: no daemon, a daemon dying mid-stream,
or a daemon too old to answer the subscribe frame all drop the stream to
unconditional `--interval` polling, and a periodic re-probe picks a returning
daemon back up. There is no heartbeat — process death is the liveness signal, so
a consumer that owns the process respawns it on EOF. The stream exits only on a
signal or when its stdout closes; it prints one JSON document per line to stdout
and nothing else there.

Four things the stream deliberately does not do:

- **No replay.** A subscriber sees what happens from the moment it starts. There
  is no backlog, no cursor, and no way to ask for what you missed while your
  consumer was restarting.
- **The first line is the current snapshot**, not an event: in the default mode
  it is the full document as of the entry cycle, and under `--events` there is no
  first line at all (see below).
- **Coalescing loses intermediate states.** Pushes inside the 100 ms window
  collapse into one cycle, so a pane that went `working` → `blocked` → `working`
  faster than that emits nothing at all. The stream is level-triggered on each
  cycle's observation, not a log of every instant.
- **The poll degrade is silent.** Nothing is printed to stderr and the stream does
  not exit; only latency changes. If you need to know which mode you are in, `tma
  doctor` reports whether a daemon is running.

### `--changes-only`

In poll mode the stream emits every `--interval` whether or not anything moved,
which is the pre-daemon self-poller contract: a consumer that just re-renders
does not care. A consumer that *appends* does — a daemonless logger writing to a
file gets 86,400 identical lines a day. `--changes-only` makes the poll tick
behave the way the push-mode belt already does: emit only when the document
differs from the last one sent.

It is a no-op in push mode (those wakes are already edges) and under `--events`
(edges are change-triggered by construction), accepted silently in both so a
script does not have to know which mode it landed in. The entry snapshot is
always emitted.

### `--events`

Instead of snapshots, emit one record per state transition, one JSON object per
line:

```json
{"schema":1,"at_ms":1700000000000,"pane":"%5","agent":"claude","from":"working","to":"blocked","detail":"permission","locator":"work:1.0","repo":"app","branch":"main"}
```

| key | meaning |
|---|---|
| `at_ms` | When the stream **observed** the transition (the diffing cycle's clock), not necessarily when the agent changed. Coalescing and the poll interval both sit between the two. |
| `from` / `to` | The state on each side, in the [selector vocabulary](#selector-flags): `idle`, `working`, `blocked`, `unknown`, `done`. |
| `detail`, `locator`, `repo`, `branch` | The same values the row carries after the transition (`null` where the row's are). |

The states are the **disjoint** reading: a finished-but-unreviewed pane is `done`,
not `idle`, so setting the attention flag on an idle pane is a real `idle` →
`done` edge and jumping to it (which clears attention) is `done` → `idle`.

Two edges have an open end, spelled as the empty string:

- A pane that **appeared** since the last cycle emits `"from": ""`.
- A pane that **vanished** emits `"to": ""`, carrying the fields from the last row
  seen.

The empty string, rather than `unknown`, is what makes those distinguishable: a
pane genuinely can be observed in `unknown` (it is there, its agent's state is
unreadable), and a consumer must be able to tell that from "there was no pane".

A pane whose state did not change emits nothing, even if its detail or title did —
this is a transition stream, not a change feed.

**There are no synthetic edges for the initial snapshot.** The first cycle
establishes the baseline silently; the first line you see is a real transition. A
consumer starting fresh has no prior state to reconcile, and inventing `""` →
`working` edges for panes that have been running for an hour would misdate them.
If you need the current state at startup, run `tma ls --json` once before (or
alongside) the stream.

With a selector, rows are filtered **before** the diff, so a pane leaving the
selection looks like a departure and one entering it looks like an appearance.
`tma subscribe --json --events --repo app` is a clean per-repo event feed as long
as you read it that way.

The jsonl logging recipe is in
[Stream state
changes](../how-to/stream-state-changes.md#log-every-transition-to-jsonl).

## `tma watch`

Persistent live dashboard for a normal pane, tmux window, or terminal of its own
(not a popup): `new-window "tma watch"`, or just `tma watch` in a spare terminal.
It shows the picker's rows in a live-updating list, refreshing every second and
on a focus-change nudge. Enter jumps the acting client to the highlighted agent
and clears its attention but keeps the dashboard open (non-modal); `q`, Esc, or
`ctrl-c` quit.

`a` opens the [action menu](#tma-act) for the highlighted agent — the same
`display-menu` `tma act --menu` renders, but aimed at the pane under the cursor
rather than the one you are standing in, so a row of blocked agents is answered
without jumping to each. The menu is a tmux overlay: the list keeps refreshing
behind it, and nothing opens when no action is fireable on that pane.

The body adapts to the pane width. Below 76 columns it is a single list. At or
above 76 it splits, with a live preview of the highlighted pane beside the list;
press `p` to swap that preview for a full-width status table (glyph, agent,
state with detail, context gauge, time-in-state, locator, title, and a model
column when any visible pane stamps `@agent_model`), and `p` again to swap back.
The chosen body is session-local (never persisted).

Both wide bodies group rows by repo (worktrees roll up under their origin's
name), each group under a dimmed `▸ repo-name` header, groups ordered so the
longest-blocked agent's group leads; every pane with no resolved repo folds into
one `▸ (no repo)` group. Grouping is the default; press `g` to flatten the list
to a flat state-sorted view and `g` again to regroup (session-local, like `p`).
Selection and Enter-jump target the agent under the cursor regardless of the
group headers. A dimmed branch label sits beside each row (table: a `branch`
column; single list: after the time column), present only when a visible pane
resolved one. The narrow single-list body stays flat but still shows the label.

```
Usage: tma watch [OPTIONS]
```

| option | meaning |
|---|---|
| `--table` | Open directly in the full-width status table when the pane is wide enough (`p` toggles back to the preview). A pane below 76 columns still falls back to the single list. |
| [selector flags](#selector-flags) | Show only the agents in scope, e.g. a `tma watch --repo app` window per repo. |

tma places nothing for you: run it where you want it. `prefix G` gives it a tmux
window of its own, a `split-window -h -l 40 'tma watch'` gives it a pane beside
your work, and a second terminal (or a second monitor) works just as well, since
`tma watch` reaches the server over the socket like any other client. Every
instance advertises its pid in `@tma_watch_pid` on its own pane, which is what
the focus-change nudge signals; several at once are fine.

A scoped watcher still runs the unscoped poll cycle every second, so it remains a
full ambient producer for every pane on the server. Its first frame is painted
from stamps, which carry no repo label yet, so a `--repo`/`--branch` watcher
starts empty and fills in on the first refresh.

The invoking client comes from the global `--client`.

## `tma daemon`

Run the event-hub daemon in the foreground. The daemon is strictly additive
(tier 3): never required.

```
Usage: tma daemon [OPTIONS]
```

| option | meaning |
|---|---|
| `--ensure` | Spawn a detached daemon if none is running for this server, then exit 0 (idempotent). |

## `tma reload`

Signal the running daemon to hot-reload its config and manifests (SIGHUP). It
prints a no-op message if none is running for this server; one-shot surfaces and
the picker reload on their own.

A reload is all-or-nothing: a config or manifest that does not parse leaves both
the running pair in place. Every surface that reloads names the failing file on
stderr, once per breakage rather than once per poll tick, so a mid-edit save is
quiet but a file left broken is not. A TUI (`tma watch`, the picker) holds its
line until the surface closes, so it cannot land on the alternate screen.

```
Usage: tma reload [OPTIONS]
```

Global options only.

## `tma init`

First-run setup. It runs the commands below in order rather than reimplementing
them, so every write still shows you its diff first and re-running changes
nothing:

1. **Detect.** Every bundled agent `install-hooks` can wire is looked for on your
   `PATH`, under the names its manifest gives (the manifest name plus its
   `process_names`, minus generic ones like `node`, which identify a runtime and
   not an agent). Found, not found, and "runs under a generic name, so tma cannot
   detect it" are all reported.
2. **Wire each agent found**, exactly as [`install-hooks <agent>`](#tma-install-hooks) does.
3. **Report the status line.** `tma` never edits `status-right`: it is your
   format string, in whichever config set it. init says whether it already runs
   `tma status`, and if not prints the line to add, the config file to add it to,
   and the reload command.
4. **Install the keybindings**, as [`install-keys`](#tma-install-keys) does. An
   install that is already current is skipped with a note.
5. **Start the daemon** with `--daemon` (what `tma daemon --ensure` does).
6. **Report** with [`doctor`](#tma-doctor), so you see the posture the steps
   above produced.

```
Usage: tma init [OPTIONS]
```

| option | meaning |
|---|---|
| `--yes` | Apply every step without the interactive diff confirmations (scripts, tests). |
| `--daemon` | Also start the event-hub daemon for this server. |
| `--config-dir <DIR>` | Override the tma config dir holding the managed `tmux.conf` and the per-server `hooks-state-<server>.toml` (env `TMA_CONFIG_DIR`). |
| `--conf <PATH>` | The tmux config to mark with the keybindings `source-file` line, and the file the status-line instructions name. Same default as `install-keys --conf`. |

The per-agent config paths are not flags here; they resolve through the same
`TMA_*` environment ladder [`install-hooks`](#tma-install-hooks) documents, so
`TMA_WRAPPER_PATH` is what a Nix install exports before running init.

Exit code 1 if a step failed or a confirmation was declined; the closing doctor
report is informational and never changes it. With no terminal behind stdin and
no `--yes` every confirmation declines, which init says up front.

## `tma install-hooks`

Install, uninstall, or verify the agent and tmux hook wiring.

```
Usage: tma install-hooks [OPTIONS] [AGENT]
```

`[AGENT]` is the agent whose config to wire (e.g. `claude`); it is optional only
with `--check`.

| option | meaning |
|---|---|
| `--uninstall` | Remove tma's hook wiring (symmetric to install). |
| `--check` | Verify hook wiring and report drift. Bare (`--check`) inspects every known agent; with an agent named, the drift report and exit code scope to that agent. The shared wrapper and tmux server hooks are always checked. |
| `--yes` | Apply without the interactive diff confirmation (scripts, tests). |
| `--settings <PATH>` | Override the agent settings path (env `TMA_CLAUDE_SETTINGS`). |
| `--gemini-settings <PATH>` | Override Gemini's `settings.json` path (env `TMA_GEMINI_SETTINGS`). Defaults to `~/.gemini/settings.json`. |
| `--config-dir <DIR>` | Override the tma config dir holding the per-server `hooks-state-<server>.toml` (env `TMA_CONFIG_DIR`). |
| `--wrapper-path <PATH>` | Override where the `tma-hook` wrapper is written (env `TMA_WRAPPER_PATH`). |
| `--opencode-plugin <PATH>` | Override where the OpenCode plugin is written (env `TMA_OPENCODE_PLUGIN`). |
| `--codex-config <PATH>` | Override Codex's `config.toml` path (env `TMA_CODEX_CONFIG`). Defaults to `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`. |
| `--codex-hooks <PATH>` | Override Codex's `hooks.json` path (env `TMA_CODEX_HOOKS`). Defaults to `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`. |
| `--cursor-hooks <PATH>` | Override Cursor's `hooks.json` path (env `TMA_CURSOR_HOOKS`). Defaults to `~/.cursor/hooks.json`. |
| `--cursor-cli-config <PATH>` | Override Cursor's `cli-config.json` path, which holds the `statusLine` context shim (env `TMA_CURSOR_CLI_CONFIG`). Defaults to `~/.cursor/cli-config.json`. |
| `--pi-extension <PATH>` | Override pi's extension file path (env `TMA_PI_EXTENSION`). Defaults to `$PI_CODING_AGENT_DIR/extensions/tma.js`, else `~/.pi/agent/extensions/tma.js`. |

Per-agent trust and wiring caveats (codex `/hooks` trust, gemini folder trust)
are in [Agent coverage](agent-coverage.md).

## `tma install-keys`

Install, uninstall, or verify tma's tmux keybindings. The bindings are written to
a managed file (`~/.config/tma/tmux.conf`, honoring `XDG_CONFIG_HOME`), and your
tmux config is given a single `source-file ... # tma keys` line. By default that line is
`source-file -q "$XDG_CONFIG_HOME/tma/tmux.conf" "$HOME/.config/tma/tmux.conf"`, which tmux
expands when it loads the config, so the same tmux config works on another machine (`-q`
skips the XDG path quietly when the variable is unset). Pinning the dir with `--config-dir`
or `TMA_CONFIG_DIR` writes that literal path instead, double-quoted so a space in it still
parses. Install is
idempotent and diff-before-write; uninstall removes the managed file and that one
marked line, and touches no other binding. Uninstall exits non-zero if it cannot
remove the `source-file` line (a declined confirmation or an unwritable config),
naming the line you are left to remove by hand.

```
Usage: tma install-keys [OPTIONS]
```

| option | meaning |
|---|---|
| `--uninstall` | Remove the managed file and the marked `source-file` line (symmetric to install). |
| `--check` | Verify the managed file is current and the resolved tmux config sources it exactly once; report drift. A file with or without either opt-in group counts as current. |
| `--mouse` | Also write the root-table bindings that make the status-line counts clickable. With `--check`, require them instead of accepting either file. |
| `--daemon` | Also write a `run-shell` line that starts the event-hub daemon for every tmux server that loads the file. With `--check`, require it. |
| `--yes` | Apply without the interactive diff confirmation (scripts, tests). |
| `--conf <PATH>` | The tmux config to mark with the `source-file` line. Defaults to the first tmux config that exists, in tmux's own load order: `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, `~/.config/tmux/tmux.conf`. With none of them present, tma creates `$XDG_CONFIG_HOME/tmux/tmux.conf` (or `~/.config/tmux/tmux.conf` when `~/.config` exists, else `~/.tmux.conf`); it only ever creates a config when you have none, so the new file cannot shadow one. |
| `--config-dir <DIR>` | Override the tma config dir holding the managed `tmux.conf` (env `TMA_CONFIG_DIR`). Defaults to `~/.config/tma`. |

The default bindings are prefix-key bindings: `a` opens the picker in a popup,
`G` opens `tma watch --table` in a new window (the full-width status table; `g` is
taken by `jump --blocked`), `A` opens `tma act --menu` on the active pane, and
`j`/`g`/`b`/`h` run `tma jump` with
`--attention`/`--blocked`/`--back`/`--home`. The status-line driver `#(tma status)`
is not written; add it to `status-right` yourself. See
[Install the keybindings](../how-to/install-the-keybindings.md) and the full
[key tables](keybindings.md).

`--mouse` adds four root-table bindings that dispatch on `#{mouse_status_range}`.
A left-click walks a three-arm chain, first match wins: the blocked count jumps to
the longest-blocked agent, any other `tma:*` range opens the picker popup, and
anything else falls
through to tmux's own `switch-client -t=`. A right-click on any tma range opens
`tma jump --menu`. They need `set -g mouse on`, which tma never sets (it changes
copy/paste in every pane), and they claim tmux's status-line mouse keys: a
left-click elsewhere still switches window, a right-click on a window name no
longer opens tmux's window menu (`Alt`-right-click still does). `tma doctor` warns
when the bindings are installed but `mouse` is off. Full write-up in [Clickable
status segments](../how-to/install-the-keybindings.md#clickable-status-segments).

`--daemon` appends one line:

```
run-shell -b 'tma --socket-path "#{socket_path}" daemon --ensure >/dev/null 2>&1'
```

The managed file is sourced when a tmux server loads its config, so this fires
once per server start. `run-shell` expands `#{socket_path}` to the socket of the
server doing the loading, so `tmux -L work` starts a daemon for itself rather
than for the default server a bare `tma daemon --ensure` would resolve. Nothing
accumulates on a re-source: `--ensure` takes a single-instance lock and exits 0
when a daemon already holds it. The daemon exits on its own when its tmux server
does, so there is no matching stop line. Without this flag the daemon starts
only when you run `tma daemon --ensure`, `tma init --daemon`, or set
`[daemon] autostart = true`; see [Run the daemon](../how-to/run-the-daemon.md).

## `tma doctor`

Diagnose each agent pane's effective tier (3 daemon, 2 hooks, 1 polling) and
why: hooks wired, daemon alive, last evidence source and age, and the
ambient-driver check. Read-only.

```
Usage: tma doctor [OPTIONS]
```

| option | meaning |
|---|---|
| `--json` | Emit JSON (`"schema": 1`) instead of the human-readable report. |
| `--exit-code` | Exit 1 when the report carries a warning or a pane is below the tier its manifest supports. Without it doctor is a report: exit 0 unless the config fails to load or the server is unreachable. |

Beyond the per-pane tier, doctor reports the conditions that quietly disable a
tier:

| check | what it means |
|---|---|
| tmux version | The server's own `#{version}` against the 3.6 floor tma is tested on. Older servers load configs in a different order and expand `display-popup` differently, so a keybinding or the picker can misbehave for reasons nothing else in the report explains. A warning line only: it never counts toward `--exit-code`, and a version string tma cannot parse produces no warning at all. |
| attached clients | A `#()` status job only runs while a client draws the status line, so a server with none has no ambient polling floor. Reported as a warning only when no daemon is covering for it. |
| global `status` | With `status` off, the `#(tma status)` driver never runs and `display-message` notifications are invisible. |
| clickable segments | The `install-keys --mouse` bindings are installed but the server's `mouse` option is off, so no click can reach them. |
| tmux hooks | Per hook: present, stale (it runs a different command than this build installs, e.g. a moved binary), wiped (recorded but gone server-wide — a restart), or missing. |
| `process_names` truncation | A manifest entry longer than the 15 characters both macOS libproc and the Linux kernel truncate `comm` to, with no truncated spelling beside it, can never match a pane. |
| hook demotion | A pane that registered through a hook (`@agent_session` stamped) whose current evidence came from capture: its hooks have stopped firing. |
| manifests and actions | Files the loader skipped, and actions naming an unknown agent. |
| remote panes | A pane whose foreground is a remote shell (ssh, mosh, docker, podman, kubectl). Neither the process walk nor a capture crosses that boundary, so an agent behind it reports only if its hooks can reach this tmux socket ([Run an agent in a container](../how-to/agents-in-containers.md)). Any `@agent_*` options such a pane still carries are held, not refreshed. Reported, not warned about: running an agent elsewhere is a choice, not a misconfiguration. |
| unreadable stamps | A pane carrying an `@agent_*` option that does not decode. Every read path treats a corrupt stamp as no stamp, so the pane reads as never-stamped with nothing else to say why; doctor names the option and the value. `tma debug explain` prints the same fact for one pane. |

Reading the report section by section, and the `--exit-code` CI recipe, are in
[Diagnose with `tma doctor`](../how-to/diagnose-with-doctor.md).

## `tma debug`

Manifest-authoring and inspection tools.

```
Usage: tma debug [OPTIONS] <COMMAND>
```

| subcommand | summary |
|---|---|
| `redact` | Redact a capture (paths, emails, and `--pattern` regexes) to stdout, preserving layout width, so it can be committed as a fixture. |
| `capture` | Print exactly what the detector saw for a pane, in fixture format. |
| `explain` | Run identity, the rule engine, and fold for a pane; print evidence, matched and failed rules, and the verdict. `--json` emits the versioned schema. |
| `transitions` | Print the running daemon's recent state transitions (its in-memory ring). `--json` emits the versioned schema. |
| `notify-test` | Fire the notify command a trigger resolves to against a representative payload. `--trigger blocked\|done\|context_high` (default `blocked`). |
| `stamp` | Internal, unstable: apply a guarded stamp to a pane, for testing the pane-option write guards directly. Not a public interface. |

### `tma debug transitions`

Reads the daemon's bounded ring of recent state transitions over its socket:

```
$ tma debug transitions
transitions (3 held, cap 256, 12 recorded over the daemon's life):
  %1     -        -> working  at=1700000000000 src=hook
  %1     working  -> blocked  at=1700000001500 src=hook
```

Oldest first, `-` for a pane's first observation. `--json` emits
`{"schema":1,"cap":...,"recorded":...,"transitions":[...]}` with `from` as an
explicit `null`.

The ring is daemon memory: it needs a running daemon (the command says so and
exits non-zero otherwise) and starts empty after a restart. A daemon older than
this build rejects the request and the command says to restart it — a reload
cannot add a protocol verb. For a durable per-notification record, use
[`[notify] log`](configuration.md#notify-notifications).

### `tma debug notify-test`

A real notification is fire-and-forget with the command's output discarded, which
makes a broken hook silent. This subcommand runs the same command the same way,
except that it waits, shows stderr, and reports the exit status:

```
$ tma debug notify-test --trigger blocked
payload   {"schema":1,"agent":"claude","pane":"%0","state":"blocked",...}
command   ~/.local/bin/tma-notify
exit      0
```

It needs no tmux server and no agent: the payload is synthesized (with `repo` and
`branch` resolved from the current directory) so a hook sees the real shape. It
exits non-zero when the trigger resolves to no command or the command failed,
so it works as a check. The outcome updates the same record `tma doctor` reads,
so a passing run clears a stale failure report.

## `tma version`

Print version and build information (`tma <version>`).
