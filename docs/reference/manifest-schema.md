# Manifest schema

One TOML manifest is the complete description of an agent: how to recognize its
pane, how its hook events map to states, which states its screen rules detect,
and the screen rules themselves. Bundled agents ship as manifests in
`crates/tma-core/manifests/`; a user manifest in `~/.config/tma/agents/` adds a
new agent or shadows a bundled one by filename stem, with no code change.

State routing is normative and not manifest-overridable: a manifest maps its
agent's events and screens into the closed state vocabulary (`idle`, `working`,
`blocked`, `unknown`); it cannot invent or remap a state. `[details]` carries
token spellings only.

## Top level

| field | required | type | meaning |
|---|---|---|---|
| `min_engine_version` | yes | version string | The minimum engine version this manifest needs (e.g. `"0.1"`). Missing components default to zero. A manifest that needs a newer engine is rejected with an upgrade error, checked before the strict parse so a newer-schema field never surfaces as a confusing "unknown field". |
| `[identity]` | yes | table | How to recognize the pane. |
| `[hooks]` | no | table | Present marks the agent hook-capable. Absent is the screen-only floor. |
| `[capture]` | yes | table | Which states the screen rules reliably detect. |
| `[[rules]]` | no | array | Screen rules. |
| `[details]` | no | table | Detail-token alternate spellings. |
| `[telemetry]` | no | table | Metric channels the agent exposes. Absent means it exposes none. |

Unknown fields at any level are a parse error.

Manifests load per file. A user manifest that fails to parse (or whose rule
regexes fail to compile) is skipped and the rest of the set still loads, so a typo
in one file never costs you the bundled agents. The poll surfaces print one
`tma: skipping manifest <path>: <error>` line to stderr; `tma event` stays silent
(a hook must never speak); the daemon logs it and starts on what loaded; and
`tma doctor` lists every skipped file with its error under `agents:`. A *bundled*
manifest that fails is fatal — that is a build bug, not user input.

## `[identity]`

| field | required | type | meaning |
|---|---|---|---|
| `process_names` | yes | array of string | `#{pane_current_command}` values that cheaply flag a candidate agent pane. |
| `title_patterns` | no | array of string | Regexes over `#{pane_title}` that narrow a generic `process_names` match. When non-empty, a pane is this agent only when a `process_names` entry matches AND the current title matches one of these patterns (or the flicker-stickiness hold is active). Empty (the default) leaves identity as process match alone. Patterns compile at engine build; an invalid pattern is a build-time error naming the file. |

### Comm truncation: why a name may need two spellings

`process_names` is matched against two different sources, and they do not always
report the same string for the same process:

- **`ps -eo comm`**, the process-tree walk. This is what decides the pane holds an
  agent at all. tma takes the first whitespace-separated token and basenames it.
- **`#{pane_current_command}`**, the foreground check. This decides
  `foreground_is_agent`, and a false answer caps every screen verdict at
  `unknown` — the pane is still identified, but its capture evidence stops
  meaning anything.

**Keep every entry to 15 characters or fewer, and list both spellings when the
real name is longer.** Fifteen is where the truncation lands: the Linux kernel's
`comm` field is 16 bytes including the terminator, and macOS's libproc — which is
where tmux gets `#{pane_current_command}` — cuts at the same width. On Linux both
sources truncate, so one 15-character entry covers both. On macOS they diverge:
`ps` reports the invoked path (untruncated, and the symlink you typed), while
tmux reports the *resolved* binary's name, truncated.

The bundled codex manifest is the worked example, and the divergence is why it
carries two entries:

```toml
[identity]
process_names = ["codex", "codex-aarch64-a"]
```

Homebrew installs codex as `codex-aarch64-apple-darwin` behind a `codex` symlink.
Launch it and the two sources disagree, verified on macOS:

```
$ ps -eo pid,comm | grep codex
13071 /opt/homebrew/bin/codex          ← basenames to `codex`: identifies the pane

$ tmux display -p '#{pane_current_command}'
codex-aarch64-a                          ← 15 chars of the resolved binary
```

Drop the second entry and codex panes are still found (the walk matches `codex`)
but every screen rule is capped at `unknown`, because the foreground check
compares `codex-aarch64-a` against a list that has no such name.

`tma doctor` flags the trap directly — an entry longer than 15 characters with no
truncated sibling in the same list:

```
agents:  6 loaded, no issues
  - myagent: process_names entry "my-very-long-agent-binary" is longer than 15 chars, the width both
    macOS libproc and the Linux kernel truncate `comm` to, and no truncated spelling sits beside it —
    add "my-very-long-ag"
```

A long entry with its prefix already listed is not flagged: that is the codex
shape, and it is correct.

## `[hooks]`

Presence of this block marks the agent hook-capable.

| field | required | type | meaning |
|---|---|---|---|
| `covers` | no | array of token | Which states and lifecycle the agent's hooks report: any state token, plus the literal `lifecycle`. This is the first coverage gate. |
| `[[hooks.map]]` | no | array | Event-to-claim mappings. |

### `[[hooks.map]]`

| field | required | type | meaning |
|---|---|---|---|
| `event` | yes | string | Agent hook event name (e.g. `Notification`, `SessionStart`). |
| `matcher` | no | string | Optional payload matcher regex (e.g. `permission_prompt|elicitation_dialog`), applied over the raw payload. |
| `claim` | yes | table | The claim this event raises: either a state claim `{ state = "...", detail = "..." }` (detail optional) or a lifecycle claim `{ lifecycle = "start" }` / `{ lifecycle = "end" }`. |
| `turn_end` | no | bool | Whether this event MEANS a turn ended (`false` by default). Set it on the agent's turn-end event and nowhere else. It is a property of the EVENT, not of its claim: the same `state = "idle"` is raised by screen rules too, where nothing ended. tma raises the done marker on a turn end even when the pane was already idle, which is the only way a second completion is signalled after the user cleared the first marker; an event that merely observes idleness (an idle-reminder notification) must leave it `false`, or a cleared marker would come straight back. |

