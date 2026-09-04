# Agent coverage

`tma` detects an agent's state from three kinds of evidence: hook events the
agent fires, the pane's on-screen chrome, and the process running in the pane.
This page documents, per agent, which hook events map to which states, which
states the screen rules cover, and how each agent's pane is identified.

Every mapping here is verified against real agent behavior, not documentation:
each table reflects events actually observed firing with a full payload.

## Coverage matrix

| agent | mechanism | hook coverage | context telemetry | notes |
|---|---|---|---|---|
| Claude Code | `settings.json` `hooks` block or plugin manifest | working / idle / blocked / lifecycle | statusline push shim: per-turn `context_window.used_percentage`, parsed by `claude-statusline-json` (event channel) | The `Notification` matcher distinguishes permission prompts from idle reminders. `tma install-hooks claude` writes the block and the statusline context shim. |
| Codex CLI | two channels: a `notify` program in `config.toml` (payload as a trailing argv arg) and a Claude-style `hooks.json` in `CODEX_HOME` (payload on stdin, real session id) | working / idle / blocked / lifecycle, all hook-covered; blocked, working and idle also screen-carried | rollout `token_count` file-tail, parsed by `codex-rollout-jsonl` (file-tail channel): the per-turn `token_count` event carries `total_token_usage` + `model_context_window`, so the percent needs no model table | `tma install-hooks codex` writes both. The `hooks.json` entries need one-time in-TUI trust (`/hooks`) before they fire. Discovery keys the pane's rollout file off `@agent_session`; its cross-version stability is the fixture caveat in ACTIONS.md open question 6. |
| OpenCode | JS plugin in `~/.config/opencode/plugin/` | working / idle / blocked / lifecycle (registration only); blocked, working and idle also screen-carried | — | No session-end or subagent hooks; deregistration rides the pid-change / pane-close path. `tma install-hooks opencode` writes the plugin. Answers `approve`/`deny` over its HTTP API rather than keystrokes (API lane, below). |
| Gemini CLI | `settings.json` `hooks` object, native event names | working / idle / blocked / lifecycle, hook- and screen-carried | — (turn-granularity token counts only) | Reuses the Claude JSON editor over `~/.gemini/settings.json`. Local config is gated behind a per-folder trust prompt. |
| Cursor CLI | user-level `~/.cursor/hooks.json` in cursor's own shape, plus a `statusLine` shim in `~/.cursor/cli-config.json` | working / idle / lifecycle via hooks; blocked screen-only | statusLine push shim: per-turn `context_window` (`total_input_tokens` / `context_window_size`), parsed by `cursor-statusline-json` (event channel). The `statusLine` mechanism works but is undocumented (highest churn risk) — a payload change degrades to an absent gauge | Cursor exposes no permission hook, so blocked rides a screen rule. `tma install-hooks cursor` writes cursor's own hooks JSON shape and the statusLine context shim. |
| pi | extension module in `~/.pi/agent/extensions/` | working / idle / lifecycle via the extension; no blocked | `getContextUsage()` push shim: the extension forwards pi's `ctx.getContextUsage()` (a precomputed `percent` + absolute `contextWindow`) on the turn-settled event, parsed by `pi-context-json` (event channel) | pi auto-runs tools with no approval state, so there is no blocked signal at all. `tma install-hooks pi` drops the extension. |

