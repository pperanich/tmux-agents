# Pane options and JSON contracts

`tma` keeps all shared state in tmux pane options (user options, pane-scoped). There is no socket and none
is needed: any `tmux show-options -p` or `#{@agent_state}` format string reads
the current verdict straight from tmux's own option store, and `tma ls --json`
gives the same data as a stable structured document. This page is the contract
for both: the pane-option schema and the JSON schemas.

## The stamp grammar

Option values are machine tokens, never glyphs. State is one of the closed
vocabulary `idle`, `working`, `blocked`, `unknown`. Glyph and color rendering
happens only in the surfaces, and is configurable.

`@agent_detail` is a lowercase machine token (`[a-z0-9_-]`) qualifying *why* the
pane is in that state. **Unlike `state`, this vocabulary is open and unstable
until 1.0**: an agent manifest may emit a token this engine has never heard of,
and it round-trips intact rather than erroring. Read it defensively — match the
tokens you know and degrade gracefully on the rest. What the bundled manifests
emit today:

| token | state | meaning |
|---|---|---|
| `permission` | `blocked` | a tool-use permission prompt: approving grants the one action in front of the user |
| `plan` | `blocked` | a plan-approval dialog. Its affirmative option grants **every following action**, so `tma act approve` deliberately does not resolve here |
| `trust` | `blocked` | a workspace-trust gate. Its affirmative option grants the **whole folder**, so neither `approve` nor `deny` resolves here |
| `rate_limit` | `working` **or** `blocked` | a usage-limit wait. The state is the whole point of the pair: `working/rate_limit` is the agent waiting out its own limit and resuming by itself, `blocked/rate_limit` is a wait that halted and needs you (a keypress, or a fresh prompt). A `wait --until blocked` that also reads the detail can tell "needs the clock" from "needs permission" |

`tma-core` additionally declares `question`, `error`,
`background` and `compacting` as constants; no bundled manifest emits them yet.

Every `@agent_*_at` value is epoch **milliseconds** (13 digits today), not
seconds. Millisecond resolution is what keeps two episodes opening in the same
wall-clock second distinguishable.

`@agent_attention` is a presence flag: the literal value `1` when set, and the
option is absent otherwise. It is compared against `@agent_since` by the
ordered-input clear, which is why the raise instant has to stay write-once.
Writers order a chained stamp so `@agent_stamped_at` is written last; a reader
that sees `stamped_at` older than `since` or `evidence_at` caught a chained write
mid-flight and should treat the tuple as in-progress.

One exception keeps that rule from latching. A chained stamp commits in
milliseconds, so a `since` more than 2 seconds ahead of `stamped_at` is not a
write in flight: it is a backward wall-clock step (a suspend, an NTP
correction) that stranded the write-once `since` in the future. Such a tuple
reads as settled, and the next publish rewrites `since` rather than holding it,
so a stepped clock costs one stale transition time instead of a pane that
re-captures every cycle for the rest of the session.

## Pane-option schema

The store carries provenance (`@agent_source`, `@agent_evidence_at`) so a
stateless producer can rank a stamped hook claim above its own fresh capture.
Pane-scoped options describe one agent pane; window-scoped and server-scoped
options carry rollups and hints.