## `[capture]`

| field | required | type | meaning |
|---|---|---|---|
| `visible` | no | array of state | The states the agent's screen rules reliably detect, evidence-backed. This is the second coverage gate that the coverage-aware decay reads. |

## `[[rules]]`

One screen rule. Higher `priority` wins when multiple rules match.

| field | required | type | meaning |
|---|---|---|---|
| `state` | yes | state | The state this rule asserts on match. |
| `detail` | no | detail token | Detail to attach (e.g. `permission` for a permission prompt). |
| `priority` | no | integer | Higher wins on multiple matches. Default `0`. |
| `region` | yes | string | Where to look (see below). |
| `match` | yes | matcher | The text predicate (see below). |
| `skip_state_update` | no | bool | This screen shows history, not live state: freeze, do not restate. Default `false`. |

### `region`

| value | meaning |
|---|---|
| `tail_lines(N)` | Match against the last `N` lines of the captured tail. Bottom-anchored agents use a small window that always fits the visible screen, so this never reads scrollback for them. |
| `bottom_non_empty_lines(N)` | Match against the last `N` lines that end at the last line with content: trailing blank lines (blank after ANSI stripping) are discarded before the window is taken. Use this instead of `tail_lines(N)` for an agent that renders inline, where a session that has not yet filled the screen leaves blank rows below its chrome that would consume the whole window. |
| `visible` | Match against the visible screen only: the last `#{pane_height}` lines of the captured tail, before any further scoping. This removes scrollback lines for agents whose chrome floats in the transcript, so a whole-screen rule cannot match a prior turn's chrome out of scrollback on a short pane. When the height is unknown it degrades to the whole captured tail. |
| `title` | Match against the pane title. |

### `match`

A screen matcher composes leaf text predicates. TOML is externally tagged:

| form | meaning |
|---|---|
| `{ contains = "x" }` | Substring match. |
| `{ regex = "..." }` | Regex over the region. |
| `{ line_regex = "..." }` | Regex applied per line. |
| `{ any = [ ... ] }` | Any child matches. |
| `{ all = [ ... ] }` | All children match. |
| `{ not = { ... } }` | The child does not match. |

Regex strings are stored verbatim and compiled at match time.

## `[details]`

Maps a canonical detail token to its alternate spellings, so a screen or hook
that spells a detail differently still normalizes to the canonical token.

```toml
[details]
rate_limit = { aliases = ["ratelimited", "rate-limited"] }
```

Each key is the canonical token and `aliases` lists alternate spellings.

## `[telemetry]`

One optional sub-table per metric. Only `context` exists today; a second metric
would be an additive sibling rather than a rename.

| field | required | type | meaning |
|---|---|---|---|
| `[telemetry.context]` | no | table | How tma obtains this agent's context-window utilization percent. Absent means the agent has no gauge. |

### `[telemetry.context]`

| field | required | type | meaning |
|---|---|---|---|
| `channel` | yes | token | The transport shape: `event` (the agent pushes a payload to `tma event --kind context`), `file-tail` (tma reads a bounded, end-anchored slice of a file the agent writes), or `screen` (last-resort extraction). Any other value is a parse error naming the three. |
| `format` | yes | string | The compiled-in parser id, bytes in and metric out (`claude-statusline-json`, `codex-rollout-jsonl`, `cursor-statusline-json`, `pi-context-json`). |

`format` is not user-authorable: a new one needs core code, so the loader accepts
any string here and the intake refuses an unknown id at read time rather than
failing the whole manifest. Declaring the block is what separates a `gated`
refusal for a context-gated action (the channel exists, the metric has not landed
yet) from a permanent `no-coverage` one. Which agent uses which channel is in
[Agent coverage](agent-coverage.md).

## Token rules

A detail token (a `[details]` key, an alias, a `[[rules]]` detail, or a
`[[hooks.map]]` claim detail) must be a safe machine token: non-empty and drawn
from lowercase `a-z`, digits, `_`, and `-` only. This rejects the format
metacharacters (`#`, `{`, `}`, `,`), whitespace, control bytes, and any non-ASCII
glyph at the load boundary, so a corrupt token can never reach the render chain.
A `[details]` key that collides with a state token is rejected, because state
routing is normative and not manifest-overridable.

## A full manifest

```toml
min_engine_version = "0.1"

[identity]
process_names = ["claude"]

[hooks]
covers = ["working", "blocked", "idle", "lifecycle"]

[[hooks.map]]
event = "Notification"
matcher = "permission_prompt|elicitation_dialog"
claim = { state = "blocked", detail = "permission" }

[[hooks.map]]
event = "Stop"
claim = { state = "idle" }
turn_end = true

[[hooks.map]]
event = "SessionStart"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "SessionEnd"
claim = { lifecycle = "end" }

[capture]
visible = ["working", "idle", "blocked"]

[telemetry.context]
channel = "event"
format = "claude-statusline-json"

[[rules]]
state = "blocked"
detail = "permission"
priority = 100
region = "tail_lines(5)"
match = { any = [ { contains = "Do you want to proceed?" }, { regex = "❯\\s" } ] }

[[rules]]
state = "idle"
priority = 10
region = "tail_lines(50)"
skip_state_update = true
match = { all = [ { contains = "transcript" }, { not = { contains = "❯" } } ] }

[details]
rate_limit = { aliases = ["ratelimited", "rate-limited"] }
```