The context-telemetry column records how tma obtains each agent's context-window
utilization percent (`@agent_context_pct`), declared per agent by a
`[telemetry.context]` manifest block naming the channel shape (`event` /
`file-tail` / `screen`) and a compiled-in parser `format`. Claude Code's
statusline command receives a per-turn JSON payload; `tma install-hooks claude`
installs a chaining statusline shim that runs the user's existing statusline
command unchanged and forwards the payload to `tma event --agent claude --kind
context --pane "$TMUX_PANE" --payload -` fire-and-forget. Codex uses the pull
shape instead: the poll cycle tails the last
64 KiB of the session's rollout JSONL (discovered from `@agent_session`), reads
the newest `token_count` record, and stamps the gauge under the same
evidence-time guard — no shim, no persisted offset. pi uses the push shape like
Claude: its extension API exposes `ctx.getContextUsage()` directly (a precomputed
`percent` against pi's own window, so no window table is needed), which
`tma install-hooks pi`'s extension forwards on the turn-settled event, parsed by
`pi-context-json`. Cursor also uses the push shape: its `~/.cursor/cli-config.json`
carries a `statusLine` command that runs per turn with a `context_window` payload
(`total_input_tokens` / `context_window_size`), which `tma install-hooks cursor`'s
chaining shim forwards to `tma event --agent cursor --kind context --pane
"$TMUX_PANE" --payload -`, parsed by `cursor-statusline-json`.
Cursor's `statusLine` mechanism works but is absent from its documented config
reference, so it is the highest-churn channel: the parser reads only the two
confirmed numeric fields and fails safe (a missing field is ignored, never a wrong
stamp and never a clear), letting a payload change degrade to an absent gauge.
Gemini exposes only turn-granularity token counts (too coarse for a live gauge),
so its row carries no gauge. An agent with no `[telemetry.context]` block has no
gauge, and a context-gated action (e.g. `compact`) refuses `no-coverage` on it
rather than `gated`.

### Absolute token counts

Some of those channels also carry the raw number the percent is made of. Where
that number is unambiguously **the tokens currently in the context window**, tma
stamps it as `@agent_tokens` (with `@agent_tokens_at`, its evidence time) and
emits it as the `tokens` key on the JSON rows. Where it is not, nothing is
stamped: an absolute under a name that is wrong half the time is worse than no
absolute at all, and the percent is unaffected either way.

| agent | `@agent_tokens` | why |
|---|---|---|
| pi | yes — `context_usage.tokens` | the number pi divides by `contextWindow` to get the `percent` it sends; a footprint by construction |
| Cursor CLI | yes — `context_window.total_input_tokens` | the numerator of the percent tma computes, in the same `context_window` object Claude publishes; Claude's copy of that object carries `used_percentage: 78` beside `total_input_tokens: 156000` over a 200000 window, which is what pins the field's meaning |
| Claude Code | yes, from 2.1.132 — `context_window.total_input_tokens` | the numerator of the `used_percentage` Claude computes, in the object Cursor's row cites. Gated on the payload's own `version` because the pre-2.1.132 cumulative-fields bug corrupts the count and the percent together (`used_percentage: 247` beside `total_input_tokens: 494000`), and early in such a session the percent still reads plausible while the count is already wrong. Below the gate, or with no parsable version, the count stays absent and any stored one is cleared |
| Codex CLI | no | `total_token_usage` mixes the two meanings (below) |
| Gemini CLI, OpenCode | no | no context channel at all |

Codex is the interesting one. Its `token_count` record carries a
`total_token_usage` whose terms disagree about what they measure:
`input_tokens` tracks `last_token_usage.input_tokens` exactly (the per-request
context sent — a footprint), while `output_tokens` climbs past the last turn's
(a session-cumulative counter). Their sum, which the gauge divides by
`model_context_window`, is therefore a hybrid: dominated by the footprint term,
so the percent is sound, but not a quantity either "tokens in context" or
"tokens spent" describes. Until a live `token_count` reading settles it
(ACTIONS.md open question 6), Codex panes carry a gauge and no count.

`@agent_tokens` is a level, not a total, and adding it up across turns means
nothing. tma still computes no usage total and ships no pricing table.

tma also records each pane's model name in `@agent_model`, taken from the hook
registration payload where the agent sends one: Claude's `SessionStart`, Codex's
session hooks (a common `model` input field), and Cursor's `sessionStart` each
carry `model` as a top-level string, stamped last-write-wins on the
registration-class event (a pane's model changes only via the agent's own
switcher). Two context channels keep it fresh from a payload they already read:
Codex's rollout tail from its `turn_context` record, and Claude's statusline from
`model.id` (which is nested in an object, so the registration path's top-level
read cannot reach it). All of them write the same value and do not fight. Gemini,
OpenCode, and pi send no model in their hook payloads, so their panes carry no
`@agent_model`. The label feeds `tma doctor`'s recognized-model line: a model no
`[telemetry.windows]` entry names is reported, not warned about.

## Account quota, and the one cost figure

Beside the per-pane gauge, two channels publish an **account-wide** rate-limit
reading in a payload tma already receives: Claude's statusline
`rate_limits.{five_hour,seven_day,spend_limit}` and Codex's rollout
`rate_limits.{primary,secondary}`. tma stamps the window closest to exhausted as
`@agent_quota_pct` with its `@agent_quota_window` token, plus
`@agent_quota_resets_at` where the channel states one. Context is per-pane and a
`/compact` away from recoverable; the quota is shared by every pane on the
account and is not, which is what makes it the number that decides whether
starting a sixth agent is worth it.

The absence rules are the context lane's, reused verbatim: a missing
`rate_limits` block is IGNORED, never treated as a clear. It is absent for
API-key auth, absent before the agent's first API response, and dropped per
window once that window's `resets_at` passes, so a payload without one says
nothing about the account. The stored reading stays and ages via
`@agent_quota_at`.

Claude also publishes `cost.total_cost_usd`, which tma stamps as
`@agent_cost_usd`. This is the one exception to the no-cost posture and it is a
narrow one: the figure is the vendor's own live estimate for the **current
session**, stamped as stated and never recomputed. tma reports which pane, right
now. It does not aggregate cost across sessions or over time,
[`ccusage`](https://ccusage.com) is the tool that answers "how much since
Monday". Gemini, OpenCode, pi and Cursor publish no cost figure, so their panes
carry none.

Anthropic's own note applies to the number and travels with it: on a Max or Pro
subscription the session cost "isn't relevant for billing purposes", and it is an
estimate at list price rather than the bill.

## OpenCode API lane

OpenCode answers a pending permission prompt over HTTP rather than by keystroke, so
`tma act approve` / `deny` on an OpenCode pane POST the reply instead of sending a
key. The bundled `approve`/`deny` actions carry an `[api]` transport for OpenCode
(`op = "permission-reply"`, `reply = once` / `reject`); the keys path is untouched
for every other agent. `interrupt` stays keys-everywhere.

The captured request/response pair (verified against the `@opencode-ai/sdk` v2
types shipped with OpenCode 1.18.0), the evidence a new operation needs:

- **event** — `permission.asked`, `properties = { id, sessionID, permission, … }`;
  the `id` is the reply's `requestID`. The plugin forwards it as `request_id`
  (accepting `requestID` too), stamped to `@agent_permission_request` under the
  session-ownership filter and cleared on the working/idle edge or a
  `permission.replied` event.
- **endpoint** — `POST {serverUrl}/permission/{requestID}/reply` with body
  `{"reply": "once" | "always" | "reject"}`, a 2xx on success and 404 once the
  prompt is answered or withdrawn. The plugin stamps `{serverUrl}` (from its
  `PluginInput.serverUrl`) to `@agent_api_endpoint` at registration; the server
  pins its own port, so there is no hardcoded default. `[api.opencode] api_base`
  in `config.toml` is the fallback when the plugin cannot stamp it.

`tma doctor` warns on an OpenCode pane that has a pending `@agent_permission_request`
but no resolvable endpoint.

Agents with partial coverage get hybrid treatment: hook events for what they
report, screen-capture fallback for what they do not. The per-agent manifest
declares which states its hooks cover (`[hooks].covers`) and which its screen
rules can see (`[capture].visible`).

### Idle screen rules

Every bundled agent ships a positive `idle` rule, anchored on its composer chrome.
It matters most for a pane nobody wired hooks into: without one, a turn ending
leaves no claim on the screen at all, so the fold holds the previous verdict and
the pane reads `working` forever.

Two properties are shared by all six and are the reason the rules are safe:

- **The composer co-renders with the working chrome.** Every one of these anchors is
  on screen mid-turn as well — claude's `⏵⏵` mode line sits under the spinner, codex's
  `›` under `esc to interrupt`, and so on. The fold's slot order (blocked, then
  working, then idle) resolves the co-render, so the idle claim only decides anything
  once the working chrome is gone.
- **`idle` is deliberately absent from `[capture].visible` for all but claude.**
  `visible` is what lets screen evidence expire a contradicting hook claim. Chrome
  that renders mid-turn is not evidence a turn *ended*, so listing idle there would
  let working chrome decay a legitimate idle hook claim. The rule gives the fold a
  claim; it does not give it authority over a hook.

### Where each agent's `working` anchor lives

Codex and cursor anchor on a streaming footer, gemini on its own chrome. Claude used to be the
exception: it animated a braille spinner in its OSC title, and the title was cheaper to read than
the screen. That stopped at 2.1.246: the title is now a static `✳ <task>` in every state, and
since the `✳` also drives claude's idle rule, the two states became indistinguishable from the
title alone. Claude now reads the bottommost body row that starts with one of its activity glyphs.
A live spinner such as `· Actioning… (4m 16s · ↓ 16.8k tokens)` needs an ellipsis and must lack the
textual ` · done ` marker; a gerund-less `✻ Waiting for 1 background agent to finish` has its own
predicate. A completion below an old spinner therefore stops the working claim, while a newer
spinner below completion history remains working. Orange and gray ANSI colors are not signals:
capture matching strips them, and no-color terminals omit them. The title rule is retained for
older builds, at a lower priority.

This only ever mattered on capture tier. A hook-wired pane reports `working` directly — but hooks
fire on tool calls, so a long tool-free stretch (extended thinking, a single long response) leaves
capture as the only live evidence, which is where the stale `idle` showed up.

Hooks always reference a stable wrapper script (`tma-hook`), never the binary
directly: the wrapper resolves the binary at fire time and exits silently when it
is missing, so rebuilds and moves never surface as hook failures. `tma
install-hooks <agent>` writes the wiring (idempotent, additive, printing a diff
first) and `--check` verifies it. For the installer's per-agent caveats
(including the codex and gemini trust gates), see
[install-agent-hooks](../how-to/install-agent-hooks.md).

## Bundled action key sequences

The `keys` actions tma ships send a per-agent key sequence
through the `tma-tmux` write path. Each element is one `send-keys` argument with
named-key interpretation on, so `Enter`, `Escape`, and `/compact` mean what tmux
says. An agent with no cell for an action is not covered by it (the action does
not apply to that agent's panes).

| action | claude | codex | gemini |
|---|---|---|---|
| `approve` | `1` | `Enter` | — |
| `deny` | `Escape` | `Escape` | — |
| `interrupt` | `Escape` | `Escape` | `Escape` |
| `compact` | `/compact` `Enter` | — | — |

These sequences derive from each agent's captured prompt chrome (the same
captures the blocked/working screen rules anchor on): `approve` is the confirm
key of the permission prompt (Claude's `❯ 1. Yes` selection cursor, Codex's
`Press enter to confirm` footer), `deny` and `interrupt` are the reject/cancel
key (`esc`). Gemini has no captured confirm/reject key, so it carries only
`interrupt`; `compact` is Claude-only until other agents' compact commands are
captured. The sequences are provisional pending per-agent, per-version keystroke
fixtures, the same discipline detection rules get (ACTIONS.md open question 2):
where a prompt offers numbered choices, "approve" means option 1 by convention.

## Per-agent hook mappings

### Claude Code mapping

| hook | tma event | state effect |
|---|---|---|
| `SessionStart` | agent-start | pane registered, state `idle` |
| `UserPromptSubmit` | working | `working` |
| `PreToolUse` / `PostToolUse` | working (heartbeat) | `working`, refreshes liveness |
| `PermissionRequest` | blocked | `blocked` / `permission`, the moment the decision is needed |
| `Notification` (permission / idle-prompt) | blocked | `blocked` / `permission` (fallback) |
| `Notification` (usage-limit auto-continue) | rate limit | `working` / `rate_limit` while it resumes itself, `blocked` / `rate_limit` when it halts |
| `Stop` | idle | `idle` |
| `SubagentStart` / `SubagentStop` | subagent bookkeeping | append/remove session id in `@agent_subagents`; never a top-level state change |
| `SessionEnd` | agent-end | pane deregistered, options removed |

`PermissionRequest` is the claim that matters for `blocked`. It fires the moment
a tool call needs a decision, carries the pending call in its payload
(`tool_name`, `tool_input`, `tool_use_id`), and tma writes no decision back:
the hook exits 0 with nothing on stdout, so Claude Code draws its normal prompt.
The hook is deliberately **not** installed with `async: true`, the point is to
stamp the pane before the dialog draws, and a backgrounded hook would race it.

The `Notification permission_prompt|elicitation_dialog` entry stays as the
fallback for a build without `PermissionRequest`. On its own it was late: the
vendor docs gate that notification on the prompt having already waited about six
seconds, so a pane read `working` for those six seconds. The matcher runs as a
regex over the whole raw JSON payload, so it hits whether the discriminator lands
in `message` or a `notification_type` field.

Three further `Notification` types report a claude.ai usage-limit wait (Claude
Code 2.1.234 and later, where automatic continue is on by default):

| `notification_type` | claim | why |
|---|---|---|
| `quota_auto_resume_fired` | `working` / `rate_limit` | Claude Code continues the task on its own, at the reset or as soon as credits, an upgrade or a model switch frees usage. Nobody is waiting on you |
| `quota_auto_resume_stale` | `blocked` / `rate_limit` | the limit reset while the computer slept for more than about 30 minutes, so Claude Code waits for an Enter keypress instead of continuing |
| `quota_auto_resume_disabled` | `blocked` / `rate_limit` | the wait ended without continuing (`autoContinueAtUsageLimit` off, the reset moved past 24 hours, repeated limit hits, or a blocked continuation). Nothing resumes until you send a prompt |

The installed `Notification` hook carries no matcher, so every notification type
reaches `tma event` and the manifest's matchers are the whole filter. That is
what let these three be mapped without touching an installed config.

Re-verified against Claude Code 2.1.212 (2026-07-29): driving a live
Bash permission prompt fired `Notification` with `notification_type":"permission_prompt"`
and `message":"Claude needs your permission"` — unchanged, so the matcher still
covers the one blocking flow. No new blocking `notification_type` was observed, so
the matcher is **not widened** (capture-gated: nothing captured justifies it). The
idle-reminder `Notification` could not be reproduced — it fires only when a real
terminal loses focus, and a detached scratch pane (no attached client, 15+ min
idle) never triggered it; idle stays driven by the `Stop` hook regardless, so a
name change there would not affect tma.

### OpenCode mapping

A JS plugin in `~/.config/opencode/plugin/` forwards OpenCode's event-bus events
to `tma-hook opencode <token>` with the payload on stdin.

| OpenCode event | tma event | state effect |
|---|---|---|
| plugin load / `session.created` | `session-start` | pane registered, state `idle` |
| `session.status` = busy / `chat.message` / `tool.execute.before` | `user-prompt-submit` | `working` |
| `session.idle` / `session.status` = idle | `stop` | `idle` |
| `permission.asked` | `permission-required` | `blocked` |

`blocked` and `working` are visible on screen, so `[capture].visible = ["blocked",
"working"]`. The working anchor is the in-flight status row's `esc interrupt` hint,
present for the whole of a live turn and gone the moment it settles; only the text is
matched, since the `■`/`⬝` progress bar beside it animates. `idle` is anchored on
`ctrl+p commands`, the invariant tail of the composer's status row (the rest of that row
is per-pane: token count, cost, cwd). The permission dialog replaces the composer, so a
blocked screen never raises it.

