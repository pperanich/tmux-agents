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
| `[keys]` | keys | table | Per-agent key sequences. Forbidden for `exec`. A `keys` action needs at least one entry across `[keys]` and `[api]`. |
| `[api]` | keys | table | Per-agent API-channel transports (below). Forbidden for `exec`. An agent may appear in `[keys]` or `[api]`, never both. |

Structural rules are enforced at parse: `kind = "keys"` requires at least one
transport entry across `[keys]` and `[api]` (an api-only action is legal) and
forbids `command` / `detach`; `kind = "exec"` requires `command` and forbids
`[keys]` / `[api]`; an agent named in both `[keys]` and `[api]` is a parse error
(the broker never picks a transport at act time, so there is no silent fallback).

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
otherwise-failing server is `error` (exit 1). The API path never degrades to keystrokes — firing a
stale key sequence into a pane whose prompt state just proved unknowable is
exactly what the guard exists to prevent.

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
| `approve` | keys | `state = ["blocked"], detail = ["permission"]` | Affirmative answer to a permission prompt (`1` for Claude, `Enter` for Codex; an API `permission-reply` `once` for OpenCode). |
| `deny` | keys | `state = ["blocked"], detail = ["permission"]` | Negative answer to a permission prompt (`Escape` for Claude/Codex; an API `permission-reply` `reject` for OpenCode). |
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
