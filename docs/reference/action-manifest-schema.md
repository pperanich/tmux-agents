# Action manifest schema

One TOML manifest declares one action: what it fires (`keys` into the pane, or an
`exec` process), which agents and states it applies to, and how the broker guards
it. Bundled actions ship as manifests in `crates/tma-core/actions/`; a user
manifest in `~/.config/tma/actions/` adds a new action or shadows a bundled one by
filename stem, with no code change. Fire one with [`tma act`](cli.md#tma-act); to
author one, see [Author a custom action](../how-to/custom-actions.md).

The action name is normative: `name` must equal the filename stem, so a user file
cannot collide with a bundled action's name without also shadowing it. Unknown
fields at any level are a parse error, the same discipline as the agent manifest
and `config.toml`.

## Top level

| field | required | type | meaning |
|---|---|---|---|
| `min_engine_version` | yes | version string | The minimum engine version this action needs (e.g. `"0.1"`). A manifest that needs a newer engine is rejected with an upgrade error. |
| `name` | yes | string | The action name; must equal the filename stem. Invoked as `tma act <name>`. |
| `label` | yes | string | The human label shown in `--list` and the menu. |
| `kind` | yes | `keys` \| `exec` | `keys` sends a guarded key sequence into the pane; `exec` spawns a guarded process with context env. |
| `when` | no | table | The gate. Absent means the action is always fireable for its applicable agents. |
| `agents` | no (exec) | array of string | Which agents an `exec` action applies to; empty (the default) means all agents. A `keys` action derives applicability from its `[keys]` table instead, so this is ignored for `keys`. |
| `requires` | no | array of token | Context keys that must be non-empty for the gate to pass: `session`, `cwd`, `pid`, `title`. An unknown token is a parse error. |
| `confirm` | no | bool | Mark the action as wanting a second factor (below). Default `false`. |
| `detach` | no (exec) | bool | Run an `exec` action detached under a tma-owned supervisor. Default `false`. Forbidden for `keys`. |
| `timeout_ms` | no (exec) | integer | Synchronous exec timeout in milliseconds. Default `30000`. |
| `detach_timeout_ms` | no (exec) | integer | Detached exec wall-clock deadline in milliseconds, after which the supervisor kills the process group. Default `900000` (15 minutes). |
| `command` | yes (exec) | string | The exec command, passed to `sh -c` verbatim with no substitution. Required for `exec`, forbidden for `keys`. |
| `[keys]` | keys | table | Per-agent key sequences. Forbidden for `exec`. A `keys` action needs at least one entry across `[keys]`, `[api]` and `[hook]`. |
| `[api]` | keys | table | Per-agent API-channel transports (below). Forbidden for `exec`. An agent may appear in `[keys]` or `[api]`, never both. |
| `[hook]` | keys | table | Per-agent hook-lane transports (below). Forbidden for `exec`. An agent may appear in `[keys]` and `[hook]` at once; in `[api]` and `[hook]` never. |

Structural rules are enforced at parse: `kind = "keys"` requires at least one
transport entry across `[keys]`, `[api]` and `[hook]` (a single-transport action is
legal) and forbids `command` / `detach`; `kind = "exec"` requires `command` and
forbids all three tables; an agent named in both `[keys]` and `[api]`, or in both
`[api]` and `[hook]`, is a parse error (the broker never picks between two
structured transports at act time, so there is no silent fallback).

## `[when]`: the gate

All present keys are ANDed. A `keys` action re-verifies a stale state stamp with a
fresh detection cycle before gating.

| field | required | type | meaning |
|---|---|---|---|
| `state` | no | array of state | The states that satisfy the gate: `idle`, `working`, `blocked`, `unknown`. |
| `detail` | no | array of detail token | Detail tokens that satisfy the gate (e.g. `permission`). |
| `context_pct_min` | no | integer | Minimum context-utilization percent. **Fails closed**: an absent metric refuses. |
| `context_pct_max` | no | integer | Maximum context-utilization percent. Fails closed the same way. |

A context bound that reads a metric the agent's manifest declares no telemetry
channel for refuses permanently with reason `no-coverage`; a bound whose metric is
merely absent right now refuses with `gated` (see the reason tokens in
[Pane options and JSON contracts](pane-options-and-json.md#tma-act-json-result)).

## `[keys]`: per-agent key sequences

Each key is an agent name and its value is the key sequence for that agent. An
agent with no entry cannot receive the action (that is how a `keys` action's
applicability is derived).

Each array element is one tmux `send-keys` key argument with named-key
interpretation on, so `Enter`, `Escape`, `C-c`, and `/compact` mean what tmux says
they mean; the whole sequence is delivered in a single `send-keys` through the
`tma-tmux` write adapter, with no inter-key delay.

```toml
[keys]
claude = ["1"]
codex = ["Enter"]
```

## `[api]`: per-agent API-channel transports

Some agents answer a prompt over HTTP instead of via keystrokes. `[api]` maps an
agent name to a built-in operation the broker delivers with one HTTP POST rather
than a `send-keys` (OpenCode, whose server answers a pending permission). It is a
transport for the same action, not a new action: `approve` on a Claude pane sends
keys, on an OpenCode pane it replies over the API, under one name and one gate.

Applicability is the union of `[keys]` and `[api]`; an agent in both tables is a
parse error. The operation vocabulary is closed — v1 ships exactly
`permission-reply`, whose `reply` is one of `once` / `always` / `reject`. An
unknown `op` or `reply` (or a missing `reply`) is a parse error.

```toml
[api]
opencode = { op = "permission-reply", reply = "once" }
```

The broker reads the pending request id from `@agent_permission_request` and the
server base URL from `@agent_api_endpoint` (both stamped by the OpenCode plugin),
falling back to `[api.opencode] api_base` in `config.toml` for the endpoint. An
empty request id or no resolvable endpoint refuses `requires-unmet` before the
lock. The POST is bounded by `timeout_ms` (connect and total, no retry): a 2xx is
the `replied` outcome, a 404 (the prompt was answered or withdrawn first) is
`vanished` with `reason` `request-gone` (exit 3), and an unreachable or
otherwise-failing server is `error` (exit 1). A 2xx also clears
`@agent_permission_request`: the id is spent, and leaving it stamped until the
plugin's next `permission.replied` event lets a later reader mistake it for a
pending request. A 404 leaves the option alone, since it may already name a newer
request the plugin stamped. The API path never degrades to keystrokes — firing a
stale key sequence into a pane whose prompt state just proved unknowable is
exactly what the guard exists to prevent.

## `[hook]`: per-agent hook-lane transports

The third transport, beside a keystroke and an HTTP POST: an answer returned to the
agent's own permission hook. `[hook]` maps an agent name to the verdict the broker
writes when a hook is parked on the pane's current request. v1 covers claude, whose
`PermissionRequest` hook holds the tool call open while the [hook reply
lane](../how-to/install-agent-hooks.md#answer-claudes-prompts-over-the-hook-lane) is
switched on.

```toml
[hook]
claude = { verdict = "allow" }
```

`verdict` is the only key and its vocabulary is closed: `allow` and `deny`. Any
other value, or a missing one, is a parse error. There is deliberately no spelling
for `approve_always` here, since a standing grant is not a decision to take from a
transport whose caller saw exactly one call.

Applicability is the union of all three tables. An agent may sit in `[keys]` and
`[hook]` at once, and the bundled `approve` and `deny` both do for claude: that
overlap IS the degradation path, because a fire falls through to the key sequence
whenever no hook is holding. An agent in `[api]` and `[hook]` is refused at parse
for the reason `[keys]` and `[api]` cannot share one either, and the direction
matters: a hook-lane miss falls through to keystrokes, never to HTTP, and a manifest
implying otherwise should not load. Only `kind = "keys"` may carry the table; a
`kind = "exec"` action with a `[hook]` is a structural error.

The broker takes this arm only when both halves line up: the action has a `[hook]`
entry for the pane's agent, and a request record is parked for the id the pane
carries in `@agent_permission_request`. Under the pane's held single-flight lock it
creates the verdict file (a temp file in the same directory, fsynced, then
`link(2)`, so a second dispatch cannot answer one request twice), clears
`@agent_permission_request`, and reports outcome `replied` (exit 0). Spending the id
there is what makes a second dispatch quoting it refuse `request-gone` (exit 4) at
the [`--expect-permission-request`
binder](cli.md#binding-a-dispatch-to-the-pane-you-saw), before it reaches the file
at all. A verdict file that somehow already exists is the request having been
answered in the gap: `vanished` with reason `request-gone` (exit 3), the same pair
the API lane reports on a 404, and nothing is overwritten. With no record on disk
the arm is skipped and the `[keys]` sequence fires as it always has.

## `requires` and the context env

An `exec` action's `command` receives context only as environment variables (never
interpolated into the command string). `requires` names the keys that must be
non-empty for the gate to pass, so a script never half-runs on a missing value.

| token | env var | source |
|---|---|---|
| `session` | `TMA_SESSION_ID` | the agent's own session id (`@agent_session`) |
| `cwd` | `TMA_CWD` | the pane's current path |
| `pid` | `TMA_PID` | the process-group leader pid |
| `title` | `TMA_TITLE` | the pane title (untrusted text) |

Beyond the `requires` set, every exec action also receives `TMA_PANE`,
`TMA_AGENT`, `TMA_STATE`, `TMA_DETAIL`, `TMA_LOCATOR`, and `TMA_ACTION`. Quote
every `TMA_*` expansion in the script: a pane title is attacker-influenced text,
kept inert only by env transport.

Caller-supplied values arrive the same way. `tma act <name> --arg <value>` (
repeatable) sets:

| env var | value |
|---|---|
| `TMA_ARG` | the first `--arg` value |
| `TMA_ARG_1` … `TMA_ARG_N` | every value in order |
| `TMA_ARG_COUNT` | how many were passed |

None of the three is set when no `--arg` was passed, so a script can tell "not
passed" from "passed empty". Values are never interpolated into `command`: they
cross as environment for the same reason `TMA_TITLE` does, so a value carrying
`$(...)` or `;` is data the shell has no occasion to re-parse. A `keys` action
rejects `--arg` (exit 2) — its sequence is manifest-static, which is what makes
it reviewable — so anything that turns a value into keystrokes is an `exec`
action whose script decides what to type, and should set `confirm = true`.

## `confirm`: the second factor

`confirm = true` marks an action as wanting confirmation before it fires.
Enforcement is per-surface: the CLI takes `--yes` or an interactive prompt on a
TTY, the menu nests a confirm entry, and the broker refuses a confirm action from
a non-TTY without `--yes` so a script cannot stumble into one. Set it for anything
that injects into a live session or mutates a repo; tma cannot inspect what a user
script does, so this one bit is the author's honest declaration.

## Bundled actions

| name | kind | gate | effect |
|---|---|---|---|
| `approve` | keys | `state = ["blocked"], detail = ["permission"]` | Affirmative answer to a permission prompt (`1` for Claude, `Enter` for Codex; an API `permission-reply` `once` for OpenCode; a hook `verdict = "allow"` for Claude when one is holding). |
| `deny` | keys | `state = ["blocked"], detail = ["permission"]` | Negative answer to a permission prompt (`Escape` for Claude/Codex; an API `permission-reply` `reject` for OpenCode; a hook `verdict = "deny"` for Claude when one is holding). |
| `interrupt` | keys | `state = ["working"]` | Interrupt a working agent. |
| `compact` | keys | `state = ["idle"], context_pct_min = 75` | Compact the context window once it is high (`/compact` Enter for Claude). |

Shadow any of these by dropping a file of the same stem in
`~/.config/tma/actions/` (for example, retune `compact`'s threshold).

## A full manifest

```toml
min_engine_version = "0.1"
name = "summarize"
label = "Summarize progress"
kind = "exec"
agents = ["claude"]
when = { state = ["working", "idle"] }
requires = ["session"]
confirm = true
detach = true
detach_timeout_ms = 120000
command = "~/.config/tma/actions/summarize.sh"
```