The OSC title is not usable. The original audit found it static (`OpenCode`); on 1.18.18
it is state-bearing (`OC | Running <command>`) but goes stale, still reading `Running`
a minute after the turn settled, which would pin such a pane to `working` forever. That makes registration the only thing standing between a quiet pane
and `unknown`, which is why the plugin fires `session-start` at load and not just on
`session.created`: OpenCode emits `session.created` for a brand-new session only, so a
TUI waiting at its prompt and `opencode --continue` (a restored session) both used to
sit at `?` until the first message. The load-time fire carries no session id — the
`session.created` edge that follows a real new session records it.

`permission.updated` is accepted as a synonym for `permission.asked`. The
`@opencode-ai/sdk` typings shipped alongside 1.18.18 name only the former while the
1.18.18 binary contains only the latter, so the plugin answers to both and a rename
lands inert instead of silently dropping `blocked`.

Two further event-bus signals were captured live (driving `opencode serve`'s
`/event` SSE stream through the HTTP API, 2026-07-29) and deliberately left both
**observed but not wired**:

- `permission.replied` — `{sessionID, requestID, reply:"once"|"always"|"reject"}`,
  fires the instant a pending `permission.asked` is answered. It clears `blocked`,
  but it is redundant with tokens the plugin already forwards on the same edge: an
  approve (`once`/`always`) is immediately followed by `tool.execute.before`
  (⇒ `working`) and a reject/turn-end by `session.idle` (⇒ `idle`). Since the plugin
  must be live to receive `permission.replied` at all, it is live for those too, so
  wiring it adds no coverage. Wiring it correctly would also need the reject-vs-approve
  split, and only the `once` (approve) case was captured — so, capture-gated, it
  stays unwired.
