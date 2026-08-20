# Daemon architecture: event-driven, hook-integrated

Status: draft v2 (supersedes v1 polling-accelerator design)
Date: 2026-07-20
Cites requirement IDs from REQUIREMENTS.md. Proposes deltas at the end.

## Model

The daemon is a true event-driven hub, not a poll loop on a timer. It sits on a unix
socket and consumes three event sources, ordered by fidelity:

1. **Agent hooks** — coding agents (Claude Code first) announce their own state
   transitions via hook commands the agent runs at lifecycle points. Highest fidelity:
   the agent *tells us* it is blocked, at the moment it blocks, with zero inference.
2. **tmux control mode** — a control-mode client (`tmux -C`) receives push
   notifications for pane lifecycle and output activity. Discovery and activity
   evidence arrive as events, not `list-panes` polls.
3. **Screen capture, on demand** — the phase-1 manifest detector survives as the
   fallback for agents without hook support, and as a disambiguator. It is *triggered*
   (by an activity-quiet edge or a reconciliation sweep), never run on a timer in
   steady state.

Steady state with hook-capable agents: the daemon does nothing until an event arrives.
No subprocess spawns, no captures, no wakeups. Blocked latency drops from "within one
poll interval" to effectively immediate (hook fires → stamp → notification, well under
D3's 5 s target).

Consumers are unchanged. Stamped tmux options remain the public API (F13, F14); the
picker, status line, `tma ls`, and user tmux config read the same options regardless of
how the daemon learned the state.

## Event source 1: agent hooks

### Delivery: `tma event`

Agents don't speak our protocol; they run commands. The bridge is one subcommand,
but **agent configs never reference it directly**: they reference the wrapper script
(F28), and the wrapper owns the argument mapping (e.g. config says
`tma-hook claude notification`; the wrapper translates to the current `tma event`
invocation). This keeps `tma event`'s flag grammar internal and changeable — freezing
it before the Codex/Cursor payload audits complete would calcify a guessed interface
into thousands of settings files. What is frozen (F26) is the *wrapper's* argument
contract, which is deliberately tiny: `<agent> <event-name>`, payload on stdin.

```
tma event --agent claude --state blocked --kind notification [--payload -]   # internal shape, unstable
```

`tma event`:

- resolves its own pane from `$TMUX_PANE` (inherited by hook processes from the agent's
  environment — the pane binding comes for free, no process-tree walk needed);
- connects to the daemon socket and delivers the event, waiting up to 500 ms for a
  one-byte acknowledgement (a daemon that cannot parse the frame or lacks the agent's
  manifest NAKs, and the client falls through to a direct stamp — no event is ever
  silently lost to version skew; an unresponsive daemon costs the hook that bounded
  wait, nothing more);
- **if no daemon is running, stamps the tmux options directly and exits.**

That last point preserves the atuin-style optionality from design v1, but stronger:
*daemonless mode is also event-driven* for hook-capable agents. The daemon adds
cross-event intelligence (dedup, history, reconciliation, fallback detection), not
basic liveness.

### Claude Code mapping