| option | scope | semantics |
|---|---|---|
| `@agent_name`, `@agent_pid` | pane | identity (pid: process-group leader found by the walk) |
| `@agent_state`, `@agent_detail` | pane | the verdict (state) and its detail token |
| `@agent_source` | pane | provenance of the current state: `hook` / `capture` / `process`. `activity` is a legacy value still accepted on read; nothing produces it any more (a viewport hash change stopped being state evidence) |
| `@agent_evidence_at` | pane | epoch **ms** of the evidence behind the current state |
| `@agent_since` | pane | epoch **ms** of the state transition, written once by the first producer to record it and never rewritten while state is unchanged (the one exception is a value stranded ahead of `@agent_stamped_at` by a backward clock step) |
| `@agent_stamped_at` | pane | per-pane freshness marker, written last in a chained stamp |
| `@agent_hash` | pane | scheduling only, and **its value is not interpreted**: a hash of the last captured viewport whose PRESENCE means "this pane's screen has been read at least once", which is all any reader checks before reusing a stored stamp. Nothing compares two hashes, a changed one makes no claim about state, and the algorithm is not part of this contract. Treat it as a boolean; absent on a pane detected purely from hooks |
| `@agent_attention` | pane | presentation flag: value `1` when set, option absent otherwise. Cleared by the focus hooks (the pane arrived at, the pane departed) and by the poll cycle when a client displaying the pane was typed into after `@agent_since` |
| `@agent_notified_at` | pane | notification-episode marker, written only by the notifier |
| `@agent_turn_at` | pane | epoch **ms** of the last hook event that meant "a turn ended" and raised the done marker (`Stop`, codex's `notify`, pi's `agent_settled`). It exists because `@agent_since` is write-once per state run and so cannot move when a second completion lands on a pane that never left `idle`; the notify dedup and `wait --since` compare against the later of the two. Written only by the hook intake, and only when the marker was down, so one turn end reported on two channels records one turn. Absent on a pane that has never had one, which leaves both comparisons reading `@agent_since` alone |
| `@agent_session` | pane | owning agent session id from hook registration; the subagent guard compares incoming event session ids against this |
| `@agent_subagents` | pane | space-separated live subagent session ids; SubagentStart appends, SubagentStop removes; bookkeeping only, never top-level state. While it is non-empty, only an event whose session matches `@agent_session` may write state; an event that cannot be attributed (either side missing) is ignored |
| `@agent_context_pct` | pane | context-utilization metric percent (integer `0`–`100`), or absent when the agent has no telemetry coverage or the channel reported no window (a null-clear); written only by the context intake under the evidence-time write guard, never part of the state tuple |
| `@agent_context_at` | pane | epoch **ms** of the evidence behind `@agent_context_pct`; written last in the context mini-chain and advanced even by a null-clear, so a reordered stale push cannot walk the gauge backward (`not older` acceptance) |
| `@agent_tokens` | pane | tokens currently in the agent's context window: the absolute `@agent_context_pct` is a percent of, written in the same guarded chain. Absent for an agent whose channel reports no count tma can call a footprint (see [Agent coverage](agent-coverage.md#absolute-token-counts)) and cleared by any observation that carries none, so a stale count never sits beside a fresh gauge. A level, never a cumulative spend: tma stamps no usage totals and no cost |
| `@agent_tokens_at` | pane | epoch **ms** of the evidence behind `@agent_tokens`, set and cleared with it under the same guard. It equals `@agent_context_at` whenever a count is present — one observation stamps both — and exists so a reader that wants only the count can age it without reading the gauge's marker |
| `@agent_context_notified_at` | pane | the `context_high` notify marker: a present/absent **armed flag** (absent = armed, present = already fired), never the state lane's `@agent_notified_at`; its value is an epoch **ms** for debuggability only, not a comparison basis. Written only by the context-high notifier, guarded set-from-absent so concurrent firers resolve to one bell; cleared (rearmed) when the gauge dips below `threshold - 10` |
| `@agent_model` | pane | best-effort model-name label the file-tail context intake reads from the rollout window; never load-bearing for a gauge, it only feeds `tma doctor`'s recognized-model line (a model no `[telemetry.windows]` entry names). Plain-set, cleared on deregister, absent when no model record sat in the tail |
| `@agent_permission_request` | pane | the pending OpenCode permission request id, stamped by the event intake from a `permission.asked` edge (ownership-filtered against `@agent_session`) and cleared on the edges that end the prompt (a working/idle transition, or a `permission.replied`); the action broker reads it to answer an `[api]` `permission-reply` op, and an empty value refuses that op `requires-unmet`. The broker also clears it itself on a 2xx reply, so a spent id does not read as a pending request until the plugin's next event; it leaves the option alone on a 404, which may already name a newer request |
| `@agent_pending_tool`, `@agent_pending_call` | pane | the tool name and call id of the permission decision a `blocked` pane is waiting on, stamped from Claude Code's `PermissionRequest` hook (`tool_name` / `tool_use_id`). Set together with `@agent_pending_summary` and cleared together on every edge that ends the prompt: the pane leaving `blocked` (a `PostToolUse`/`Stop` working-or-idle stamp), and `SessionEnd`, which removes the whole tuple. Absent on an agent with no such hook |
| `@agent_pending_summary` | pane | a one-line summary of that call, derived from the hook's `tool_input`: the command for `Bash`, the file path for `Edit`/`Write`/`Read`, otherwise the first string-valued field. **Agent-supplied text**, capped at 120 bytes with control characters stripped and a `…` marking a truncation. Treat it the way you treat a pane title: it is not a machine token, and tma deliberately keeps it out of the notification payload, the notify audit line, and every `TMA_*` env var, so a summary can never reach a `[notify] command` sink or a third-party push carrier. It is here and on the JSON rows, both of which stay on your machine |
| `@agent_api_endpoint` | pane | the OpenCode server base URL, stamped at registration by the plugin from its serving address; the broker's `permission-reply` endpoint, with a `[api.opencode] api_base` config fallback (neither present refuses `requires-unmet`) |
| `@agent_ignore` | pane | **you set this one.** Any non-empty value takes the pane out of detection: no identity, no capture, no row, and a stamp left from before it was set is cleared on the next cycle. `tmux set-option -p @agent_ignore 1` in the pane (add `-t <pane>` from elsewhere), `tmux set-option -pu @agent_ignore` to undo. tma never writes or clears it; `tma doctor` lists every pane carrying it |
| `@agent_mute_until` | pane | notification mute deadline in epoch **ms**: while it is ahead of the clock the pane fires no notification of any kind (state triggers and `context_high` alike), and every other lane is untouched — it is still detected, stamped, and counted. `tma mute --for 30m` writes now + the window, a bare `tma mute` writes the far-future sentinel `99999999999999` (indefinite), and `tma mute --clear` unsets it. Living in the store is what makes a mute survive a tma or daemon restart |
| `@agent_action` | pane | single-flight action lock: value `<expiry_ms>:<nonce>:<pid>:<name>`, acquired and reclaimed by a server-side conditional write on the leading expiry, released nonce-conditionally, self-healing via the embedded expiry; written only by the action broker |
| `@agent_summary` | window | rollup, a pure function of the sibling panes' options; space-separated `<state>:<count>` in the fixed order `blocked working idle unknown`, zero-count states omitted, empty or absent when the window has no agents (e.g. `blocked:1 working:2`) |
| `@agent_session_summary` | session | the same rollup grammar over every agent pane in the session, written by the same writers under the same guards. A distinct key rather than `@agent_summary` at session scope because a pane-context format read falls back pane → window → session: one shared name would make an agentless window render its session's counts |
| `@tma_last_poll` | server | hint only; the per-pane `@agent_stamped_at` is authoritative for freshness |
| `@tma_watch_pid` | pane (on the watcher's own pane) | the focus-nudge target; `tma watch` advertises its pid here on its own `$TMUX_PANE` at startup and unsets it on every quit path, so it dies with the pane |

Values that depend on a previous value (the write-once `@agent_since`, the
notification dedup, the hook-versus-capture arbitration) are governed by
server-side conditional writes (`set-option -pF`), which expand formats in the
target pane's context atomically at write time. Everything else is
last-writer-wins over deterministic values, which converges.

Reading these from your own bar, prompt, or script is a supported first-class use;
[Read agent state from a status bar or
script](../how-to/read-agent-state-from-a-status-bar-or-script.md) covers
the read forms and the freshness rule that goes with them.

## `tma ls --json`

A versioned, additive-only document. The top level is `{ "schema": 1, "agents":
[ ... ] }`; each element of `agents` is one agent row with this exact key set:

| key | type | meaning |
|---|---|---|
| `pane` | string | tmux pane id (e.g. `%5`) |
| `agent` | string | agent name |
| `state` | string | `idle` / `working` / `blocked` / `unknown` |
| `detail` | string or null | detail token, `null` when none. [Open vocabulary](#the-stamp-grammar) — read defensively |
| `since` | number | epoch **ms** of the last state transition (original unsuffixed key, kept for compatibility) |
| `since_ms` | number | the same value as `since`; names the unit, preferred in new consumers |
| `episode_ms` | number | epoch **ms** of the instant `wait --since` compares against: the later of `since_ms` and `@agent_turn_at`. Equal to `since_ms` until a second completion lands on a pane that never left `idle`, and the value a supervisor loop feeds back as its next `--since` (see [Drive a supervisor loop](../how-to/block-a-script-on-agent-state.md#drive-a-supervisor-loop)) |
| `locator` | string | `session:window.pane` |
| `title` | string | pane title |
| `attention` | boolean | `true` when the pane still carries `@agent_attention` |
| `done` | boolean | `true` when the pane is idle **and** carries `@agent_attention`: finished with output nobody has reviewed |
| `session` | string or null | owning agent session id, `null` when the pane never registered one |
| `context` | number or null | context-utilization percent (`0`–`100`), `null` when the agent has no telemetry coverage or the channel reported no window |
| `context_at_ms` | number or null | epoch **ms** of the evidence behind `context`, `null` when `context` is |
| `muted` | boolean | `true` when the pane's `@agent_mute_until` is still ahead of the clock, so its notifications are suppressed ([`tma mute`](cli.md#tma-mute)). A resolved boolean, not the deadline: the row is rendered at a known instant, and this is the only question a consumer asks |
| `tokens` | number or null | tokens currently in the context window (the absolute `context` is a percent of), `null` when the agent's channel reports no count tma can call a footprint. Aged by `context_at_ms`, which is its evidence time too. Never a spend total |
| `repo` | string or null | the pane's git repo name (basename of the git common dir's parent, so worktrees share their origin's name), `null` when the pane's cwd resolves to no repo |
| `branch` | string or null | the pane's checked-out branch (the literal `HEAD` for a detached head), `null` when `repo` is |
| `worktree` | boolean or null | `false` for a resolved main checkout, `true` for a linked worktree, `null` exactly when `repo` is |
| `pending_tool` | string or null | the tool name of the permission decision the pane is waiting on (`@agent_pending_tool`), `null` when nothing is pending |
| `pending_call` | string or null | that call's id, `null` exactly when `pending_tool` is (an agent whose hook carries no id stamps `""`) |
| `pending_summary` | string or null | one line of at most 120 bytes describing the call, `…` where it was truncated, `null` exactly when `pending_tool` is. **Agent-supplied text** (a command line, a path): treat it as you treat `title`. It is deliberately absent from the notification payload, the notify audit line, and every `TMA_*` env var, so it never leaves the machine through a `[notify] command` sink |
| `server` | string | the tmux server this row was observed on: its own `#{socket_path}` (e.g. `/private/tmp/tmux-501/default`) |
| `host` | string | the hostname of the machine that observed it |

The "done" surface is `state == "idle"` and `attention == true`; `done` carries
that conjunction precomputed from the one definition the whole tool shares (it is
also what `wait --until done` and `--state done` mean), so consumers stop
re-deriving it. The state token itself is never mangled — it stays `idle`. All of
`attention`, `done`, `session`, `context`, `context_at_ms`, `muted`, `tokens`,
`repo`, `branch`, `worktree`, `pending_tool`, `pending_call`, `pending_summary`,
`server`, and `host` are additive, so the schema stays `1`;
render an absent `context` or `tokens` as absence (no gauge, no count), never as
`0`. The `repo`/`branch`/`worktree`
keys are best-effort: the resolver memoizes one bounded `git` call per unique cwd
and degrades every field to `null` on any failure, so a consumer treats them as
hints, never guarantees.

### `server` and `host`: merging rows from more than one place

A pane id is unique within one tmux server and nothing more. Collect
`tma ls --json` from your laptop and from a build box and both sets will contain a
`%5`, with no way to tell them apart — the same is true of two servers on one
machine (`tmux -L work` and `tmux -L scratch` number panes independently). The
`server`/`host` pair is what makes a merged set addressable: `(host, server, pane)`
is the key you want, and either alone is not enough.

`server` is the server's own `#{socket_path}`, which is what the daemon already
keys its per-server socket and lock on, so it is stable for the life of the
server and identifies it however you addressed it: `--socket-name work`,
`--socket-path /tmp/tmux-501/work`, and an invocation from inside that server all
report the same value. The path rather than a hash of it, because an operator
reading a merged log can tell `/private/tmp/tmux-501/default` from a tmate socket
at a glance.

Both are resolved once per invocation — one tmux call and one `uname` — and
repeated onto every row, since a line-oriented consumer that filters `agents`
down to one element must not lose the provenance with it. A long-lived
`tma subscribe` resolves them once for the life of the stream.

## `tma wait --json`

The single matched agent row as one schema-1 object: the top-level `schema` key
plus the same row fields as an `ls --json` element (`pane`, `agent`, `state`,
`detail`, `since`, `since_ms`, `episode_ms`, `locator`, `title`, `attention`, `done`,
`session`, `context`, `context_at_ms`, `muted`, `tokens`, `repo`, `branch`,
`worktree`, `pending_tool`, `pending_call`, `pending_summary`, `server`, `host`). It shares the serialization with `ls --json`, so the two can
never disagree on keys, order, or null handling.

The fleet targets satisfy a SET of panes, so `wait --all --json` and `wait
--count <n> --json` emit the `ls --json` document instead — `{ "schema": 1,
"agents": [ ... ] }`, one element per satisfied row, with those same row keys. A
consumer parses one shape whether it listed or waited; the single-object form
stays the single-pane targets' emission (`--pane`, `--agent`, `--any`).

## Notification hook payload

When a `[notify]` command fires, it receives one JSON object on stdin. It
carries metadata only, never captured screen content. The exact top-level key
set:

| key | type | meaning |
|---|---|---|
| `schema` | number | payload schema version (`2`); kept the first key so a reader sees it up front |
| `agent` | string | agent name |
| `pane` | string | tmux pane id |
| `state` | string | the landed state (`blocked`, or `idle` for a completion) |
| `detail` | string or null | detail token, `null` when none. [Open vocabulary](#the-stamp-grammar) — read defensively |
| `session` | string or null | owning agent session id, `null` when none |
| `locator` | string | `session:window.pane` |
| `title` | string | pane title. **Absent unless `[notify] include_title = true`** — see below |
| `repo` | string | repo name resolved from the pane's working directory, `""` when it is not a checkout |
| `branch` | string | branch name (the literal `HEAD` when detached), `""` when unresolved |
| `since_ms` | number | age of the episode when the notification fired (`now - episode_ms`); a hook's own direct fire reads `0`, the daemon's reads its dispatch latency |
| `episode_ms` | number | epoch **ms** of the episode this fire belongs to: `max(@agent_since, @agent_turn_at)`, the same instant the row's `episode_ms` reports. Absolute, so two fires for one episode carry the same value and a sink can collapse on it (`apns-collapse-id` and friends); `since_ms` is the age of this instant and cannot be compared for equality against a stored stamp |
| `context_pct` | number or null | the pane's stored context-window utilization percent, `null` when the agent reports none |

### The pane title is not sent

The pane title is the one field whose content the pane's own program controls,
and it routinely holds a branch name, a repo path or a prompt fragment.
`notify.command` pipes this payload to whatever you configured — ntfy, Pushover,
an Apple Shortcut — so the title would reach that service's operator on every
fire. Since schema 2 it is **omitted by default**.

`[notify] include_title = true` puts it back, and it governs all three carriers
together: the payload's `title` key, the `TMA_TITLE` environment variable, and
the `notify.log` audit line. The host-local `display-message` baseline is not a
carrier and always shows the title.

The standing rule for this payload: **no field enters it that is not safe
world-readable.** Its writer is also the audit log's writer, and that log is the
file most likely to end up pasted into an issue.

The same values are also exported as environment variables (`TMA_AGENT`,
`TMA_PANE`, `TMA_STATE`, `TMA_LOCATOR`, `TMA_SINCE_MS`, `TMA_EPISODE_MS`, plus
`TMA_TITLE` when `include_title` is on,
`TMA_DETAIL`, `TMA_SESSION`, `TMA_REPO`, `TMA_BRANCH` and `TMA_CONTEXT_PCT` when
they have a value), so a hook reads whichever is more convenient. Unlike the
JSON, a value with nothing to report is an unset variable rather than an empty
string.

The daemon and the daemonless `tma event` path build this payload through one
shared builder, so the same transition yields the same object either way.

The `[notify] log` file (see [Configuration](configuration.md#notify-notifications))
holds the same object per line, with one extra key: `at`, the fire time in epoch
milliseconds, written directly after `schema`. A detached action's completion is
logged the same way, as its own payload (below) plus `at`; a completion line
carries `action` and `outcome` where a state line carries `state`, which is how a
reader tells the two apart.

## `tma act` JSON result

The result of firing one action, a schema-1 object with this exact key set. See
[`tma act`](cli.md#tma-act) for the verb and its exit codes.

| key | type | meaning |
|---|---|---|
| `schema` | number | payload schema version (`1`) |
| `action` | string | the action name |
| `pane` | string | the target pane id |
| `outcome` | string | the closed outcome token (below) |
| `exit_code` | number | the process exit code this outcome maps to; for an `exited` outcome it is the exec child's own code |
| `reason` | string or null | the refusal reason token when `outcome` is `refused`, which target went away when `outcome` is `vanished`, `null` otherwise |

`tma act --all --json` wraps those same objects: `{ "schema": 1, "results":
[ ... ] }`, one element per resolved target in the order they were fired, each
with the exact key set above. The envelope appears whenever `--all` is passed,
even for a single matched pane, so a consumer's parse never depends on the match
count. The process exit code is the worst element's (see
[Fan-out](cli.md#fan-out---all)); the per-element `exit_code` stays each target's
own.

`outcome` is authoritative and closed: `sent` (keys delivered), `replied` (an
API-channel answer delivered over HTTP, a 2xx), `exited` (a synchronous exec
child finished; `exit_code` is its code), `spawned` (a detached supervisor
launched), `timeout` (a synchronous child killed at `timeout_ms`), `refused`
(`reason` carries which gate), `vanished` (tmux reports the pane gone, or an
API target answered/withdrawn between gate and act — a 404; `reason` carries
which), `error` (broker runtime failure: an unreachable API server, or a tmux
command the server refused, whose stderr rides in the message). `reason` is one
of `gated`, `requires-unmet`, `wrong-agent`, `no-coverage` (all exit `4`),
`locked` (exit `5`), or — on a `vanished` outcome, both exit `3` — `pane-gone`
(tmux says the pane is gone) and `request-gone` (the API server answered `404`:
the permission request was already answered or withdrawn, on a pane that is
still there).

## `tma act` list document

A schema-1 document enumerating the loaded actions from `tma act --list --json`:
`{ "schema": 1, "actions":
[ ... ] }`. Each action carries this exact key set, and with `--pane` two more.

| key | type | meaning |
|---|---|---|
| `name` | string | the action name (also its file stem) |
| `label` | string | the human label |
| `kind` | string | `keys` or `exec` |
| `agents` | array of string | the agents this action applies to (empty means all, for an `exec` action). For a `keys` action this is the union of its `[keys]` and `[api]` transport agents — no per-transport surface in v1, a deck does not care how the answer travels |
| `when` | object or null | the gate, or `null` when the action is always fireable for its agents |
| `fireable` | boolean | present only with `--pane`: whether the action can fire on that pane right now |
| `reason` | string or null | present only with `--pane`: the refusal reason token when not fireable, `null` when fireable |

The `when` object carries `state` (array of state tokens), `detail` (array of
detail tokens), `context_pct_min`, and `context_pct_max` (number or null each).

## Detached-action completion payload

A detached (`detach = true`) action fires one completion notification through the
`[notify]` command when its child exits. It is its own contract, distinct from the
[notification hook payload](#notification-hook-payload) (a completion has no
`state`, and its pane may already be gone). The exact top-level key set:

| key | type | meaning |
|---|---|---|
| `schema` | number | payload schema version (`1`) |
| `action` | string | the action name |
| `pane` | string | the target pane id |
| `agent` | string | the agent name |
| `outcome` | string | the outcome token (`exited` / `timeout` / `error`) |
| `exit_code` | number or null | the child's exit code for `exited`, `null` for a deadline kill or spawn failure |
| `locator` | string or null | `session:window.pane`, `null` when the pane is already gone |
| `lock_release_failed` | boolean | present only as `true`, when the supervisor's nonce-conditional clear of `@agent_action` failed; absent on the ordinary release. Additive, so the schema stays `1`. A dead pane's failing option write correlates with a null `locator` |

The same values reach the command as environment variables: `TMA_ACTION`,
`TMA_PANE`, `TMA_AGENT`, `TMA_OUTCOME`, plus `TMA_EXIT_CODE` and `TMA_LOCATOR`
when they have a value. As on the state path, a value with nothing to report is an
unset variable rather than an empty string, so `${TMA_EXIT_CODE:-}` is how a hook
tells a deadline kill from a child that exited. There is no env mirror of
`lock_release_failed`; read it from the JSON.

A completion rides the same sinks a state notification does: the `display-message`
baseline, the opted-in `bell`/`osc` tty sinks, and the `[notify] log` audit line.
A hook that cannot start, or that exits non-zero, updates the same failure marker
`tma doctor` reports.

## `tma doctor --json`

The whole diagnosis as one schema-1 object. It is grouped rather than flat: each
check is its own sub-object, so a consumer reads `.daemon.alive` rather than
guessing which prefix belongs to what. Top level:

| key | type | meaning |
|---|---|---|
| `schema` | number | payload schema version (`1`) |
| `daemon` | object | `alive` (boolean), `socket` (string, `null` when the server was unreachable and no socket could be keyed), `version` (string or null), `version_matches` (boolean, `null` when there is no reported version to compare against this build) |
| `ambient_driver` | object | `polling` (boolean: the server option `@tma_last_poll` carries a non-zero timestamp), `last_poll_age_ms` (number, `null` when it does not) |
| `clients` | object | `attached` (number of attached clients) |
| `status_option` | object | `enabled` (boolean: the server's global `status`) |
| `mouse` | object | `bindings_installed` (boolean), `enabled` (boolean: the server's `mouse` option). Both true is the working state; installed-without-mouse is the warning |
| `watch` | object | `running` (boolean), `watchers` (number of panes advertising `@tma_watch_pid`) |
| `wrapper` | object | `path` (string), `present` (boolean) for the `tma-hook` wrapper |
| `notify` | object | `last_failure`: `null`, or an object of `at` (epoch ms), `reason`, `command` |
| `tmux_hooks` | array | one object per checked hook: `hook` (name), `present` (boolean), `hook_state` (`present` / `drifted` / `wiped` / `missing`) |
| `manifests` | object | `ok` (number loaded) and `issues`, an array of `{ file, problem }` |
| `process_name_issues` | array | `{ agent, name, comm_max }` per `process_names` entry past the truncation width |
| `process_walk` | object | `ok` (boolean: the `ps` walk ran) and `error` (string or `null`). With `ok: false` the `agents` array holds only panes a hook registered |
| `nested_multiplexers` | array | `{ pane, locator, command }` per pane running an inner multiplexer client |
| `remote_panes` | array | `{ pane, locator, command, stamped }` per pane behind a remote shell; `stamped` says whether it still carries a held `@agent_*` stamp |
| `ignored_panes` | array | `{ pane, locator, value }` per pane carrying `@agent_ignore`, with the value you set |
| `stamp_issues` | array | `{ pane, locator, problem }` per pane carrying an `@agent_*` option that does not decode |
| `agents` | array | one object per agent pane (below) |
| `actions` | object | `ok` (number loaded) and `issues`, an array of `{ file, problem }` |

Each `agents` element carries this exact key set:

| key | type | meaning |
|---|---|---|
| `pane`, `agent`, `locator` | string | the pane, its agent name, and `session:window.pane` |
| `state` | string or null | the stamped state token, `null` when the pane has none |
| `source` | string or null | provenance of that state (`hook` / `capture` / `process`; `activity` is legacy, read-only) |
| `evidence_age_ms` | number or null | age of the evidence behind it |
| `hook_status` | string | `wired`, `incomplete`, `not_installed`, `hookless`, or `no_adapter` |
| `hooks_wired` | boolean | `true` only for `wired`, so a consumer needs no token table for the common question |
| `model` | string or null | the best-effort `@agent_model` label |
| `window_covered` | boolean or null | whether `[telemetry.windows]` names that model; `null` when there is no model to check |
| `endpoint_ok` | boolean or null | whether the pane's API endpoint answered; `null` when the agent has no API lane |
| `hook_demoted` | boolean | registered through a hook but currently running on capture evidence: output kept arriving that its hooks did not account for. A `working` hook claim accounts for output until capture contradicts it, so a long tool call does not set this |
| `tier` | number | the effective tier (`3` / `2` / `1`) |
| `tier_reason` | string or null | why it is not higher; `null` at the top of what its manifest supports |

`remote_panes` and `ignored_panes` are additive, so the schema stays `1`.

## `tma debug explain --json`

One pane's identity, rule evaluation, and verdict as a schema-1 object. Absent
optional fields are an explicit `null`, never dropped.

| key | type | meaning |
|---|---|---|
| `schema` | number | payload schema version (`1`) |
| `pane`, `locator`, `command`, `title` | string | the pane id, `session:window.pane`, `#{pane_current_command}`, and the pane title |
| `agent` | string | the resolved agent name, or `unknown` |
| `identity_source` | string or null | `observed` (the process walk found it) or `registered` (a hook claimed the pane); `null` when nothing identified it |
| `out_of_scope` | string or null | the foreground command that put the pane out of scope (a remote shell, an inner multiplexer) |
| `out_of_scope_kind` | string or null | which category that command falls under |
| `registered_behind` | string or null | the boundary a live registration outranks: the pane is in scope, but its agent runs where no capture reaches |
| `registered_behind_kind` | string or null | that boundary's category |
| `ignored` | boolean | the pane carries `@agent_ignore`, which is why an otherwise recognizable pane reports no agent and no verdict |
| `foreground_is_agent` | boolean | whether the foreground command is the agent itself; `false` caps every screen verdict at `unknown` |
| `scrolled`, `history_view` | boolean | the pane is scrolled back, and the screen is showing history rather than live state |
| `evidence` | array | `{ source, claim, at, meta }` per evidence record the fold saw |
| `rules` | array | `{ index, matched, state, detail, priority, region, skip_state_update }` per screen rule evaluated; empty when no rules ran |
| `verdict` | object or null | the fold's result, `null` when nothing was evaluated (an ignored or unidentified pane) |

The `verdict` object carries `state`, `detail` (string or null), `action`
(`publish` or `hold`), `may_override`, `set_attention`, `episode_reset`, and
`winning_evidence`, itself `{ source, at, label }`.

`ignored` is additive, so the schema stays `1`.

## Additive-schema discipline

All the JSON contracts on this page share one rule: additive changes (a new key)
keep the `schema` at `1`; a breaking change (renaming or removing a key) bumps
`schema` and the exact-key-set drift tests with it. A consumer can branch on
`schema` rather than guess. Each serialization site has a test pinning its exact
key set, so a silent drift cannot ship.