- `session.deleted` — `{sessionID, info:{…full session record…}}`, fires only on an
  **explicit** session delete (API/TUI action), not on TUI/process exit (a closed
  pane's session persists on disk, undeleted). It is therefore not the session-end
  signal tma lacks: deregistering on it would remove a still-live pane, and (since
  tma's deregister is keyed on the pane, not the session id) a delete of some *other*
  background session would wrongly deregister the active one. Real OpenCode
  session-end continues to ride the pid-change / pane-close path.

### Codex mapping

Codex has two mechanisms, both wired by `tma install-hooks codex`. The `notify`
program in `<CODEX_HOME>/config.toml` is spawned on a notification with the JSON
appended as a trailing argv argument (not stdin); it fires only
`agent-turn-complete`, whose payload carries `thread-id`/`turn-id`, not a
`session_id`.

| Codex notify type | tma event | state effect |
|---|---|---|
| `agent-turn-complete` | `notify` (matcher `agent-turn-complete`) | `idle` |

The Claude-style `<CODEX_HOME>/hooks.json` (its `command` must be a string, not
an argv array) delivers one JSON payload on stdin with a real `session_id`, so
registration and the subagent guard are live here.

| Codex hooks.json event | tma event | state effect |
|---|---|---|
| `SessionStart` | agent-start | pane registered, state `idle` |
| `UserPromptSubmit` | working | `working` (fires pre-response, lands even on a failed turn) |
| `PreToolUse` / `PostToolUse` | working | `working` |
| `PermissionRequest` | blocked | `blocked`/permission (payload names the pending tool) |
| `Stop` | idle | `idle` |
| `SessionEnd` | agent-end | pane deregistered, options removed |
| `SubagentStart` / `SubagentStop` | subagent bookkeeping | append/remove session id in `@agent_subagents` |

Combined: `[hooks].covers = ["working", "idle", "blocked", "lifecycle"]`. Blocked
is also screen-carried for the daemonless, quiet-edge, and untrusted-hook cases,
so `codex.toml` ships `[capture].visible = ["working", "blocked"]`. `idle` has a
screen rule too — the `›` composer arrow in the last six rows — but stays out of
`visible` (see "Idle screen rules", above). The approval dialog numbers its options
with the same arrow, so the rule carries a `not` leaf excluding `› <n>. `.

### Gemini mapping

A Claude-shape `hooks` object in `~/.gemini/settings.json`, so `tma install-hooks
gemini` reuses the Claude JSON editor unchanged. Payloads arrive on stdin with a
real snake_case `session_id`; Gemini uses its own native event names.

| Gemini event | tma event | state effect |
|---|---|---|
| `SessionStart` (`source` = "startup") | agent-start | pane registered, state `idle` |
| `BeforeAgent` (`prompt`) | working | `working` |
| `BeforeTool` / `AfterTool` (`tool_name`/`tool_response`) | working | `working` |
| `AfterAgent` (`prompt_response`/`stop_hook_active`) | idle | `idle` (fires last in a turn) |
| `Notification` (`notification_type` = "ToolPermission") | blocked | `blocked`/`permission` |
| `SessionEnd` (`reason` = "exit") | agent-end | pane deregistered, options removed |
| `SubagentStart` / `SubagentStop` | subagent bookkeeping | wired but inert (no gemini subagent events) |

`blocked` is gated by the `ToolPermission` matcher so a future non-permission
notification cannot false-block. Coverage: `[hooks].covers = ["working", "idle",
"blocked", "lifecycle"]` and `[capture].visible = ["working", "blocked"]`. Idle
rides the `AfterAgent` hook, and additionally has a screen rule anchored on the
bottom edge of the composer box (`▀▀▀…`) within a `tail_lines(8)` window; that
chrome overlaps working, which is why idle has a rule but is not `visible` (see
"Idle screen rules", above). The window is the safety here, not the glyph: gemini
echoes each prior user message into the transcript inside an identical box, so the
frame appears on a blocked screen too, but the approval dialog replaces the
composer and the footer, leaving the bottom of the screen empty of box edges.

### Cursor mapping

User-level `~/.cursor/hooks.json` only (a project `.cursor/hooks.json` fires
nothing). The shape is cursor's own, not Claude's: `{"version": 1, "hooks":
{"<event>": [{"command": "…"}]}}`, so `tma install-hooks cursor` uses a
dedicated adapter. Payloads arrive on stdin with a real snake_case `session_id`.

| Cursor event | tma event | state effect |
|---|---|---|
| `sessionStart` (`model`, `is_background_agent`) | agent-start | pane registered, state `idle` |
| `beforeSubmitPrompt` (`prompt`) | working | `working` (interactive only; headless takes the prompt from argv) |
| `preToolUse` / `postToolUse` (`tool_name`/`tool_output`) | working | `working` |
| `postToolUseFailure` (`failure_type`/`error_message`/`is_interrupt`) | working | `working` (matcher `"is_interrupt":false`) |
| `stop` (token counts, `status`) | idle | `idle` |
| `sessionEnd` (`reason`/`final_status` = "completed") | agent-end | pane deregistered, options removed |
| `subagentStart` / `subagentStop` | not observed | absent (see below) |

`blocked` is not hook-covered: Cursor exposes no dedicated permission hook
(`beforeShellExecution` fires for approved and pending commands alike), so it
rides the approval-dialog screen rule. Coverage: `[hooks].covers = ["working",
"idle", "lifecycle"]` and `[capture].visible = ["working", "blocked"]`. `idle` has
a screen rule (outside `visible`) anchored on the composer's half-block frame plus
an `→` row — the frame rather than the hint text, because the hint reads `→ Plan,
search, build anything` on a fresh session and `→ Add a follow-up` afterwards, and
because the approval dialog reuses the same arrow glyph but not the frame.

`postToolUseFailure` (captured 2026-07-29): a `cat` of a missing file
exited non-zero and fired `postToolUseFailure` carrying `failure_type":"error"`,
`error_message`, and `is_interrupt":false`; the agent recovered and produced its
final answer, so this is a **working continuation** (the failure sibling of
`postToolUse`), not a blocked signal. It is wired to `working` behind the matcher
`"is_interrupt":false`: the user-abort variant (`is_interrupt":true`) was not
captured, so it stays unmapped rather than false-stamp `working` on a turn the
human just stopped (cursor fires no `stop` on an interrupt, so a wrong `working`
would linger). The original hypothesis that this event could distinguish a tool
failure from the screen-rule-only `blocked` inference did not hold: a failed tool
does not block, it continues, so `blocked` remains screen-carried only.

`subagentStart` / `subagentStop`: **absent** (re-verified 2026-07-29). Cursor
fires no subagent hook even when the model narrates spawning a "background
subagent" — a `-p --force` prompt asking for a parallel sub-task produced the
narration but no hook. By the Claude precedent (subagent events are
ownership-filtered bookkeeping, never state-driving) this is no coverage loss:
even if captured they would not drive pane state.

Cursor's context gauge rides a separate file from its hooks: `~/.cursor/cli-config.json`
carries a `statusLine` command (`{"type": "command", "command": "…", "padding": N}`)
that Cursor runs per turn with a JSON payload on stdin whose `context_window` object
holds `total_input_tokens` and `context_window_size`. `tma install-hooks cursor`
installs a chaining statusLine shim there (like Claude's, sharing the same machinery):
it runs the user's existing statusLine command unchanged, preserves sibling keys such
as `padding` byte-faithfully, and forwards the payload to `tma event context`. The
`cursor-statusline-json` parser computes the percent from the two fields with no window
table. This `statusLine` mechanism works but is absent from Cursor's documented config
reference (confirmed live 2026-07-29): it is the highest-churn context channel, so a
missing `context_window` is ignored rather than treated as a clear, and a payload change
degrades to an absent gauge instead of a wrong one.

### pi mapping

JS/TS modules auto-discovered from `~/.pi/agent/extensions/` subscribe with
`pi.on("<event>", handler)`. `tma install-hooks pi` drops a self-contained
extension that shells out to `tma-hook pi <event>` fire-and-forget, is inert
outside tmux, and never blocks pi. pi's events carry no session id, so the
extension reads `ctx.sessionManager.getSessionId()` and forwards `{session_id}`.

| pi event | tma event | state effect |
|---|---|---|
| `session_start` (`reason`="startup") | agent-start | pane registered, state `idle` |
| `before_agent_start` (`prompt`, `systemPrompt`, …) | working | `working` |
| `tool_execution_start` (`toolName`, `args`) | working | `working` |
| `agent_settled` (`type` only) | idle | `idle` (fires once per turn) |
| `session_shutdown` (`reason`="quit") | agent-end | pane deregistered, options removed |
| `SubagentStart` / `SubagentStop` | subagent bookkeeping | wired but inert (no pi subagent events) |

`blocked` is not a pi state: pi auto-runs tools with no per-tool permission
prompt, so there is neither a hook nor a screen rule for it. Coverage:
`[hooks].covers = ["working", "idle", "lifecycle"]` and `[capture].visible =
["working"]` (the `Working...` loader row). `idle` has a screen rule outside
`visible`, requiring both a full-width composer rule line in column 0 and the
`<pct>%/<window>k` context gauge on the status row.

On the turn-settled `agent_settled` event the extension additionally forwards pi's
`ctx.getContextUsage()` to `tma event --kind context`. pi's `ContextUsage` carries
a precomputed `percent` and an absolute `contextWindow` (both `null` right after a
`/compact` until the next assistant response, and the whole object omitted when no
model/window is available), so the `pi-context-json` parser reads the percent with
no window table; an unknown window stamps no gauge (fail-safe, not wrong).