Claude Code's hook set maps almost one-to-one onto our state model. The
hook-to-state mapping table is now the user-facing contract in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("Claude Code
mapping"), the single source of truth the `install` drift guard pins.

The `Notification` hook supports matchers; `permission_prompt|elicitation_dialog`
distinguishes permission prompts from idle reminders (verified in the wild by
tmux-agent's setup — resolves former open question 1). Implementation note (as built,
`tma/src/event.rs`): the manifest `matcher` is applied as a regex over the whole raw
JSON payload blob, not against Claude's native `matcher` field; for
`permission_prompt|elicitation_dialog` this hits whether the discriminator lands in
`message` or a dedicated `notification_type` field (a v1 simplification, D12).

Hooks are configured in the agent's own settings (for Claude Code:
`~/.claude/settings.json` `hooks` block) but MUST reference a stable wrapper script,
not the binary (F28, tmux-agent-sidebar's `hook.sh` pattern): the wrapper resolves the
binary at fire time and exits 0 silently when it is missing, so rebuilds and moves
never surface as hook failures to the agent. `tma install-hooks claude` writes the
block (idempotent, additive, prints a diff before applying); `tma install-hooks
--check` verifies wiring (F29). Uninstall symmetric.

**Subagent guard (F27).** Subagents share the parent's `$TMUX_PANE` and fire hooks
with foreign session ids/cwds. `tma event` carries the session id; while a pane has
live subagents, events from non-owning sessions may update subagent bookkeeping but
never pane identity or top-level state. Designed in from the start; retrofitting this
was a documented bug class in tmux-agent-sidebar.

### OpenCode mapping

OpenCode is hook-capable through its JS **plugin** bridge, not a settings block. A module
in `~/.config/opencode/plugin/` exports hooks OpenCode calls on its event bus; the tma
plugin (installed by `tma install-hooks opencode`) forwards the state-bearing events to
`tma-hook opencode <token>` with the payload on stdin. Verified live against OpenCode
1.17.15 (Homebrew) by driving a real TUI in a scratch tmux server and logging every event:

The OpenCode-event-to-state mapping table is now in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("OpenCode
mapping"), the single source of truth the `install` drift guard pins.

Two honest gaps, both by evidence: there is **no session-end event** (TUI close is a bare
process exit), so deregistration rides the universal pid-change / pane-close path (F4/F16),
not a hook; and OpenCode emits **no subagent hooks** (none observed across driven sessions,
matching tmux-agent-sidebar's finding), so the F27 guard is inert for it. On screen, only
`blocked` is reliably detectable: the OSC title is the static string `OpenCode` (no
state-bearing spinner, unlike Claude), and idle vs working differ only by a fragile
per-message `▣ … · <elapsed>` footer. So working/idle are hook-covered and `[capture].visible
= ["blocked"]` — the permission dialog (`Permission required` + `Allow once` / `Reject`,
tool-invariant across bash and edit prompts) is what the screen fallback contributes, which
is exactly the state a quiet/hookless pane most needs.

A third gap closed 2026-08-19, found by chasing a `?` on a live OpenCode pane: because
registration is the only thing between a quiet OpenCode pane and `unknown`, and OpenCode
fires `session.created` for a **brand-new** session only, a TUI waiting at its prompt and
`opencode --continue` (a restored session) both registered nothing and read `?` until the
first message. Both reproduced on 1.18.18. The plugin now fires `session-start` once at
load — session-id-less, since none exists yet; the `session.created` edge that follows a
real new session still records it, and `decide` maps `Register` to `idle`, which is the
honest state for a waiting prompt.

### Nix-wrapped agents (`.<name>-wrapped`)

Nix's `wrapProgram` moves the real binary to `.<name>-wrapped` and installs a shell wrapper
under the original name. The wrapper `exec -a "$0"`s the real one, which splits the two
identity channels: `ps -o comm` shows the wrapper's `argv[0]` (basename `opencode`, so the
subtree walk matches), but `#{pane_current_command}` shows the executable — `.opencode-wrapped`,
which macOS truncates to 15 characters (`.opencode-wrapp`). No manifest `process_names` entry
matched that, so `foreground_is_agent` was false and fold precedence 2 capped **every**
nix-installed agent pane at `unknown` — taking the screen tier down with it, since the
foreground cap precedes the screen ladder, so even the `blocked` rule never ran.

`normalize_comm` (tma-tmux) now strips the decoration after the basename: a leading `.` plus a
trailing prefix of `-wrapped`, longest match first. Requiring the leading dot keeps the rewrite
off normally-named binaries. Fixing it centrally covers every manifest at once rather than
asking each to carry a `.foo-wrapped` spelling whose truncation point is platform-dependent.

**The tty veto.** Names are the wrong shape for this question, so `foreground_is_agent` no longer
rests on one alone. `ps` now reads a `tpgid` column: on the PANE ROOT that is the kernel's own
foreground process group for the controlling terminal, so `tpgid == pgid(agent)` asks who owns the
screen without reading any executable name (`identity::foreground_owns_tty`).

It is a veto over the name comparison, not a replacement, and the reason is worth recording because
the first attempt got it wrong. A process group is coarser than a process: a child sharing its
parent's group — a launcher's real binary (cursor's `cursor-agent` → `node`), or any background job
started without job control — cannot be told apart from the parent by pgid. Replacing the name test
outright therefore broke `nested_agent_found_by_walk_when_command_shows_shell`, whose pane runs
`sleep 100000 & wait`: no job control, so the background "agent" shares the shell's group and the
tty comparison called it the foreground.

What the veto answers precisely is the converse. When the agent's group is NOT the foreground one,
the screen is definitely not the agent's, whatever tmux named — and that kills a false positive the
name cannot: the three manifests matching a bare `node` (cursor, gemini, pi) otherwise read any
unrelated foreground `node`, a dev server or a build watcher, as their own agent on screen. Missing
facts (pane root or agent absent from the `ps` snapshot, no controlling terminal) abstain.

Compare herdr, which owns its pty and so can take the honest version of both halves: full `argv`
per process (so nix is handled by falling back to `argv[0]`, never by un-mangling the name) and
`tcgetpgrp` for the foreground group. tmux hands us one truncated string instead, which is why the
name test needs the veto rather than the other way around.

### Codex mapping

Codex CLI has TWO event mechanisms, both wired by `tma install-hooks codex`: the single
`notify` program (T15) and a Claude-style `hooks.json` (audited in H4, below). The first
surface audited was the `notify` program set in `<CODEX_HOME>/config.toml`:
`notify = ["prog", "arg", ...]`. On a notification Codex **spawns** that program and appends the
notification JSON as a trailing **argv** argument (not stdin — unlike Claude's hooks and
OpenCode's plugin). `tma install-hooks codex` writes `notify = ["<tma-hook>", "codex", "notify"]`;
the `tma-hook` wrapper forwards the argv-appended payload to `tma event` on stdin (an additive
extension to the frozen two-arg wrapper contract), so `tma event`'s grammar stays uniform.
Verified live against Codex CLI 0.145.0 (Homebrew, `codex-aarch64-apple-darwin`) by configuring
`notify` to a logging program. H14 (2026-07-25) captured a **real** `agent-turn-complete` fire
(previously the payload keys were read from the binary and exercised with a constructed payload):

The notify-type and hooks.json event mapping tables are now in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("Codex mapping"),
the single source of truth the `install` drift guard pins.

The notify program fires **only** the `agent-turn-complete` type. There is **no notify for
approval/permission prompts** (a pending approval means the turn has not completed, so notify
stays silent), and none for registration or mid-turn working. The notify payload carries
`thread-id`/`turn-id`, not `session_id`, so on that channel the F27 subagent guard is inert
(Codex notify is single-pane).

**hooks.json (H4 audit + H14 turn-gated completion, verified live on 0.145.0).** Codex also ships
a Claude-style hook system: `<CODEX_HOME>/hooks.json`, shaped
`{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command": "<string>"}]}]}}` — the same
structure tma writes into Claude's `settings.json`, except the `command` MUST be a string (an
argv array is rejected: "invalid type: sequence, expected a string"). Command hooks receive one
JSON payload on **stdin** (unlike notify's argv) with `session_id` / `transcript_path` / `cwd` /
`hook_event_name` / `model`, plus `turn_id` / `permission_mode` on turn-scoped events, `prompt`
on the prompt-submit event, and `tool_name` / `tool_input` on the tool and permission events — a
real `session_id`, so registration and the F27 guard are live on this channel. The full event
vocabulary (binary + docs): SessionStart, SessionEnd, UserPromptSubmit, Stop, PreToolUse,
PostToolUse, PermissionRequest, PreCompact, PostCompact, SubagentStart, SubagentStop. H14 drove a
**completed turn** with a logging hook under a scratch CODEX_HOME, so every turn-gated event was
observed with a full payload (the H4 audit had only seen the pre/post-turn three): the prompt-scoped
event on submit, the tool events (`tool_name` `Bash`, `tool_input`/`tool_response`), the
permission event on the approval prompt (carrying `tool_name`/`tool_input`), the stop event on
turn completion (`last_assistant_message`), plus session start/end. `tma install-hooks codex`
wires the mapped set into `hooks.json` (idempotent, byte-identical round-trip, reusing the Claude
JSON editor):

The hooks.json event mapping table is in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("Codex mapping"),
alongside the notify table.

**The trust gate (load-bearing installer caveat).** A non-managed hook definition runs only
after the user reviews and trusts it inside the codex TUI (`/hooks`); until then it is
**silently skipped** — verified: the same run without `--dangerously-bypass-hook-trust`
executed nothing and printed no warning. Trust is recorded against the hook definition's hash,
so a changed wrapper path requires re-trust. Consequence: after `tma install-hooks codex` the
hooks.json wiring is INERT until the user opens codex and trusts the tma entries — the
installer prints this next step, and `--check` cannot see trust state (it lives in codex's
internal store), only the wiring. The `notify` key is a separate config value, not a hook
definition, and is not trust-gated.

Combined coverage: `working` (UserPromptSubmit / PreToolUse / PostToolUse) + `idle` (Stop +
notify agent-turn-complete) + `blocked` (PermissionRequest) + lifecycle
(SessionStart/SessionEnd) ⇒ `[hooks].covers = ["working", "idle", "blocked", "lifecycle"]`. H14
retired the notify-only differentiator framing: with PermissionRequest verified, **blocked is
hook-covered**, and it is *also* screen-carried (below) for the daemonless / quiet-edge and
untrusted-hook cases.

**Both former evidence gaps closed by H14:**

1. **`working`/`blocked` screen rules — SHIPPED.** A real turn was driven (scratch CODEX_HOME,
   `approval_policy = "untrusted"`, `sandbox_mode = "read-only"`); a write command (`touch`)
   triggered the approval prompt. `tma debug capture` recorded the streaming screen and the
   approval prompt at two widths, redacted (`codex_working_w{100,60}.txt`,
   `codex_blocked_w{100,60}.txt`). `codex.toml` now ships `[capture].visible =
   ["working", "blocked"]` with rules anchored on invariant chrome (`esc to interrupt` + the
   braille-spinner title for working; `Would you like to run the following command?` +
   `Press enter to confirm` for blocked), with the idle captures as negative fixtures. Evidence
   scope: only the command-exec approval prompt was captured; the patch/other approval variants
   are a nice-to-have fixture follow-up (the PermissionRequest hook covers them regardless).
2. **The live notify payload / argv delivery — VERIFIED.** A real `agent-turn-complete` fire was
   captured (argv-delivered, keys as tabled above); the mapping test now replays that verbatim
   payload instead of a constructed one. The hooks.json channel is likewise backed by verbatim
   2026-07-25 fires for every mapped event (paths redacted).

### Gemini mapping

Gemini CLI is hook-capable through a Claude-shape `hooks` object in `settings.json` (`gemini
hooks` subcommand, incl. `gemini hooks migrate --from-claude`). The block is shaped EXACTLY like
Claude's (`{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command": "…"}]}]}}`), so
`tma install-hooks gemini` reuses the Claude JSON editor unchanged, writing to
`~/.gemini/settings.json`. Command hooks receive one JSON payload on **stdin** with a real
`session_id` (snake_case), `transcript_path`, `cwd`, `hook_event_name`, and `timestamp`. Gemini
uses its OWN native event names (NOT the Claude vocabulary). Verified live against gemini 0.46.0
(H14 capture, 2026-07-25) by driving one tool-using turn (`echo hello-from-tma`) with a logging
hook under a project `.gemini/settings.json`, so every mapped event was observed with a full
payload:

The Gemini-event-to-state mapping table is now in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("Gemini mapping"),
the single source of truth the `install` drift guard pins.

`BeforeModel` / `AfterModel` (payload `llm_request` / `llm_response`) are **deliberately
unmapped**: they fire multiple times per turn and an `AfterModel` lands within ~16 ms of the
final `AfterAgent` in the capture, so mapping `AfterModel` → working would risk clobbering
`AfterAgent`'s idle. `BeforeAgent` + the tool events already cover `working`, so the
model-boundary events add no coverage, only race surface.

**Identity: a hook rescue plus an H20 passive title signal.** Gemini runs as `node` on BOTH tmux
read paths (a node-script launcher; H20 re-verified both `#{pane_current_command}` and the `ps`
comm are `node` on 0.46.0), so `process_names = ["gemini"]` would never match and `["node"]` alone
would false-match every Node app. The strongest path is still the hook: the `SessionStart` hook
stamps `@agent_session` and marks the pane an agent pane; AD3 registration is **sticky**, honored
on later reads even with no walkable agent process, so once a hook-wired gemini pane starts, tma
tracks it for the whole session despite the `node` comm. **H20 adds a passive title signal** for
panes with no hooks wired: gemini's OSC pane title encodes its state as `<glyph>  <phrase> (<cwd>)`
and is distinct in every state — idle `◇  Ready`, working `✦  Working…`, blocked `✋  Action
Required` — so `process_names = ["node"]` narrowed by those three `title_patterns` (the H16
secondary signal) safely identifies a hookless gemini pane. Because every state carries a matching
title, the pattern set covers all states; the H16 pid-anchored stickiness only carries brief
uncovered transients (the startup window before the title is set, any unobserved sub-phrase). This
also makes the screen rules below reachable passively — before H20 a gemini pane had to be
hook-wired to be seen at all.

**Both H15 gaps closed by H19 evidence (2026-07-26).** (1) **`blocked` is now hook-covered.** The
H14 turn auto-approved (a read-only command), so it never hit an approval prompt. H19 drove a WRITE
command (`rm -rf …`) under `--approval-mode default` in an isolated HOME (read-only commands like
`uname`/`echo` auto-approve at gemini's policy priority 50, so only a write/destructive root command
prompts). When the "Allow execution of [Shell]?" dialog appeared, gemini fired a `Notification` hook
with `notification_type` "ToolPermission" BEFORE the prompt was answered — confirmed in three
separate drives (order: `BeforeTool` → `Notification`, `AfterAgent` not yet fired, i.e. the turn
paused at the prompt). gemini's source agrees: `notifyHooks` fires only when a tool confirmation is
required and is skipped when the command auto-approves. So `Notification`/ToolPermission ⇒
`blocked`/`permission`, gated by a matcher so a future non-permission notification cannot false-block
(`ToolPermission` is the only type 0.46.0 emits). (2) **Screen rules now ship.** The working
(thinking footer `esc to cancel`) and blocked (`Allow execution of [Shell]?` + `Allow once`) screens
were captured live at two widths, so `[capture].visible = ["working", "blocked"]` with real rules
scanning `Region::Visible`. idle stays screen-INVISIBLE — its composer chrome overlaps working, so
letting it into `visible` would mean working chrome could decay an idle hook claim — but as of
batch B it does carry a positive idle RULE (`Type your message or @path/to/file`), so a pane whose
hooks never fired is no longer pinned at `working` after a turn. The two are separate switches:
`[capture].visible` grants authority over a hook claim; a `[[rules]]` entry only supplies a claim
for the fold to rank (D10).

**The folder-trust gate (installer caveat).** Gemini gates local config (hooks/MCP/skills) behind
a per-FOLDER trust prompt ("Trusting a folder allows Gemini CLI to load its local configurations,
including … hooks …"). Until the working dir is trusted the hooks do not load; once trusted they
fire and gemini reports success; there is no separate silent per-hook trust gate like codex's
`/hooks`. `tma install-hooks gemini` prints this next step.

Coverage: `working` (BeforeAgent + tool events) + `idle` (AfterAgent) + `blocked`
(Notification/ToolPermission, H19) + lifecycle (SessionStart/SessionEnd) ⇒ `[hooks].covers =
["working", "idle", "blocked", "lifecycle"]`, hook-carried AND screen-carried.

### Cursor mapping

Cursor CLI is hook-capable through **USER-level** `~/.cursor/hooks.json` (H16 overturned H14's
"hookless" finding, which had tested only a *project* `.cursor/hooks.json`). The shape is cursor's
OWN, NOT the Claude/gemini shape: `{"version": 1, "hooks": {"<event>": [{"command": "…"}]}}` —
lowercase event names, a flat `{command}` entry (no nested `hooks` array, no `type`). So
`tma install-hooks cursor` uses a dedicated `CursorAdapter` (not the Claude JSON editor), writing
`~/.cursor/hooks.json` and preserving unrelated user hooks. Command hooks receive one JSON payload
on **stdin** with a real snake_case `session_id` (so `parse_session_id` reads it and the generic
`map_event` resolves the lowercase events with no cursor-specific parser code). Verified live
against cursor 2026.07.23-e383d2b (H16, 2026-07-26) in the real authenticated HOME — an **isolated
HOME cannot authenticate** (cursor's login state binds to the real HOME beyond config + keychain;
empirically confirmed across three configurations), so the `hooks.json` was created transiently and
removed after — driving both a headless `-p` and interactive turns with a logging hook:

The Cursor-event-to-state mapping table is now in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("Cursor mapping").

Deliberately unmapped (fire, but add no coverage): `afterAgentThought` (multi-fire reasoning),
`beforeShellExecution`/`afterShellExecution` (shell audit — redundant with the tool events, and
NOT a blocked signal: they fire for auto-approved AND approval-pending commands, and the hook's
permission verdict is observer-only, the CLI's allowlist still gates), `postToolUseFailure`,
`preCompact`. `afterAgentResponse` was wired but never observed firing, so it is not mapped either.

**The identity rescue + the title secondary signal.** Cursor runs as `node`
(`#{pane_current_command}`) / `agent` (`ps -o comm`), never `cursor-agent`, both generic — so
`process_names` alone is unsafe. Two paths make it safe: (1) the `sessionStart` hook stamps
`@agent_session` and marks the pane (sticky AD3 registration, like gemini — the primary path for
hook-wired panes); (2) the H16 identity `title_patterns` narrow the generic comms — a `node`/`agent`
pane is cursor only when `#{pane_title}` matches `^Cursor Agent$`, held across cursor's title
flicker (idle `Cursor Agent` → tool-name during actions) by the pid-anchored `@tma_title_match_pid`
stickiness (AD3/AD4). The title path is the fallback for unregistered panes; a hook registration
bypasses the title gate entirely.

**One honest gap: `blocked` is NOT hook-covered.** Cursor exposes no dedicated permission/approval
hook — the on-screen approval ("Not in allowlist: …") is signaled by no distinct event
(`beforeShellExecution` fires for approved and pending commands alike). So `blocked` rides a
**screen rule** (the approval-dialog chrome, captured live at two widths), not a hook, and
`[hooks].covers` omits it. Coverage: `working` + `idle` + lifecycle via hooks; `blocked` +
`working` via screen rules ⇒ `[hooks].covers = ["working", "idle", "lifecycle"]`,
`[capture].visible = ["working", "blocked"]`.

### pi mapping

pi (the earendil-works coding agent) is hook-capable through its **extension system**, not a JSON
hook block: JS/TS modules auto-discovered from `~/.pi/agent/extensions/` (or a project
`.pi/extensions/`) export `default function (pi)` and subscribe with `pi.on("<event>", handler)`.
So `tma install-hooks pi` uses a dedicated `PiAdapter` that drops a self-contained JS extension
(`assets/pi-extension.js`, the pi analog of OpenCode's plugin — both reuse the generic JS-bridge
helpers, proving X1) into the extensions dir; it shells out to `tma-hook pi <event>`
fire-and-forget, is inert outside tmux (`$TMUX_PANE` absent ⇒ registers nothing), and never blocks
pi. Verified live against pi 0.82.1 (H17, 2026-07-26) under an **isolated `PI_CODING_AGENT_DIR`** (a
copy of the user's config; pi authenticates from a file-based API key in `auth.json`, not a
keychain, so the isolated dir authenticated cleanly and the real `~/.pi/agent` was never touched),
driving trivial, tool-using, and trust-prompt turns with a logging extension — the events fired
identically in print (`-p`) and interactive (tui) modes:

The pi-event-to-state mapping table is now in
[reference/agent-coverage.md](../reference/agent-coverage.md) ("pi mapping"), the
single source of truth the `install` drift guard pins.

Deliberately unmapped (fire, but add no coverage): `agent_start` (redundant with
`before_agent_start`), `turn_start`/`turn_end` + `message_start`/`message_end` (per-message noise),
`tool_execution_end`/`tool_call`/`tool_result` (redundant with `tool_execution_start` for
`working`; `agent_settled` is the clean idle landing), `model_select`/`input`/`user_bash`.

**The session id is injected, not carried.** pi's event objects carry **no** session id, and
`PI_SESSION_ID` is set only in bash-tool **child** processes, not pi itself — so the extension reads
`ctx.sessionManager.getSessionId()` and forwards `{session_id}` (snake_case) to `tma event`, which
`parse_session_id` reads exactly like gemini/cursor.

**The identity rescue + the title secondary signal.** pi runs as `node` (`#{pane_current_command}`)
/ `pi` (`ps -o comm`, pi sets its own process title), never `pi` on the cheap read path — `node` is
generic. Two paths make it safe: (1) the `session_start` hook stamps `@agent_session` (sticky AD3
registration, the primary path for hook-wired panes); (2) the H16 `title_patterns` narrow the
generic comms — a `node`/`pi` pane is pi only when `#{pane_title}` matches `^π ` (pi's stable
terminal title `π - <cwd-basename>`, verified to stay put even during a working turn, so no flicker
hold is needed although the `@tma_title_match_pid` machinery still applies). Honest gap: during pi's
pre-trust **startup** the title is still the terminal default and `session_start` has not fired, so
a pi pane at the "Trust project folder?" prompt is not yet identifiable — it becomes identifiable
once the TUI loads (title set) or the session registers.

**`blocked` is not a pi state (honest gap, stronger than cursor's).** pi auto-runs tools with no
per-tool permission prompt — tool gating is an *extension* concern (`tool_call` can return
`{block:true}`), not a built-in blocked state. The only approval-class native event is
`project_trust` (a one-time startup trust gate), and it fires *before* the title is set and *before*
`session_start`, so it could never fire on an identified pi pane. So `blocked` gets neither a hook
nor a screen rule. Coverage: `working` + `idle` + lifecycle via hooks; `working` via a screen rule
(the `Working...` loader row, captured live at two widths) ⇒ `[hooks].covers = ["working", "idle",
"lifecycle"]`, `[capture].visible = ["working"]`.

### Other agents

Hook/notify support varies and must be verified per agent against real behavior — the
same evidence-first rule as manifests (D10 applies to hook mappings too). The
per-agent coverage matrix (mechanism, expected coverage, verification status) is
now in [reference/agent-coverage.md](../reference/agent-coverage.md) ("Coverage
matrix").

Agents with partial coverage get hybrid treatment: hook events for what they report,
capture fallback for what they don't (e.g. an agent that reports turn-complete but not
permission prompts still needs screen evidence for `blocked`). Agents with no
mechanism at all run entirely on the fallback detector. The per-agent manifest
declares which states its hooks cover, so the engine knows what the fallback must
still watch for.

### Trust and arbitration

Hook events are self-reported by a cooperating local process. Within the state engine
they are the highest-ranked evidence — above screen chrome — because they are direct
statements rather than inference. Revised arbitration order (amends F8):

1. hook event (fresh, from a registered agent pane)
2. visible blocker chrome on the live viewport
3. visible working chrome ⇒ working
4. visible idle chrome ⇒ idle
5. hold previous state / `unknown`

A hook event's authority decays per F8's coverage-aware rules: process evidence (pid
gone, e.g. died without `SessionEnd`) expires any hook claim; screen evidence expires
hook claims only for capture-visible states. Carve-out, stated identically in F8 and
AD4: **visible blocker chrome overrides a `working`/`idle` hook claim iff the stamped
`@agent_evidence_at` predates the capture's timestamp** — evidence ordering, so a
hook claim newer than the capture wins (answered-prompt race) and an older one loses
with no decay wait. Socket lives in `$XDG_RUNTIME_DIR` (fallback `$TMPDIR`) with
0700 directory perms, keyed per tmux server (hash of `#{socket_path}`); events are
accepted from the local user only. Spoofing is a local-user-only, low-stakes concern
(worst case: wrong glyph in your own status line).

Connection handling is bounded so no client can stall the daemon: each accepted
connection must deliver its *whole* frame within a fixed deadline (2 s), enforced as a
single wall-clock budget across all reads rather than a per-read timeout — so a client
that connects then dribbles one byte at a time holds the serial accept loop for at most
that deadline, never indefinitely. On the *client* side, the hook connects to the socket
non-blocking: if the accept backlog is momentarily full (reachable while the daemon is
mid-startup, since its up-to-2 s control-mode probe runs before it begins accepting), the
hook direct-stamps immediately rather than blocking the agent (never-block-tmux). The
diagnostic paths (`tma doctor`, `tma reload`) keep a plain blocking connect — they are
human-invoked, and a bounded connect would misreport a merely slow-to-accept daemon.

## Event source 2: tmux control mode

The daemon holds one long-lived `tmux -C` client (revises N7, which banned control
mode in v1 — that ban stands for the *CLI one-shots*, which stay subprocess-based; the
daemon is exactly the component a persistent connection is for).

**Scoping reality (round-2 empirical review, verified on tmux 3.6a): BOTH `%output`
and `refresh-client -B` subscriptions are session-scoped.** The `%*`/`@*` subscribe
targets mean "all panes/windows *in the attached session*"; explicit cross-session
pane/window IDs subscribe successfully but never deliver `%subscription-changed` —
the command succeeds while being silently useless, so no command-success probe can
detect it (N10: probes must test behavior). The earlier claim that `-B` covers the
whole server was false.

Revised design: **one control client per session.** The daemon maintains a small
pool of `tmux -C` clients, one attached per session, each subscribed to its own
session's pane activity (and receiving that session's `%output`). Pool membership
tracks `%sessions-changed`/session lifecycle events, which any single client does
receive server-wide. Cost: one idle client process per session — sessions are
typically few (one per project), and idle control clients consume no measurable CPU.
Latency claims for hookless quiet-edge detection hold under this design. Degrade
path if control-mode behavior probing fails at the floor: reconciliation sweep at a
faster interval, with the latency table's numbers restated accordingly, never
silently kept.

Control mode provides, as push:

- **pane lifecycle** — window/pane created, closed, renamed (server-wide): discovery
  without `list-panes` polling; a closed pane immediately clears state (F16) even
  when the agent died without `SessionEnd`.
- **activity** — via `-B` subscriptions as above; per-pane granularity without
  capture.

Activity events are rate-limited into an edge signal (active/quiet per pane, with a
quiet threshold) rather than processed per-event, bounding daemon CPU (N4).

## Fallback detection (capture on demand)

The phase-1 manifest detector is unchanged as a library (D7) but is now *triggered*:

- **quiet edge** — a hookless agent pane goes from active to quiet: capture once,
  classify (working chrome? blocker? idle prompt?). This is where blocked is caught
  for hookless agents: permission prompts stop output, so the quiet edge is precisely
  the moment to look.
- **contradiction** — hook state says `working` but the pane has been quiet past a
  threshold: capture to confirm or correct.
- **reconciliation sweep** — see below.

For a fleet of hook-capable agents, captures approach zero. For hookless agents, the
capture rate is bounded by their output burst rate, still below timed polling.

## Reconciliation: why a slow sweep survives

Pure event-driving fails open: a missed hook (agent killed -9, hook misconfigured,
daemon restarted mid-session), a dropped control-mode connection, or an agent started
before the daemon leave state wrong until the next event that may never come. The
daemon therefore runs a low-frequency reconciliation sweep — the full phase-1 poll
cycle — every 30–60 s (configurable):

- discovers agents that never announced themselves (hookless, or hook-install missing);
- clears state for processes that died silently;
- corrects any hook-state drift via capture evidence.

This is self-healing, not state-driving: at 30–60 s it is ~30× cheaper than the v1
1–2 s poll loop and its latency only bounds the *repair* of anomalies, not normal
detection. Design invariant: **events drive state; the sweep repairs it.**

## Notification dedup

The episode marker is the persisted `@agent_notified_at` pane option, written only by
whichever process fires the notification. A notifier fires iff it records a new
notifiable transition (a `notify.on` trigger: blocked always by default, the
working→idle done completion when opted in — H2) AND `@agent_notified_at` predates
the current `@agent_since`. Because `@agent_since` is write-once *per state*, the
marker dedups per state-run, not per agent episode: a blocked-then-done sequence
re-arms at the state transition and fires once for each configured trigger.
Rationale (adversarial review killed two earlier designs): keying on
`(pane_id, @agent_since)` fails because `@agent_since` is observation-time and
producer-dependent — the key mutated mid-episode and re-fired (fixed separately by
making `@agent_since` write-once, AD4); and the attention flag cannot double as the
marker because focus-then-leave-unanswered clears attention while the episode
continues. The marker lives in tmux options, not daemon memory: it is the one dedup
record whose loss on daemon restart would violate F22's MUST, so it is exempt from
the "daemon memory is disposable" rule (AD4).

The marker is written as `max(now, @agent_since)`, not the bare fire time, at both write
sites (the daemon's `fire_for` and the daemonless `tma event` direct-fire). Under a monotone
clock `now >= since`, so this is exactly `now` and dedup is unchanged; the clamp only matters
under a backward wall-clock step that would otherwise land `now` *before* the episode's own
`since`, writing a marker that predates the episode it dedups (`notified_at < since`) and so
reads as not-yet-notified and re-fires. Clamping forward to `since` keeps the marker at-or-after
the episode start so the per-state-run dedup holds. This is the forward-clamp counterpart to the
re-accepted future-marker behaviour below: it never *lowers* a marker already past `now`, so it
cannot re-enable a suppressed future marker (no double-fire across a forward step).

The dispatch itself is fire-and-forget: the notify command runs as a detached child the
daemon reaps on later passes. In-flight children are capped (N4) — a pathological hung
sink cannot accumulate handles unbounded. At the cap the oldest handle (most likely the
hung one) is killed and reaped to make room, so a saturated ring never leaks a defunct
child; the marker is already committed before the fire, so bounding or displacing a child
never affects dedup.

The `notify.command` hook receives the notification two ways (read whichever): the `TMA_*`
env vars (`TMA_AGENT`, `TMA_PANE`, `TMA_STATE`, `TMA_LOCATOR`, `TMA_TITLE`, plus `TMA_DETAIL`
/ `TMA_SESSION` when present), and a compact JSON object on **stdin**. The JSON is a stable
contract carrying exactly these top-level keys, in order: `schema` (currently `1`), `agent`,
`pane`, `state` (the transition word `blocked` / `done`, not the raw landing token), `detail`,
`session`, `locator`, `title` — `detail`/`session` are `null` when absent, and only metadata
is ever emitted (never captured screen content, N9). The key set is additive under `schema: 1`
and pinned by a drift test (`payload_json_pins_the_exact_key_set`); a breaking rename/removal
bumps `schema` so a hook can branch on the version rather than guess.

## `tma wait` push subscriptions (H12)

`tma wait` (the blocking scripting primitive, F31/H10) is a cycle-authoritative poll loop: it
runs one `cycle::run_cycle` on entry (the immediate level check), then re-cycles on a ~1 s tick,
returning as soon as a cycle OBSERVES the targeted pane in a target state. H12 adds a
latency-only upgrade, the daemon can WAKE a waiter early, without changing what `wait`
observes or the exit contract (0 observed / 124 timeout / 3 targeted-pane vanished / 2 usage / 1
generic, all frozen). The exit-3 "targeted pane" is a `--pane` or a pinned `--agent` (H18b: `--agent`
pins to the first pane it observes and then behaves as `--pane`, vanish included; `--any` never pins,
so it keeps waiting on a vanish).

The mechanism rides the same per-server socket. A waiter connects (the non-blocking `hook_connect`
probe) and sends a bodiless `TMAS` subscribe frame, distinct from the `TMA1` event frame; the
daemon classifies the two with one leading read. On accepting it writes one `SUB_ACK` byte and
retains the connection in a small subscriber set (bounded, N4). Thereafter, on every serve-loop
iteration that did state-affecting work (a hook event applied, a control-mode activity or lifecycle
event, a quiet-edge capture, a reconcile, or a sweep), the daemon writes one `PUSH` byte to each
subscriber under the never-wait write discipline: the write is non-blocking, and a subscriber whose
buffer is full or whose peer is gone is dropped, never allowed to stall the loop (the same rule the
control-client writes follow).

**The push is a WAKE HINT, not evidence.** A waiter that receives one re-runs its own full
`run_cycle` and decides from THAT cycle, never from the push, so a spurious or stale push costs the
waiter one extra cycle, never a wrong exit. This keeps the daemon strictly additive (AD5): every
degrade path falls back to the H10 poll loop and is never an error. No daemon, a daemon that dies or
restarts mid-wait (the subscription EOFs), or a pre-H12 daemon that NAKs the unknown subscribe magic
(a non-`SUB_ACK` byte / EOF) all leave the waiter polling. `--timeout` is honored client-side in both
modes. Vanish detection (exit 3) does NOT rely on pushes: a `--pane` close is a control-mode
lifecycle event the daemon pushes on, and push mode also runs a slower fallback cycle as a belt, so
the waiter's own `list-panes` existence check runs exactly as it does under polling.

## Surface subscribe stream (H20)

**Question.** External read-path surfaces (a Stream Deck plugin, a dashboard, any repainting
consumer) poll `ls --json` at ~1 Hz. ACT7 (ACTIONS.md) declined a daemon hub but conceded the
one thing a daemon genuinely buys such surfaces: push latency on the read path. ACTIONS.md
open question 4 defers that design here. How does a surface get pushed repaints when the
daemon is present, without becoming a daemon client in the hub sense?

**Options.** (a) The surface speaks the socket protocol directly: connect, send `TMAS`,
re-poll `ls --json` on each `PUSH` byte. This makes the wire protocol a public contract
(version skew forever), and every plugin re-implements the probe, fallback, and reconnect
logic H12 already wrote once. (b) The daemon pushes state rows to subscribers as payload.
That is a second evidence path: H12's rule is that a push is a wake hint and never evidence,
and payload push would make the daemon authoritative over what a surface displays, inverting
the strictly-additive tier (AD5). (c) A streaming CLI verb, `tma subscribe --json`: a
long-running process that emits one complete `ls --json` schema-1 document per line, riding
the H12 subscription (hook_connect probe, `TMAS` frame, wake, then its own cycle) when a
daemon is present, and degrading to its own poll loop (`--interval`, default 1 s) when not.
The emitted contract is identical in both modes.

**Decision.** (c). Surfaces stay CLI consumers (ACT7): a deck plugin spawns
`tma subscribe --json` instead of running a polling timer, re-renders on each line, and still
holds no connection to anything but the `tma` binary. Pinned:

- Each emission is a complete `ls --json` document on one line, snapshot semantics, no
  diffs. A dropped or missed line costs staleness until the next emission, never a wrong
  accumulated state, and snapshot output is what keeps the push and poll modes
  contract-identical by construction. Row-level diffing is the consumer's job if it wants
  one.
- Wake handling follows H12 verbatim: a `PUSH` is a hint, the subscriber runs its own
  cycle and emits what that cycle observed, never what the daemon said. Hints arriving
  inside a short debounce window (100 ms) coalesce into one emission.
- Push mode keeps the slow fallback cycle H12 waiters run as a belt, so a wedged
  subscription that never EOFs cannot freeze the stream; the belt cycle emits only when it
  observes a change from the last emitted document.
- Degrade is never an error and is invisible except as latency: no daemon, a daemon dying
  mid-stream (the subscription EOFs), or a pre-H12 daemon NAKing the subscribe magic all
  drop the process to its poll loop, and a periodic re-probe picks a returning daemon back
  up. In poll mode the process emits every `--interval` unconditionally, which is exactly
  the contract a self-polling consumer had before.
- No heartbeat mechanism: process death is the liveness signal. The consumer spawned the
  process and owns its stdout; EOF means dead, respawn. A quiet system otherwise emits
  nothing in push mode.
- Placement: the verb lives in `tma`; the stream loop lives in `tma-runtime` beside the
  existing H12 client machinery; the daemon changes not at all, because the H12 subscriber
  set already serves any `TMAS` client. This is the whole point of the wake-hint design
  paying off twice.

Relationship to actions (ACTIONS.md): a deck re-runs `tma act --list --json --pane %N` for
its visible panes when an emission shows a pane changed; fireability rides the same wake.
There is no separate action-fireability stream in v1.

**Revisit if** consumers demonstrably need row-level diffs (bandwidth on very large
sessions) or a pushed fireability stream; either extends the emission contract additively
under the schema rule rather than reopening the hub question.

## Timing corners: resolved and re-accepted

The store keeps epoch **milliseconds** (AD4), not seconds. That resolution change closed the
four sub-second races the earlier epoch-second clock only bounded (items 1, 3, 4, 6 below);
the two that remain are re-accepted here with the reason each is the *chosen* behaviour, not a
discovered one. The tmux `-F` guards do the ms arithmetic on 13-digit values (`e|<=`, `e|<`,
`e|/`; verified on 3.6a — no new floor over the existing `set -F` requirement, N10), and a
store still holding legacy 10-digit epoch-seconds stamps is normalized on read (a nonzero
`*_at` below 10^12 is scaled `×1000`) so an in-place upgrade never mixes units in the fold.

**Resolved**

- **Sub-second episode collision — resolved: millisecond `@agent_since`.** Two distinct
  blocked episodes opening inside one wall-clock second now get distinct write-once
  `@agent_since` values, so the second episode's marker strictly predates its own `since`
  and the F22 notification fires. (Only a true same-*millisecond* pair of episodes would
  still collide — a human answering a prompt and the agent re-blocking inside 1 ms — which
  is not physically reachable.)
- **Daemonless concurrent hooks resolve by finish order — resolved: hook-arbitration guard.**
  A daemonless hook stamp no longer writes unconditionally; its server-side guard suppresses
  the write iff the store already holds a *strictly newer* hook claim (`@agent_source` is
  `hook` and its `@agent_evidence_at` postdates this event's time). Two racing `tma event`
  processes therefore resolve by evidence time, not by which process finishes last — an
  older-fired event can no longer clobber a newer one regardless of scheduling. Only
  meaningful at ms resolution (at second resolution near-simultaneous hooks tie); against a
  legacy seconds stamp the guard fails safe (a 13-digit ms time never predates a 10-digit
  stored value, so it never wrongly suppresses).
- **Carve-out tie — resolved: millisecond evidence ordering.** The blocker-chrome carve-out
  compares the capture time against the hook's `@agent_evidence_at`; at ms resolution a
  capture and a hook that landed in the same *second* now carry distinct sub-second times, so
  the tie that previously resolved in the hook's favour (delaying a genuinely simultaneous
  block one cycle) is broken by ordering. A true same-*millisecond* tie still holds the hook,
  which remains the correct answered-prompt-safe default (D1) for that unreachable case.
- **Ack-timeout duplicate stamp — resolved (state + notification): hook-arbitration guard +
  read-back fire gate.** A client that times out on the daemon ack and direct-stamps its
  (older) event can no longer revert a newer state the daemon already applied: the same
  arbitration guard suppresses the late write when the store's `@agent_evidence_at` is already
  newer. The notification half is now closed too (H7a): the daemonless direct-fire commits the
  `@agent_notified_at` marker under that same guard and reads it back, firing (`display-message`
  toast and the F23 hook command) only when the marker landed on this event's time — i.e. only
  when its stamp won. A losing direct-stamp fires nothing and clobbers no marker, so it can no
  longer produce even the cosmetic one-shot toast, and no future episode is affected.

**Re-accepted**

- **Backward wall-clock steps — partially resolved; notify half re-accepted.** The freshness
  read now treats a `@agent_stamped_at` in the *future* relative to `now` as **stale**, not
  fresh (`stamped_at <= now && now - stamped_at < window`), so a backward NTP step no longer
  makes every stamp read fresh forever — the pane re-stamps against the corrected clock on the
  next cycle. The notification half stays re-accepted: a `@agent_notified_at` left in the
  future by the step keeps suppressing until the clock passes it. Clamping it to re-enable
  firing is the wrong trade — the same future marker is what a just-fired notification writes,
  so a re-fire-on-future rule would *double-fire* across the step, and F22 mandates missing a
  notification over double-firing. The window is bounded by the step size and the daemon's own
  cadence is monotonic and unaffected.
- **Same-millisecond marker tie — re-accepted.** The per-state-run dedup keys on
  `@agent_notified_at` strictly predating `@agent_since`, both at ms resolution. A blocked
  episode that opens and is answered-then-reopened inside the *same millisecond* would share one
  `@agent_since`, so the second run's marker ties (equal, not strictly less) and does not re-fire.
  This is physically unreachable (a human answering a prompt and the agent re-blocking within
  1 ms) and re-accepted as the correct answered-prompt-safe default rather than widened to a
  `>=`-fires rule, which would double-fire far more often than it would rescue a real reblock.
- **Notify read-back error drops the fire — re-accepted.** The daemonless direct-fire gates on
  reading `@agent_notified_at` back after its guarded commit, firing only when the marker landed
  on this event's time (so a losing arbitration writer fires nothing). If that read-back itself
  errors transiently (a tmux hiccup between the committed write and the read), the fire is
  skipped even though the marker committed — F22 mandates missing a notification over risking a
  duplicate, and the marker is now in place so no later pass re-fires. Accepted, not worked
  around: adding a retry would trade the guaranteed at-most-once for a rare at-least-once.
- **Window summary lag — resolved on the hook path (H7a); cycle path unchanged.** A window's
  `@agent_summary` in-chain hint counts the state each producer *intended* to write, which can
  diverge from a guard-suppressed per-pane store for about one cycle. On the poll cycle this is
  cosmetic and self-heals: the end-of-cycle reconciler recomputes each window from stored
  membership and converges it deterministically, so its in-chain hint stays unguarded (the
  common case *passes* the guard, and guarding the hint from the stored state would under-report
  every ordinary stamp to fix the rare race). The hook path has no reconciler, so it now writes
  its rollup under the *same* suppression guard as the pane stamp: the summary commits iff the
  stamp commits, holding the winning claim's stored rollup when a losing event is suppressed.
  This closes the hook-path divergence without the common-case regression, because the guard —
  not the caller — chooses stored-vs-intended per write.

## Middle tier: signal nudges without a daemon

tmux-agent-sidebar demonstrates a cheap latency upgrade below "full daemon": tmux hooks
(`after-select-pane`, `after-select-window` — these fire unconditionally; do NOT use
`pane-focus-in`, which is gated on `focus-events` default-off, F30) send `SIGUSR1`
to a resident surface process whose pid is advertised in a tmux option. For tma,
`tma watch` advertises its pid in `@tma_watch_pid` **set on its own pane**
(never server scope: a server-scoped pid outlives the process, and once the pid is
recycled every pane change signals — and by default kills — an arbitrary process;
pane scope dies with the pane, sidebar's proven pattern) and handles `SIGUSR1` as
"refresh now". Pane scope *narrows* the pid-recycle kill hazard rather than closing it:
if the watcher exits without unsetting the option while its pane lives on (e.g. a `kill
-9` that skips the Drop cleanup), the stale pid lingers until the pane closes, so a pid
recycled in that gap could receive a stray `SIGUSR1`. Local-user-only and low-stakes, and
the `pid > 0` filter still blocks the process-group fan-out. `tma reload`'s SIGHUP sender
narrows its equivalent window by re-probing the socket immediately before the kill. Shipped in H6b: the receive handler (`take_nudge`) and the
`nudge_watchers` sender both live in `tma_runtime::nudge` — both need `libc`, which
the `tma-ui` display crate deliberately lacks (RD4). The always-on
`after-select-pane`/`after-select-window` hook already runs `tma clear-attention` on
every focus change; that same handler now also walks panes for `@tma_watch_pid`
(`pid > 0` only) and `SIGUSR1`s each advertiser, so every resident watcher refreshes
within one input-poll tick (~200 ms worst case) of a focus change. The hook walks
panes for the option, as sidebar does.

**The popup picker is excluded from the nudge scheme** (round-2 empirical finding):
a `display-popup -E` process runs in a hidden internal pane that `list-panes -a`
never enumerates, so no hook walk can find its advertised option. The picker's 1 s
self-refresh is its only and sufficient liveness mechanism; the docs must not claim
nudge coverage for it.

Combined with `tma event` direct-stamping, this gives hook-latency state and instant
watcher refresh with no daemon at all. The daemon remains the tier above for
fallback detection, reconciliation, history, and notifications.

## Daemonless operation

Unchanged principle: every surface works without the daemon (goal 5, phasing below).

- **Hook-capable agents, no daemon** — `tma event` direct-stamps. State in tmux
  options is event-fresh even with no daemon. `tma status` / picker read stamps.
  What's missing: notification dispatch (no resident process to fire it — opt-in
  `notify.from_event = true` lets `tma event` itself fire the notification hook
  before exiting, for whichever transitions `notify.on` selects: blocked by
  default, plus the working→idle done completion with `on = ["blocked", "done"]`,
  H2), transition history, reconciliation, and fallback detection for hookless
  agents.
- **Hookless agents, no daemon** — phase-1 behavior: any `tma` invocation runs a poll
  cycle when stamps are stale (per-pane `@agent_stamped_at`; the server-wide
  `@tma_last_poll` is a hint only — a server-scoped marker cannot represent freshness
  in a mixed fleet, where hook-active panes would keep a dead hookless pane's stale
  `blocked` looking fresh forever). The status line provides the ambient cadence and
  is documented as *required* for ambient surfaces, not optional (ARCHITECTURE AD5).
  This path also serves as the always-available floor when hooks are misconfigured.
  Stampede guard: a producer skips its cycle when another producer stamped in the
  same second (`@tma_last_poll` is epoch ms like every stamp, but the guard buckets it
  and `now` to seconds before comparing, keeping its second resolution; checked at cycle
  start) — bounds duplicate work when several one-shots fire together.

| capability | no daemon | daemon |
|---|---|---|
| blocked latency (hook agents) | immediate (direct stamp) | immediate + notification |
| blocked latency (hookless) | ≤ status-interval | quiet-edge capture, ~seconds when control-mode `%output` is available; degrades to sweep interval (30–60 s) otherwise |
| notifications | opt-in from `tma event` (hook path only), `notify.on` trigger set | full (F22/H2), deduped, `notify.on` trigger set on every detection path |
| discovery of silent/hookless agents | on-invocation poll | control-mode events + sweep |
| self-healing after missed events | on-invocation poll | 30–60 s sweep |
| transition history | none | bounded in-daemon (N4) |

## Setup surface

```
tma install-hooks claude      # writes hook config into the agent's settings (diff + confirm)
tma install-hooks --check     # verifies hook wiring for all known agents
tma daemon                    # foreground; --ensure for spawn-if-absent
tma reload                    # signal the running daemon to hot-reload config + manifests
```

Zero-config floor (F23): no hooks installed, no daemon → phase-1 polling surfaces
work. Each layer added (hooks, then daemon) upgrades latency and coverage without
changing any consumer configuration.

### Config + manifest hot-reload (H3)

A running daemon reloads `config.toml` + the manifest set **without a restart**. It
re-reads the same config path and manifest dir it started from (so `TMA_CONFIG` /
`--config` / the default path resolve identically) and swaps every config-derived
knob in place — sweep cadence, quiet threshold, `notify.on` / `notify.command` /
`notify.bell`, the fold tuning, `demote_edges` — while its live control clients, D15
demotion state, and notify history all survive the swap.

- **SIGHUP** is the reload signal. The daemon's signal self-pipe distinguishes it from
  SIGTERM/SIGINT (which still shut down); a SIGTERM racing a SIGHUP shuts down.
- **`tma reload`** is the convenience — you rarely know the daemon's pid, and
  `--socket-name` may select one of several per-server daemons. It finds the daemon for
  the target server (via the pid the daemon records in its lock file), verifies it is
  live (socket connect), and sends SIGHUP. No daemon running is a clean no-op.
- **Invalid reloaded config or manifests are kept-old**, all-or-nothing: the daemon
  logs the error and keeps serving with the last good config — a reload never kills or
  corrupts a running daemon.
- The **picker** re-reads config + manifests on its own refresh tick (edits apply
  within ~1 s while it is open); **one-shots** (`status`/`ls`/`event`/`jump`/`doctor`)
  reload per invocation by nature. This supersedes F25's original mtime-watch framing:
  reload happens on the SIGHUP / tick, not by watching file mtimes.

## Requirement deltas proposed

1. **F7** — add "agent hook events" as the highest-fidelity evidence source; new
   arbitration order as above (amends F8). D2 relaxes for hook evidence: a `blocked`
   hook event is sufficient to assert blocked without viewport chrome (it *is* direct
   evidence, not inference from silence).
2. **N7** — split: CLI one-shots remain subprocess-only; the daemon uses one
   per-session pool of long-lived control-mode clients with reconnect handling
   (Event source 2). Server-gone still
   terminates the daemon gracefully.
3. **New F-req** — `tma event` contract: `$TMUX_PANE` binding, direct-stamp fallback
   when no daemon, stable CLI interface (it becomes part of the public API alongside
   the stamped options, since users' agent configs reference it).
4. **New F-req** — `tma install-hooks <agent>` / `--check`: idempotent, additive,
   diff-before-write. Modifying a user's agent config is the one place tma writes
   outside its own domain; requires explicit invocation (consistent with N5's spirit).
5. **New D-req** — hook mappings are evidence-authored like manifests (D10): verify
   each agent's hook names, payloads, and firing conditions against the real agent
   before shipping a mapping; fixture-test the payload parsing (D8 analog).
6. **Manifest schema** — per-agent declaration of hook coverage (which states hooks
   report), so the engine knows what the capture fallback must still cover for that
   agent.
7. **D3** — tighten for hook-capable agents: blocked visible/notified in <1 s.
   Hookless agents keep the existing targets.

## Open questions (new)

1. ~~Does `Notification` distinguish permission-prompt from idle-reminder? Verify full
   payload field set on current release.~~ Resolved (H14, Claude Code 2.1.212): a real
   permission-prompt `Notification` captured from an isolated `claude` interactive run
   carries `session_id`, `transcript_path`, `cwd`, `prompt_id`, `hook_event_name`,
   `message` (`"Claude needs your permission"`), and a dedicated **`notification_type`**
   field (`"permission_prompt"`). So the discriminator is a first-class field, not buried
   in `message` — the manifest matcher `permission_prompt|elicitation_dialog` (applied to
   the whole JSON blob) hits it. The idle-reminder variant is the same envelope with a
   different `notification_type`/`message`. Note: the `Notification` hook is TUI-only — it
   did not fire under headless `claude -p` (the tool auto-resolved), so the capture required
   driving the interactive TUI to a live permission prompt.
2. Codex/OpenCode/Gemini/Cursor hook-equivalent capability audit (blocks the coverage
   table). tmux-agent-sidebar's adapters (`codex.rs`, `opencode.rs`) are MIT reference
   material for the audit.
3. ~~Control-mode output notifications vs `refresh-client -B` subscriptions at the
   tmux 3.2 floor (N10) — which is available, and what does the degrade path cost?~~
   Resolved: the code chose `%output` (`tma/src/control.rs`, behavior probe
   `probe_push`). `%output` fires even under an alternate-screen agent TUI, so it is
   the load-bearing activity source; the degrade path when the behavior probe fails is
   the reconciliation sweep at a faster interval.
4. ~~Should `tma event` accept arbitrary user-defined agents (config-mapped, X1) so
   people can wire hooks for agents we don't ship mappings for?~~ Resolved (H13): yes,
   and the machinery already ships: the X1 manifest set IS the extensibility point, so
   there is no config-level event-to-state mapping layer needed or wanted. `tma event`
   resolves an agent name against the loaded manifest set and applies that manifest's
   `[[hooks.map]]` table; a user manifest under `~/.config/tma/agents/` is loaded
   alongside the bundled corpus (shadowing a bundled stem, or adding a wholly new agent),
   and an agent with no manifest is a clean no-op (exit 0, no stamp, no error). Proven
   end to end for a non-shipped agent name in `tma-runtime`'s `custom_agent_integration`
   tests.

   How-to: write a manifest (`min_engine_version`, an `[identity]` block with
   `process_names`, a `[hooks]` block whose `[[hooks.map]]` entries map your agent's hook
   event names to claims (`claim = { state = "working" }`, `claim = { lifecycle =
   "start" }`, `claim = { state = "blocked", detail = "permission" }`), and an (optionally
   empty) `[capture]` block) and drop it at `~/.config/tma/agents/<name>.toml`. Then wire
   your agent's hook to run `tma event <name> <Event>` (with the hook payload on stdin, or
   as a trailing argv arg for notify-style programs). Each fire maps through your manifest
   and stamps the pane; no code change and no separate mapping config are involved.
