# tma custom actions and control surfaces

Status: draft v4
Date: 2026-07-28
Inputs: ARCHITECTURE.md (AD1, AD2, AD4, tier rule), REQUIREMENTS.md (F14 frozen
state vocabulary), pane-options-and-json.md, manifest-schema.md. Prior art:
OpenAI's Codex Micro control deck (Work Louder collaboration, July 2026), the
Elgato Stream Deck plugin SDK, tmux `display-menu`, and the 2026-07-27
context-monitor survey in the appendix.

Method: same as ARCHITECTURE.md. Each decision (ACT1 onward) lists the options
considered, the pick, and the condition that would reopen it.

## Problem

The read path is finished: state lives in pane options, and any surface (status
line, picker, `ls --json`, a script) is a dumb reader of the same contract. The
write path stops at `tma jump`, which moves focus and nothing else.

Physical control surfaces make the gap concrete. A Stream Deck or macro keyboard
can already paint per-pane state by polling `tma ls --json`, but a button that
*acts* on an agent has nothing sanctioned to call. Pressing "approve" on a
blocked pane means hand-rolled `tmux send-keys`, which is unguarded: the pane
may have left `blocked` between the paint and the press, and the keystrokes land
in whatever prompt is there now. Beyond keystrokes, users want side-quest
actions that use the agent vendors' SDKs against a *running* interactive
session: fork the pane's Claude session headlessly and ask for a progress
summary, spawn a reviewer in the same working directory, extract todo state from
the transcript. tma has the context those actions need (pane id, agent name,
state, working directory, agent session id) and currently gives it to no one in
an actionable form.

The layer this document designs is the **action broker**: user-defined actions
declared in manifests, validated and guarded by tma, and invocable identically
from every control surface.

## Constraints inherited, and one new one

- Pane options remain the only shared store (AD4). Actions add no socket and no
  registry process.
- The daemon stays strictly additive (tier rule). Every action must work
  daemonless; the daemon may later lower read-path latency for surfaces, but the
  act path never requires it.
- The core stays pure. Action *schema* can live beside the agent manifest
  schema; action *execution* is I/O and lives above the core.
- All tmux writes pass through the `tma-tmux` choke point, including the new
  keystroke path.
- The state vocabulary stays closed (F14). Actions gate on it; they cannot
  extend it.
- New: **tma never embeds an agent SDK.** The Claude Agent SDK, Codex SDK, and
  their successors are moving targets in other languages. tma brokers context
  to user-authored commands; the SDK dependency lives in the user's script,
  where the user can pin and update it.

## System shape

```
            surfaces (choose one, or several)                 broker                       effect
┌─────────────────────────────────────────────┐   ┌───────────────────────────┐   ┌──────────────────────┐
│ Stream Deck plugin ──┐                      │   │ tma act <name> --pane %N  │   │ keys: send-keys via  │
│ tmux display-menu ───┼── all invoke the ────┼──►│  1. load action manifest  ├──►│   tma-tmux guard     │
│ tmux keybinding ─────┤     same CLI verb    │   │  2. re-verify state gate  │   │ exec: spawn command  │
│ shell / scripts ─────┘                      │   │  3. acquire pane lock     │   │   with TMA_* context │
└─────────────────────────────────────────────┘   │  4. run, bound, release   │   │   env                │
                                                  └───────────────────────────┘   └──────────────────────┘
```

Surfaces stay dumb on the act path exactly as they are on the read path: they
enumerate actions from `tma act --list --json`, render them, and shell out to
`tma act <name>`. No surface constructs a `send-keys` line or resolves context
itself.
A surface that bypasses the broker gets no guard, which is the same deal the
read path offers today (you *can* read pane options raw; the contract is that
you do not need to).

## User flows

The decisions below were checked against these flows. Each names the surfaces
it exercises, because surface parity is a design goal: nothing may work on the
Stream Deck that a keyboard-only tmux user cannot reach.

**Flow 1, approve from inside tmux, no hardware.** The status line shows a red
`⚑ 1`. The user hits the managed keybinding (`tma install-keys` already owns a
key table), which runs `tma act --menu` for the blocked pane. tmux
`display-menu` pops with the actions valid *right now*: `approve`, `deny`,
`interrupt`, plus any user actions gated open. One keypress fires
`tma act approve --pane %5`. The broker re-verifies the pane is still
`blocked/permission`, sends the manifest's key sequence for that agent, done.
Round trip is two keypresses and no focus change; the alternative today is
jump, read, type, jump back.

**Flow 2, Stream Deck as status board and remote.** The plugin polls
`ls --json` (about 1 Hz) and paints one key per agent pane: red blocked, amber
working, green idle-with-attention. Pressing a pane key jumps to it. A second
page per pane renders `act --list --json --pane %N`: valid actions lit, gated
ones dark. Pressing `approve` runs the same CLI verb as flow 1. A `confirm`
action renders armed on first press, fires on second. The plugin contains zero
policy: it draws JSON and shells out.

**Flow 3, SDK side-quest against a live session.** A Claude Code pane has been
`working` for twenty minutes. The user fires `summarize`, a user-authored exec
action. The broker execs the user's script with `TMA_SESSION_ID` (from
`@agent_session`, stamped at hook registration), `TMA_CWD`, `TMA_AGENT`. The
script runs `claude -p --resume "$TMA_SESSION_ID" --fork-session "Summarize
progress and open questions"` and pipes the answer to a notifier. The TUI
session is untouched because the fork copies it. tma's contribution is exactly
the context handoff and the state gate; the SDK call is the user's code.

**Flow 4, authoring an action.** The user drops a TOML file in
`~/.config/tma/actions/`, then runs `tma act myaction --pane %5 --dry-run`,
which prints the resolved context env and the command (or key sequence) without
executing, plus the guard verdict it *would* have applied. `tma doctor` gains an
actions section: parse errors (loud, unknown keys rejected, same discipline as
config.toml), name collisions, dangling agent references. The loop is edit,
dry-run, fire, with no rebuild and no plugin API.

**Flow 5, scripted composition.** `tma wait --any --until blocked` already
unblocks scripts on state; a script can now follow with `tma act` and branch on
its exit codes. Wait-then-act is the headless equivalent of flow 1 and needs no
new primitive.

## ACT1: Action model, two kinds under one manifest form

**Question.** What *is* an action? Options: (a) freeform shell hooks only, a
`command` string per action; (b) a hardcoded verb set (`approve`, `deny`,
`interrupt`) implemented in Rust, no user actions; (c) manifest-declared
actions of two kinds, `keys` (a guarded key sequence into the pane) and `exec`
(a guarded process spawn with context env), with the built-in verbs shipped as
bundled manifests.

**Analysis.** (a) makes the common safety-critical case (approve) everyone's
individual shell problem, and a shell wrapper around `send-keys` cannot express
the guard without racing it. (b) covers approve but forecloses the SDK
side-quest class entirely, which is the half users cannot build themselves
safely. (c) is more schema, but it reuses the pattern the project already
proved with agent manifests: bundled TOML compiled in, user TOML in
`~/.config/tma/actions/` adding or shadowing by filename stem, fixture tests
beside the bundled files, no code path for extension. Keystroke sequences are
inherently per-agent (Claude Code's permission prompt takes `1`, Codex takes
`Enter`), so the keys kind carries a per-agent table rather than one global
sequence.

**Decision.** (c). One TOML form, two kinds:

```toml
# bundled: approve.toml
name  = "approve"
label = "Approve"
kind  = "keys"
when  = { state = ["blocked"], detail = ["permission"] }

[keys]
claude = ["1"]
codex  = ["Enter"]
```

```toml
# user: ~/.config/tma/actions/summarize.toml
name    = "summarize"
label   = "Summarize progress"
kind    = "exec"
agents  = ["claude"]
when    = { state = ["working", "idle"] }
requires = ["session"]          # refuse cleanly if @agent_session is absent
command = "~/.config/tma/actions/summarize.sh"
timeout_ms = 60000
```

Applicability of a `keys` action is derived from its `[keys]` table (an agent
with no entry cannot receive it); an `exec` action uses `agents` (absent means
all). `when` is optional; absent means the action is always fireable for its
applicable agents, and when present its keys (`state`, `detail`, and the ACT9
metric bounds) are ANDed. Gate values use the token rules from the manifest
schema. Unknown fields are a parse error. Bundled actions ship for the
highest-value guarded keystrokes (`approve`, `deny`, `interrupt`) with
evidence-backed key sequences recorded per agent in agent-coverage.md, same
discipline as screen rules.

Normative details, pinned here so no implementer has to invent them:

- **Key semantics.** Each `[keys]` array element is one tmux
  `send-keys` key argument with named-key interpretation on, so `Enter`,
  `Escape`, `C-c`, and `/compact` all mean what tmux says they mean; the whole
  sequence goes in a single `send-keys` invocation through the `tma-tmux`
  adapter, with no inter-key delay. A literal string that collides with a tmux
  key name is not expressible in v1 (none of the bundled sequences needs one);
  an escape form is added only if a real agent demands it. This is the entire
  mechanism of the safety-critical class, so it is pinned here, not left to
  the adapter.
- **One identity.** `name` must equal the filename stem; a mismatch
  is a load error. This keeps shadowing (by stem, inherited from agent
  manifests) and invocation (`tma act <name>`) the same key, so a user file
  cannot collide with a bundled action's name without also shadowing it.
- **`requires` vocabulary.** The accepted tokens and their context
  mapping are closed: `session` (`TMA_SESSION_ID`), `cwd` (`TMA_CWD`), `pid`
  (`TMA_PID`), `title` (`TMA_TITLE`). An unknown token is a parse error, same
  as an unknown field.

**Revisit if** a third kind appears that neither keystrokes nor a spawned
process expresses (see ACT8 for the wasm candidate, rejected; triggered
2026-07-29 by OpenCode's HTTP permission reply, addressed by ACT10's per-agent
`[api]` transport table rather than a new kind).

## ACT2: The guard, re-verify then act, and what TOCTOU remains

**Question.** What does the broker check between "user pressed the button" and
"keystrokes hit the pane"?

**Analysis.** The failure that matters: the surface painted `blocked` at T, the
user pressed at T+2s, the agent left `blocked` at T+1s, and a blind `y` Enter
now answers some *other* prompt, possibly a destructive one. No local check can
close the window completely (tmux has no transactional send), but the window
can be shrunk from seconds to the gap between one option read and one
`send-keys`, and the stale-paint case, which is the common one, is eliminated
entirely.

**Decision.** `tma act` performs, in order:

1. **Identity check.** The pane exists and `@agent_name` matches an agent the
   action applies to.
2. **Gate check.** Current `@agent_state` (and detail, if the gate names any)
   satisfies `when`. For `keys` actions the broker does not trust a stale
   stamp: if `@agent_stamped_at` is older than a freshness bound (default 3
   seconds, the status-line cadence plus slack), it runs one on-demand
   detection cycle on that pane first and gates on the result.
3. **Single-flight lock.** One pane option, `@agent_action`, whose value is
   `<expiry>:<nonce>:<pid>:<name>`: the absolute expiry in epoch ms first
   (so the guard extracts it by stripping everything after the first
   colon), a 128-bit random nonce (read-back correctness rides entirely on
   nonce uniqueness, so the entropy is normative), the holder's pid, and
   the action name for `tma debug` eyes. A single option is load-bearing,
   not cosmetic: a guarded state split across two options cannot be
   acquired atomically, because tmux expands each command's formats at that
   command's own execution time, so the second write's guard would re-read
   whatever the first write just changed. One option means one guarded
   write whose condition reads only pre-write state.

   The protocol:

   - **Acquire** is a single `-pF` conditional write: set the new
     `expiry:nonce:pid:name` value when `@agent_action` is absent or
     empty, or when its leading expiry field is numerically past. Storing
     the *absolute expiry* rather than an acquire timestamp is what makes
     reclaim expressible: staleness is a numeric comparison against the
     option's own pre-write value, with no lookup of the held action's
     manifest (which may have been hot-reloaded away) and no client-side
     read-decide-write. The expiry stamped is the invocation's deadline
     plus slack: `timeout_ms` for synchronous actions,
     `detach_timeout_ms` for detached ones.
   - **Liveness pre-check on reclaim.** Before treating a
     wall-clock-expired lock as dead, the acquirer checks the embedded pid
     with `kill(pid, 0)`; a live holder refuses with exit 5 even though
     the expiry has passed. Wall clocks and process timers diverge across
     suspend (the clock advances through a laptop sleep; a process timer
     may not), so expiry alone would reclaim from a supervisor whose child
     is still running. The pre-check is advisory (it races nothing: the
     CAS still decides), but it closes the suspend window.
   - Because a `-pF` set always "succeeds", the winner is decided by a
     mandatory read-back: re-read `@agent_action`, compare the nonce; a
     mismatch means another invocation won the race, and this one exits 5.
   - **Clear** is a nonce-conditional set-to-empty: tmux has no
     conditional unset, so release writes the empty string when the value
     still carries this invocation's nonce, and leaves it untouched
     otherwise. The nonce condition is *not* an `s///` field extraction
     (an `s/…/…/` pattern cannot carry a colon, and the nonce sits between
     colons, so it cannot be pulled out by substitution); it is a tmux
     fnmatch, `#{m:*:<nonce>:*,#{@agent_action}}`, truthy iff the stored
     value still carries this invocation's nonce as a complete
     colon-delimited field. The same predicate backs the supervisor's
     nonce-conditional pid *rewrite* (ACT6): fnmatch tests presence, so it
     survives a rewrite that keeps the nonce. Empty and absent are
     equivalent everywhere the lock is read; the acquire guard's first arm
     covers both. An unconditional clear would be an ABA hole: a slow
     holder whose lock expired and was reclaimed must not wipe the new
     holder's lock.

   A held, unexpired lock refuses with exit 5 immediately. The lock stops
   a double-press and stops two surfaces firing conflicting actions into
   one pane; pane death clears it for free, since pane options die with
   the pane.

   The expressions are validated against a live tmux 3.6a server (fresh
   acquire, held-refusal, expired reclaim, empty re-acquire, stale-nonce
   no-op clear, own-nonce clear, post-clear re-acquire all exercised), and
   two syntax facts the validation surfaced are normative:

   - The expiry extraction is `#{s/[^0-9].*//:#{@agent_action}}`, "strip
     from the first non-digit," and its pattern must contain **no colon
     anywhere**, not merely no leading colon: tmux consumes the *first*
     colon after the modifier as the modifier/argument separator
     regardless of the delimiter chosen, so a colon at any position ends
     the pattern there and mangles the match, silently disabling the
     reclaim arm while leaving fresh acquires working. The pinned
     `[^0-9].*` is colon-free by construction. This same constraint is why
     the nonce-conditional clear/rewrite predicate is an fnmatch, not an
     `s///` extraction (documented at **Clear** above). The `s/` target
     must also be the nested form `#{@agent_action}`; a bare
     `@agent_action` name in target position expands to empty.
   - Acquire guard, validated verbatim:
     `#{?#{||:#{==:#{@agent_action},},#{e|<:#{s/[^0-9].*//:#{@agent_action}},NOW}},NEW,#{@agent_action}}`
     with `NOW`/`NEW` interpolated by the writer. The empty string as the
     left operand of `e|<` compares as less-than (covers the cleared
     state), and a corrupt value with no leading digits extracts to empty
     and is therefore treated as expired, which is the desired recovery
     for a mangled lock. Field values never contain commas (nonce is hex,
     expiry and pid are digits, the name obeys the manifest token rules),
     so the format-argument separators are safe by construction.
4. **Act.** First re-assert the gate once under the held lock (one option
   read, shrinks the residual window for `keys` at no real cost).
   `keys`: the sequence goes through the `tma-tmux` write adapter, never a
   raw shell-out. `exec`: the command spawns with the context env (ACT3),
   bounded by `timeout_ms` (default 30 000), process-grouped so timeout kills
   the tree. Detached actions hand the lock to the supervisor instead (ACT6).
5. **Release** the lock (nonce-conditionally, step 3) on every synchronous
   exit path, including timeout and signal. A SIGKILLed broker cannot
   release; that is what the expiry bound in step 3 exists for, and it is
   the only recovery path, so every lock write carries a finite expiry.

The residual race, a state flip between step 2's read and step 4's send, is
accepted and documented. It is the same residue every interactive user lives
with when they type into a pane, now bounded at milliseconds instead of the
seconds a surface repaint cycle allows.

`--force` skips the `when` gate only, never `requires`, and never steps 3 or
5 (`requires` is a correctness precondition, not a staleness guard; a
forced `summarize` with an empty `TMA_SESSION_ID` is exactly the half-run
`requires` exists to prevent). The flag exists so the default path never needs
loosening.

**Revisit if** tmux grows an atomic compare-and-send, or evidence shows the
on-demand re-verify cycle is too slow for the deck press-to-effect budget
(target under 150 ms; a capture plus fold is well inside it today).

## ACT3: Context contract, env only

**Question.** How does an exec action learn which pane it serves?

**Analysis.** Interpolating context into the command string (`command =
"summarize.sh {pane}"`) is a quoting injection waiting to happen; pane titles
are attacker-influenced text (an agent prints what its tool output tells it
to). Environment variables cross the exec boundary without interpolation and
without shell re-parsing. The set must be enough for the SDK flows without
leaking captured screen content, consistent with the notification payload's
metadata-only rule.

**Decision.** Exec actions receive context exclusively as environment
variables; the `command` string is passed to `sh -c` verbatim with no
substitution performed by tma. The command spawns in tma's own working
directory, not the pane's: repo-relative execution would partially
reintroduce the per-project trust question deferred in the open questions, so
scripts that want the agent's directory use `TMA_CWD` explicitly. Env
transport keeps hostile values inert only at this boundary; the how-to tells
authors to quote every `TMA_*` expansion, because a user script that
interpolates one unquoted re-parses it.

| variable | source | notes |
|---|---|---|
| `TMA_PANE` | pane id | e.g. `%5` |
| `TMA_AGENT` | `@agent_name` | |
| `TMA_STATE` | `@agent_state` | as gated, at act time |
| `TMA_DETAIL` | `@agent_detail` | empty when none |
| `TMA_SESSION_ID` | `@agent_session` | agent's own session id; empty when never registered; agent-supplied, validated at *read*, not at stamp (the stamp path writes raw): the broker accepts a case-insensitive charset (ASCII alphanumerics plus `-`/`_`) and treats any other value as absent, so a corrupt or hostile stamp never reaches the env or satisfies `requires`. Broader than the manifest's lowercase-only token charset because real ids are mixed-case (OpenCode stamps `ses_…W6yCmb3x7wLH1X`), which lowercase-only would reject |
| `TMA_CWD` | `#{pane_current_path}` | filesystem-derived; quote it like everything else |
| `TMA_PID` | `@agent_pid` | process-group leader |
| `TMA_LOCATOR` | `session:window.pane` | same form as JSON rows |
| `TMA_TITLE` | `#{pane_title}` | untrusted text; env transport keeps it inert |
| `TMA_ACTION` | action name | lets one script back several actions |

`requires` (ACT1) names context keys that must be non-empty for the gate to
pass, so a script never half-runs on a missing session id.

Two additive contract changes ride along: the `ls --json` row gains a
`session` key (string or null, mirroring the notification payload; schema stays
1 under the additive rule), and `tma act --list --json` is a new schema-1
document whose `actions` array carries, per action, `name`, `label`, `kind`,
`agents` (the applicability list; an empty array means all agents, the resolved
form of an `exec` action's absent `agents`), and `when` (the gate as an object,
or `null` when the action is ungated). Given `--pane`, each action object also
carries a fireability verdict: not a bare boolean but `fireable` plus a `reason`
token (`gated` / `locked` / `wrong-agent` / `requires-unmet` / `no-coverage`,
`null` when fireable), because a deck renders those cases differently.
`no-coverage` is the permanent variant of `gated`: the gate reads a metric the
agent's manifest declares no telemetry channel for, so the action can never
light on this pane. Without the distinction, a bundled `compact` on a
no-telemetry agent is indistinguishable from "context not high enough yet," and
a deck cannot choose between gray-out-temporarily and gray-out-permanently. The
exact-key-set drift tests extend to it.

The refusal reasons are a total order, and a gate reporting more than one
refuses with the **most permanent**: `wrong-agent` (the action never applies to
this agent) then `no-coverage` (a metric bound on an agent with no telemetry
channel) then `requires-unmet` (a required context key is empty) then `gated`
(state/detail/metric-range, transient). `gated` sits last because it is the only
refusal `--force` skips (ACT2), so a permanent reason must never be masked by
the skippable one. `locked` is not a gate reason: it is a broker-time verdict
that surfaces in `--list` only for an action that is *otherwise fireable* (the
gate wins over the lock, mirroring the fire path, which refuses the gate before
it reaches the lock).

**Revisit if** an action class needs captured screen text; that would need its
own opt-in flag and a privacy note, not a silent widening of this table.

## ACT4: CLI surface and surface parity

**Question.** What are the verbs, and how do non-hardware users reach them?

**Decision.**

One verb, three modes, matching the flat verb surface of `ls`, `wait`, and
`jump` (a separate `actions` noun namespace was considered and dropped: two
spellings for one concept is a learnability tax the existing surface never
charges):

| command | role |
|---|---|
| `tma act <name> [--pane <ID> \| --agent <NAME>]` | fire one action; `--pane` defaults to the current pane inside tmux; `--agent` resolves like `wait --agent` (unique or error) |
| `tma act <name> ... --dry-run` | print resolved context (with each value's age), gate verdict, and the would-be keys or command; execute nothing |
| `tma act <name> ... --force` | skip the `when` gate (never `requires`, ACT2) |
| `tma act <name> ... --yes` | satisfy `confirm` (ACT6) non-interactively |
| `tma act --list [--json] [--pane <ID>]` | enumerate actions; with `--pane`, include the per-action fireability verdict (ACT3) |
| `tma act --menu [--pane <ID>]` | render a tmux `display-menu` of currently-fireable actions; the parity surface for keyboard-only users, wired to a key by `tma install-keys` |

Exit codes extend the `wait` convention rather than inventing one: `0` acted,
`124` exec timeout, `3` target pane vanished or never existed, `2` usage, `1`
runtime failure, plus `4` gate refused (state did not satisfy `when`, or
`requires` unmet; the refusing fact goes to stderr) and `5` pane action lock
held. The reserved band (`3`, `4`, `5`, `2`) is strictly pre-spawn broker
verdicts: for an exec action that did spawn, the child's own exit
code passes through verbatim and can therefore land inside the reserved band,
so scripted consumers branching beyond success/failure use the `--json`
result object (action, pane, outcome, exit detail; schema-1), which is
authoritative and unambiguous. `keys` actions never spawn a child, so their
codes are always broker verdicts.

The exit codes and the fireability `reason` tokens (ACT3) are one vocabulary
seen from two sides, and the mapping is pinned: `gated`, `requires-unmet`,
`wrong-agent`, and `no-coverage` all refuse with exit 4 (the specific reason
goes to stderr and the `--json` result), and `locked` refuses with exit 5.
An identity mismatch is a refusal like any other, not a runtime failure: the
pane exists and the broker worked; the action simply does not apply to it.

The `--json` result's `outcome` field is the third face of the same
vocabulary and is closed: `sent` (keys delivered), `exited` (synchronous
exec child finished; `exit_code` carries its code), `spawned` (detached
supervisor launched), `timeout` (synchronous child killed at `timeout_ms`),
`refused` (`reason` carries which gate), `vanished` (the pane disappeared
mid-act), `error` (broker runtime failure). A value-set drift test pins the
tokens alongside the key-set tests, since scripts are told this field is
authoritative.

Surface parity falls out: deck, menu, keybinding, and script all call the same
verb, so an action definition is written once and reachable four ways.
`display-menu` is deliberately the reference surface; if a flow works only
with hardware attached, the design has failed review.

**Revisit if** a surface genuinely needs an operation the CLI cannot express.

## ACT5: Crate placement

**Question.** Where does the code live under the three compiler-enforced rules?

**Decision.** The split mirrors agent manifests exactly:

- `tma-core`: action manifest schema, parsing, validation (token rules, gate
  vocabulary, per-kind field checks), applicability and gate evaluation as pure
  functions over a snapshot. Bundled action TOMLs compile in beside the agent
  manifests, with fixture tests asserting each bundled action's gate against
  captured snapshots.
- `tma-runtime`: action discovery and shadowing (user dir over bundled, by
  filename stem, hot-reload with the existing manifest reload), the broker
  (gate, lock, timeout, env assembly), exec spawning.
- `tma-tmux`: the key-sequence write path and the conditional lock write. No
  other crate constructs `send-keys`.
- `tma`: the `act` subcommand, `--json` result formatting,
  doctor checks.
- `tma-daemon`: nothing. The act path is tier 2 by construction; a stray
  daemon import would fail the existing source-guard test.
- `tma-ui`: `act --menu` rendering only, reading broker results through the
  same two-helper discipline the picker uses.

**Revisit if** the broker needs state that outlives one invocation (it must
not; the pane lock is the only cross-invocation fact, and it lives in the pane
options like everything else).

## ACT6: Execution semantics for exec actions

**Question.** Synchronous or detached, and what does "confirm" mean?

**Analysis.** SDK calls run seconds to minutes. A deck button that blocks a
plugin thread for a minute is broken, but fully-detached-by-default loses the
exit code that scripts (flow 5) rely on.

**Decision.** Synchronous by default, bounded by `timeout_ms`; stdout and
stderr pass through, exit code is the action's own (gate refusals having
already exited 4 before spawn). A manifest may set `detach = true` for
long-running actions:

- `tma act` returns 0 on successful spawn. Spawn means spawn: a detached
  action is fire-and-forget by contract, its exit code says nothing about
  the child's outcome (that arrives on the completion notification, a
  different channel), and scripted composition (flow 5) uses synchronous
  actions when it needs to branch on results.
- **A tma-owned supervisor holds the lock, not the user's script.** The user
  command knows nothing about nonces or pane options, so "the child clears
  the lock" cannot mean the script does it. The broker forks a small
  supervisor (the same binary, an internal mode) which spawns the user
  command in its own process group and then does exactly three things.
  "Forks" is the shape, not the syscall: the mechanism is a re-exec spawn
  (`Command::spawn` of the same binary into a hidden `supervise` subcommand,
  `setsid`, stdio nulled), the codebase's existing detach machinery, not a
  `fork(2)`. The supervisor then:
  - holds the single-flight lock for the child's lifetime;
  - **kills the process group at the deadline**, `detach_timeout_ms`
    (default 900 000, 15 minutes) measured against the *wall clock*, not a
    process timer, so the kill deadline and the lock expiry cannot diverge
    across a suspend. The kill is what makes the field a real execution
    bound rather than a passive number: without it, a hung SDK call would
    outlive its lock expiry and overlap the next acquisition, the exact
    race the inherited lock exists to prevent;
  - on child exit (or after the kill), clears the lock nonce-conditionally
    and fires the completion notification. On a pane that died first the
    clear is a no-op (the options died with the pane) and the notification
    still fires.

  Lock custody crosses the fork in two steps: the broker acquires with its
  own pid (it cannot know the supervisor's yet), forks, and the supervisor
  rewrites the value nonce-conditionally with its own pid before the broker
  exits. If the fork fails, the broker clears the lock synchronously rather
  than leaving it to expiry; if the broker dies between acquire and fork,
  the embedded pid is dead and the ACT2 reclaim path takes the lock at
  expiry. Under normal operation the supervisor's kill-then-clear always
  beats the expiry-plus-slack, so expiry reclaim is recovery for exactly
  one abnormal case: the supervisor itself dying uncleanly.

  The completion notification is its own pinned contract, not a reuse of
  the state-notification payload (a completion has no `state`, and its pane
  may already be gone): top-level keys `schema` (`1`), `action`, `pane`,
  `agent`, `outcome` (ACT4 vocabulary), `exit_code` (number or null), and
  `locator` (string or null when the pane is gone). One optional key rides
  additively (schema stays `1`, matching the row-JSON precedent):
  `lock_release_failed` (`true`), emitted only when the supervisor's
  nonce-conditional lock clear failed and absent otherwise, so the
  otherwise-silent release failure (the supervisor's stderr is `/dev/null`)
  reaches a consumer; a dead pane's failing option write correlates it with
  a null `locator`. Exact-key-set drift tests pin the success-case payload
  like the other three JSON contracts.

  Releasing at spawn was rejected because
  it would serialize only the millisecond broker window while two detached
  `summarize` runs raced the same live session for minutes; the lock means
  the same thing for both execution modes. `timeout_ms` does not apply to a
  detached child; `detach_timeout_ms` is the only bound.
- **No log file.** tma writes nothing to disk for a detached action; AD4's
  no-files rule (daemon lock and socket only) stands unamended, and since a
  script's stdout can echo pane text, a tma-owned log would also brush N8.
  Detached stdout/stderr go to `/dev/null`; a script that wants its output
  somewhere redirects it itself, which keeps custody of possibly-sensitive
  content in the author's hands.
- Completion rides the notify *dispatch* mechanism (the detached capped
  child) but **not** the state lane's dedup record: `@agent_notified_at`
  belongs to blocked/done episode dedup, and bumping it out-of-band would
  corrupt that arbitration. A completion is inherently single-shot
  per spawn, so it needs no dedup marker at all; it fires once, done or
  failed, with the action name and exit code.

`confirm = true` marks an action as wanting a second factor; enforcement is
per-surface (CLI: `--yes` or an interactive prompt on a TTY; menu: a nested
confirm entry; deck: arm-then-fire), and the broker refuses a confirm action
from a non-TTY without `--yes` so a script cannot stumble into one.

Guidance, not mechanism: action authors touching live sessions are pointed at
fork-and-read patterns (`--fork-session`, transcript reads) in the how-to;
`confirm = true` is recommended for anything that injects into a session or
mutates a repo. tma cannot verify what a user script does, and pretending
otherwise (a `mutates` field driving behavior) would be a false attestation; a
single `confirm` bit the *author* sets is honest about where knowledge lives.

**Revisit if** detached actions need progress reporting richer than
done/failed, which would reopen the daemon-as-optional-event-bus question in
DAEMON.md.

## ACT7: What this is not, the daemon-hub inversion, declined

**Question.** Prior art (Codex Micro against the Codex cloud backend, and most
agent dashboards) centralizes on a service that owns clients. Should control
surfaces connect to a tma daemon hub?

**Analysis.** The tier rule is the project's spine: every capability works
daemonless, the daemon only lowers latency. A hub architecture inverts that for
the entire act path and forfeits the property that makes the read path
composable (any `show-options` reader is a full citizen). The one thing a hub
genuinely buys, push latency on the read path for surfaces that hate polling,
is a *read-path* concern: a future `subscribe` stream served by the tier-3
daemon (wire protocol already in `tma-runtime`), with polling `ls --json` as
the universal fallback. That belongs in DAEMON.md as an additive decision and
is out of scope here.

**Decision.** Surfaces are CLI consumers, not clients. No hub. The deck plugin
holds no connection to anything but the `tma` binary and (optionally, later)
the daemon's subscribe stream, with graceful fallback to polling.

**Revisit if** a surface appears that cannot spawn processes at all; none of
the named ones (Stream Deck SDK plugins, QMK host apps, tmux itself) is so
constrained.

## ACT8: WASM actions, evaluated and rejected for this layer

**Question.** Should actions be WebAssembly components instead of (or beside)
shell commands? tma would embed a runtime (wasmtime), define a WIT host
interface (read snapshot, send guarded keys, stamp options), and load `.wasm`
files from the actions dir.

**Analysis.** The honest pro column is real:

- **Sandboxing with capabilities.** A wasm action declares what it may touch;
  a shell script may touch anything. This is the difference between "paste
  this action from a stranger's repo" being reasonable or reckless, so it is
  the enabling technology for a shared action ecosystem.
- **Portability and zero host dependencies.** One artifact, no bash/python/node
  version skew, Windows included.
- **A typed, versioned ABI.** A WIT interface with semver beats an env-var
  convention for long-term contract discipline.
- **In-process determinism.** Actions become fixture-testable pure-ish
  functions, the same property that makes the core's screen rules pleasant.

The con column decides it, though:

- **It amputates the primary use case.** The exec class exists to run agent
  SDKs, which are Node and Python processes (`claude -p`, the Agent SDK).
  WASI cannot spawn processes; granting a host `exec` capability to close the
  gap hands back exactly the power the sandbox was bought to remove. The
  actions worth sandboxing are the ones wasm cannot run.
- **Authoring ergonomics collapse.** The target author lives in tmux and
  writes a five-line shell script (flow 4: edit, dry-run, fire). The wasm
  loop is: pick Rust/Go/AssemblyScript, install a toolchain, compile against
  a WIT world, debug inside a sandbox. That is a plugin *developer* funnel,
  and this feature's audience is script *authors*.
- **Binary and maintenance weight.** wasmtime adds on the order of ten
  megabytes and a heavy dependency tree to a small static binary whose
  distribution story is "one executable on PATH", plus component-model churn
  tracked forever.
- **It solves a distribution problem tma does not have.** There is no action
  marketplace and no demand signal for running untrusted third-party actions.
  Keys actions, the safety-critical class, are already declarative TOML with
  no code to sandbox at all.
- **The latency win is irrelevant.** Actions are human-triggered and dominated
  by the SDK call itself; saving a fork is noise.

One insight survives the rejection: wasm's fit in this codebase is the *pure*
layer, not the effectful one. If screen-rule matchers ever prove too weak
(regex cannot parse a TUI's structured frames, say), a sandboxed pure matcher
function (bytes in, claim out, no I/O in the interface at all) slots into
`tma-core`'s fold exactly where compiled-in rules sit today, keeps the
fixture-test story, and never needs the capability that broke the actions
case. That is a separate future decision against a demonstrated detection gap,
not part of this design.

**Decision.** No wasm in the action layer. TOML for `keys`, host commands for
`exec`. Recorded here so the option is reopened on evidence, not rediscovered.

**Revisit if** (a) a real ecosystem of shared actions emerges and auditing
shell scripts becomes the adoption blocker, (b) a supported platform cannot
run host commands, or (c) the pure-matcher gap above materializes, which
reopens wasm for `tma-core` rather than for actions.

## ACT9: Telemetry, context utilization as a metric class

**Question.** Surfaces want per-pane context-window utilization (a deck key
showing 78%, a status-line gauge), and actions want to gate on it (a `compact`
button that lights past a threshold). State evidence cannot express this: it is
a continuous metric, not a member of the closed vocabulary. Where does the
number come from, and how does it ride the existing pipeline?

**Analysis.** The prior-art survey (appendix) settles two things. First, the
number is obtainable for most agents, but through four different channel
shapes, none of them the state-evidence channels tma already reads:

1. **Push shim** (Claude Code). The statusline command receives a documented
   `context_window` object per turn: `used_percentage` pre-computed,
   `total_input_tokens`, `context_window_size` (200k/1M aware), per-component
   `current_usage`, even per-subagent `tokenCount` via `subagentStatusLine`.
   Event-driven, 300ms debounce. This is the sanctioned live monitor; hook
   payloads carry no counts at all, and `PreCompact` is a cliff event, not a
   gauge. Known hazards: fields were cumulative garbage before v2.1.132,
   values go `null` early in a session and right after `/compact`, and the
   window size is misdetected for 1M and custom-endpoint models.
2. **Sidecar file tail** (Codex). Rollout JSONL under `~/.codex/sessions`
   records `token_count` events carrying both `total_token_usage` and
   `model_context_window`, so the percentage is computable externally with no
   model table. Files grow to gigabytes; the reader must seek from the end,
   never re-read. Non-interactive sessions may omit the events.
3. **Turn-granularity hooks** (Cursor `stop` carries token counts, Gemini
   `AfterAgent` can report cumulative tokens). Fine for a threshold, too
   coarse for a live mid-turn gauge. (Cursor was later found to also expose an
   undocumented `statusLine` push channel — see the channel facts below — so its
   gauge rides that, not `stop`; Gemini still has no finer channel.)
4. **Screen extraction**, demoted to last resort: Codex v0.120.0 replaced its
   footer percentage with a bar graph, which is what happens to scrape targets.

OpenTelemetry was evaluated and rejected for this purpose: both Claude's and
Gemini's exporters emit cumulative token *counters* (with a delta-temporality
default that loses short sessions), not a current-window gauge. Right tool for
cost dashboards, wrong tool for a per-pane percentage.

Second, the survey found the field genuinely open: existing tools either
aggregate one agent into one status string (agent-status-tmux, via a
statusline cache file), show context on a deck for one agent (yolodeck,
Claude Control's context ring), or track state without context
(tmux-agent-indicator, AgentDeck). Nobody stamps per-pane context as
addressable state, nobody normalizes it across agents, and nobody closes the
threshold-to-action loop. The metric class below does all three with
machinery this document already defines.

**Decision.** Context utilization becomes the first **metric**, carried
beside state, never inside it:

- **Store.** Two new pane options: `@agent_context_pct` (integer 0 to 100)
  and `@agent_context_at` (epoch ms of the evidence). Both absent when the
  agent has no telemetry coverage, the same absence contract as
  `@agent_detail`. The pair follows the full stamp grammar, not a loose copy
  of it; the metric lane earns every guard the state lane needed, because it
  has the same producers racing over the same store:
  - *Evidence-time write guard.* The pair is stamped under a server-side
    conditional write that accepts the update only when the incoming
    evidence time is not older than the stored `@agent_context_at`, the
    direct analog of the hook-versus-capture arbitration. Without it, the
    fire-and-forget push path can land turn N's 50% after turn N+1's 55%
    and walk the gauge backwards.
  - *Ownership guard.* Context intake passes the same session-ownership
    filter as hook events: only the owning `@agent_session` may stamp the
    pane's gauge, so a subagent reporting its own small context on the
    shared pane cannot clobber the parent's 78%.
  - *Marker written last.* `@agent_context_at` is written last in the
    context mini-chain, and a reader that sees a pct newer than its `at`
    treats the pair as in-progress, mirroring the `stamped_at` rule.
  - *Null clears, under the same guard.* A channel report of `null`/unknown
    (Claude emits it right after `/compact` and early in a session)
    **unsets** `@agent_context_pct` rather than being skipped. Keeping the
    last value was rejected: a manual `/compact` drops context without a
    turn, and a kept 78% leaves the compact button lit on an
    already-compacted pane. A clear is an observation like any other: it
    passes the evidence-time guard (a stale late null must not erase a
    fresher real value) and it **advances `@agent_context_at` to its own
    evidence time**. Leaving `at` behind on clear would let a reordered
    duplicate of the pre-compact push pass the `not older` guard and
    resurrect the stale 78% on the compacted pane, precisely the failure
    the clear exists to kill.

  The `ls --json` row gains `context` (number or null) and `context_at_ms`;
  additive, schema stays 1. Surfaces render absence as absence and may gray
  a stale value; the broker enforces no freshness bound for *display*.
- **Manifest.** A `[telemetry.context]` section declares the channel:
  `channel = "event" | "file-tail" | "screen"` plus a `format` id naming a
  compiled-in parser (`claude-statusline-json`, `codex-rollout-jsonl`, ...).
  The metric-named subtable exists so a second metric (cost, rate-limit
  headroom) is an additive sibling, not a breaking migration.
  Parsers are pure functions (bytes in, metric out) in `tma-core` with
  fixture files per format version, exactly the screen-rule discipline; the
  I/O (shim intake, bounded tail) lives in `tma-runtime` edges. Unlike
  screen rules, parsers are not user-authorable: a new format needs core
  code. That asymmetry is accepted; parsing arbitrary vendor JSONL is
  trusted code, not configuration. agent-coverage.md gains a telemetry
  column recording the channel and its granularity per agent.
- **Ingest, push.** A new `tma event context` intake on the existing hook
  bridge. For Claude Code, `tma install-hooks claude` gains a statusline
  shim that *chains* the user's existing statusline command (running it
  first, passing stdin through, emitting its output unchanged) and forwards
  the JSON fire-and-forget; installing must never replace or break a user's
  statusline. The shim passes its own `$TMUX_PANE` (valid here: the
  statusline command runs in the agent's real process context, not tmux
  `run-shell`, so AD1's stale-environment hazard does not apply), which
  removes any session-to-pane reverse mapping. Spawn rate is bounded by
  Claude's own 300ms statusline debounce per pane, the same order as the
  hook events the bridge already absorbs per tool call; the evidence-time
  guard makes any reordering harmless.
- **Ingest, pull.** The Codex path is a bounded tail with **no persisted
  offset**: a stored byte offset would be meaningless across the per-session
  dated rollout files' rotation, and end-anchored reads need no state at
  all. Each poll cycle reads the last K bytes from EOF (K = 64 KiB), with
  standard tail hygiene made normative because parsing rides on it: the
  leading partial line in the window is discarded, and a trailing partial
  line (caught mid-write) is ignored. If the window holds no `token_count`
  record, the reader scans backward in K-sized chunks up to a 1 MiB cap
  before giving up; a single heavy turn can append far more than 64 KiB of
  tool output after the last `token_count`, and without the backward scan
  the gauge would freeze precisely while context grows fastest. Past the
  cap the cycle stamps nothing and the gauge goes stale honestly (surfaces
  gray it via `context_at_ms`). The reader keeps a process-local memo,
  `(file identity, size, mtime, last result)`, and skips the read entirely
  when the file is unchanged, so steady state on a quiet pane is one stat
  call, not a repeating 1 MiB scan (a rollout with no `token_count` at all,
  such as a non-interactive session, would otherwise be re-scanned to the
  cap every cycle forever). A size decrease or identity change invalidates
  the memo and forces a rescan. The memo is in-memory only: it is not a
  persisted offset, so the no-stored-state argument and the rotation
  immunity stand. The newest record found is stamped through the
  evidence-time write guard, which discards anything not newer than what is
  already stored. Rotation and truncation need no special case, and nothing
  about the tail touches disk beyond the read.
- **Window size.** Prefer the channel's own figure (`context_window_size`,
  `model_context_window`). Where a channel reports raw tokens with no window
  (Gemini, Cursor), a config table `[telemetry.windows]` maps model name to
  window, shipped with defaults and user-overridable. No silent 200k guess:
  an unknown window means no percentage, because a wrong gauge is worse than
  none.
- **Gates.** The ACT1 `when` table gains `context_pct_min` /
  `context_pct_max`, and both **fail closed**: an absent metric refuses with
  exit 4, because firing `compact` on an unknown gauge would be the
  wrong-gauge-worse-than-none principle inverted into action. The refusal
  distinguishes its reason (ACT3): `no-coverage` when the manifest declares
  no telemetry channel (permanent for this agent), `gated` when the metric
  is merely absent right now (cleared by null, not yet observed). A bundled
  `compact` action ships gated on `{ state = ["idle"], context_pct_min =
  75 }`, sending each agent's compact command (`/compact` Enter for Claude
  Code); users retune the threshold by shadowing the file. `--dry-run`
  prints the value and its age.
- **Notify.** `[notify] on` accepts `context_high` with a `threshold` key.
  It gets its own marker, `@agent_context_notified_at`, never the state
  lane's `@agent_notified_at` (that marker's dedup keys against state
  transitions, and bumping it out-of-band corrupts blocked / done
  arbitration). The marker is **not** an episode stamp compared against a
  `since` (there is no metric `since` to predate); its semantics are a
  present/absent armed flag, and the value is a timestamp only for
  debuggability:
  - marker absent = armed. Fire when a real observation lands at or above
    `threshold` while armed; the fire is a guarded set-from-absent with
    read-back, same shape as the lock acquire, so two concurrent firers
    (multiple attached clients each driving a status poll) resolve to one
    bell.
  - marker present = already fired. Rearm (unset the marker) when a real
    observation lands below `threshold - 10`.
  - a null observation clears the gauge but neither fires nor rearms; the
    flag holds until a real value decides it, so a `/compact` null cannot
    ring the bell or wedge it.
  - the notifier honors the torn-pair rule before deciding: a pct newer
    than its `at` is in-progress, defer to the next cycle.

  Two documented edges rather than mechanisms: a shallow compact that lands
  inside the hysteresis band (say 80 to 70 with threshold 75) does not
  rearm, by design, since the pane genuinely still sits near the line; and
  a `threshold` changed by config hot-reload applies from the next
  observation with the flag as-is, so a lowered threshold on an
  already-high pane fires only after the next real observation, and a
  raised one may leave the flag set until the pane dips below the new
  rearm line. Both are the cost of keeping the marker a single flag; a
  per-fire threshold record was rejected as a second store for a corner
  case.

**Revisit if** agents converge on a native statusline/telemetry hook (Codex
merged opt-in status-line items in #21324, but TUI-only; OpenCode's native
statusline request #8619 died auto-closed), which would collapse the channel
zoo toward the push path.

## ACT10: API-channel actions

Status: reviewed 2026-07-29 (batch 14 gate); normative.

**Question.** OpenCode answers a pending permission prompt over HTTP: `POST
/permission/:requestID/reply` with `{reply: "once"|"always"|"reject"}`
(shipped v1.1.1, documented; needs a reachable server). That is a keystroke-free
approve/deny lane, triggering ACT1's revisit clause: a third kind appeared that
neither keystrokes nor a spawned process expresses. How does an action reach it
without forking the action model?

**Analysis.** The lane is per-agent, not per-action: `approve` on a Claude pane
is still keystrokes, on an OpenCode pane it can be an API reply. A third `kind`
would force separate action names (`approve-api`), breaking the one-name
surface parity ACT4 exists for; the precedent is ACT1's `[keys]` table, already
per-agent for the same reason (sequences differ by agent). Free-form user HTTP
(URL + body templates in the manifest) was considered and rejected: it
reintroduces the interpolation/quoting swamp ACT3 closed, and every operation
worth having needs agent-specific request-id plumbing anyway. So the channel is
a closed vocabulary of built-in operations, extended only with evidence, like
key sequences.

**Decision.** The `keys` kind gains an optional per-agent `[api]` table; the
class stays "answer the prompt", only the transport differs per agent.

```toml
# bundled: approve.toml (amended shape)
name  = "approve"
label = "Approve"
kind  = "keys"
when  = { state = ["blocked"], detail = ["permission"] }

[keys]
claude = ["1"]
codex  = ["Enter"]

[api]
opencode = { op = "permission-reply", reply = "once" }
```

Normative details:

- **Applicability and exclusivity.** An action applies to the union of its
  `[keys]` and `[api]` agents. One agent in both tables is a parse error: the
  broker never chooses a transport at act time, so there is no silent fallback
  lane and no double-fire ambiguity. ACT1's structural rule relaxes to match:
  `kind = "keys"` requires at least one transport entry across `[keys]` and
  `[api]` (an api-only action is legal; both empty stays a parse error), and
  still forbids `command`/`detach`. If the API path fails, the invocation
  fails; degrading to keystrokes behind the user's back would fire a stale
  sequence into a pane whose prompt state just proved unknowable, the exact
  blind-send ACT2 exists to prevent.
- **Operation vocabulary.** `op` is closed; v1 ships exactly
  `permission-reply` (OpenCode), whose `reply` value is closed to
  `once`/`always`/`reject`. Unknown `op`, unknown `reply`, or `[api]` on an
  `exec` action are parse errors. New operations require a captured
  request/response pair recorded in agent-coverage.md, same evidence discipline
  as key sequences.
- **Request identity, evidence-stamped.** The pending request id lives in a new
  pane option `@agent_permission_request`, stamped by the event intake from
  `permission.asked` (the plugin forwards `request_id` alongside `session_id`;
  batch 13 captured the shape: `{sessionID, requestID, reply}` on the replied
  edge, the asked edge carries the same ids) under the ACT9 store discipline
  (ownership filter against `@agent_session`), and cleared on the edges that
  end the prompt: `permission.replied`, the `tool.execute.before` ⇒ working
  edge, and `session.idle` ⇒ idle. Resolving the id at act time from the
  server was rejected: no verified endpoint enumerates pending permissions,
  and the option read keeps the act path offline-checkable and testable.
- **Server endpoint, evidence-stamped with a config override.** The base URL
  lives in a new pane option `@agent_api_endpoint`, stamped at registration by
  the plugin if OpenCode's plugin API exposes the serving address (verify at
  implementation; batch 13 confirmed the server pins its own port, so a
  hardcoded default is wrong). Fallback: `[api.opencode] api_base` in
  config. Neither present ⇒ the broker refuses `requires-unmet` before
  acquiring the lock. `tma doctor` reports an OpenCode pane with a stamped
  request but no reachable endpoint.
- **Guard sequence unchanged.** ACT2 applies verbatim: identity → gate (the
  broker additionally treats an empty `@agent_permission_request` as
  `requires-unmet` for a `permission-reply` op; no new user-facing `requires`
  token) → lock acquire → gate re-assert under lock → one HTTP POST bounded
  by `timeout_ms` (connect and total; process-local, no retry) → release on
  every exit path. `--force` still skips `when` only.
- **Outcome vocabulary grows by one.** `replied`: the API answer was delivered
  (2xx). `sent` stays keys-only; overloading it would silently change a pinned
  meaning under scripts. A 404 (the prompt was answered or withdrawn between
  gate and act) maps to `vanished`, whose meaning widens from "pane
  disappeared" to "the act's target disappeared mid-act" (exit 3 unchanged;
  cli.md's exit-code table rewords its `3` row to match at implementation);
  server unreachable or non-2xx/404 is `error` (exit 1). The `outcome`
  value-set drift test extends to `replied`.
- **Contract fallout.** `approve`/`deny` gain the `[api]` block for OpenCode;
  `interrupt` stays keys-everywhere. pane-options-and-json.md documents the two
  new options; action-manifest-schema.md documents `[api]`; the `--list`
  document's `agents` array reports the union (no per-transport surface in v1,
  a deck does not care how the answer travels).

**Revisit if** a second agent ships an API lane whose operation is not a
permission reply (the vocabulary then earns a real registry), or OpenCode
exposes a pending-permission query that would let the broker drop the stamped
request option.

## Open questions

1. **Per-project actions.** A `.tma/actions/` dir in a repo would let a project
   ship its own side-quests, but executing repo-controlled commands on a
   keypress is a supply-chain door that needs a trust prompt design
   (direnv-style allow) before it exists at all. Deferred; user-dir only in v1.
2. **Key-sequence evidence.** Bundled `approve`/`deny` sequences must be
   captured per agent per version, like screen rules. Where the prompt offers
   numbered choices, "approve" means option 1 by convention; whether that is
   ever wrong in the wild needs the same fixture treatment as detection.
3. ~~**`act --menu` on a busy pane.** `display-menu` steals input focus;
   whether the menu should refuse (or warn) when the target pane is `working`
   is an ergonomics question to settle in the how-to, not a broker rule.~~
   Resolved in the how-to: the menu never refuses on `working` (interrupting
   a working agent is the flagship menu use, and entries already show only
   what is fireable); the focus steal is documented as deliberate, with the
   target pane running untouched underneath.
4. ~~**Deck read-path push.** The subscribe stream (ACT7) is deliberately not
   specified here; when written, it amends DAEMON.md and must keep the polling
   fallback contract-identical.~~ Resolved: DAEMON.md H20 specifies
   `tma subscribe --json`, a streaming CLI verb riding the H12 wake-hint
   subscription with a contract-identical poll fallback. Surfaces remain CLI
   consumers; the daemon is unchanged.
5. ~~**Statusline shim coexistence.** The chaining wrapper (ACT9) must survive
   the user editing their statusline afterward, and `install-hooks --check`
   must detect a shim that got overwritten. Same hazard class as the tmux
   hook-array clobber already documented in AD1.~~ Resolved: the shim
   recognizes itself by its forward marker, so editing the wrapped inner
   command keeps it intact; overwriting the whole command classifies as a
   detected clobber and a moved binary as stale, both surfaced by
   `install-hooks --check`, with unit tests pinning each classification.
6. **Codex rollout file discovery.** Mapping a pane to its rollout file needs
   the session id from the hooks.json channel plus the dated directory
   layout; whether that mapping is stable across Codex versions needs the
   fixture treatment before `file-tail` ships.

   *Batch-7 finding.* Discovery is implemented as a glob of
   `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*<session_id>*.jsonl` (session id in
   the filename, newest mtime wins), keyed off the hook-registered
   `@agent_session`. It is **fail-safe by construction**: a wrong or missing file
   stamps nothing, so the gauge stays absent/stale rather than wrong — the feature
   ships rather than gates off because a miss cannot produce a bad gauge. Two
   caveats remain open, not resolvable offline in this environment:
   - the **dated-layout / filename convention** matches the documented rollout
     format (the reverse-engineering writeup in the appendix) but was not
     re-verified against a live Codex install of the current version; a layout
     change degrades to "no file found" (gauge absent), never a mis-stamp;
   - the **`token_count` field choice** the parser uses
     (`info.total_token_usage.total_tokens` over `info.model_context_window`)
     follows ACT9's named fields, but whether `total_token_usage` is the current
     context footprint or the cumulative session usage (which would pin the gauge
     near 100%) needs live verification. The fixture is internally consistent and
     the parser isolates the field, so a correction is a one-line change; if the
     reading proves cumulative, the field moves to `last_token_usage.input_tokens`
     (the per-request context sent).
7. ~~**Window-size table drift.** `[telemetry.windows]` defaults rot as models
   ship. Acceptable because wrong entries only suppress or misscale a gauge,
   never state; still, `tma doctor` should flag a model name with no entry.~~
   Resolved: `tma doctor` warns on a stamped `@agent_model` with no window
   table entry (shipped or user-configured). The drift itself remains
   accepted as designed; the table currently seeds only `gemini-*` defaults
   and stays dormant until a raw-token channel (Gemini, Cursor) ships a
   parser.

## Relationship to existing decision records

- Extends AD4: the pane option store gains `@agent_action` (the
  single-flight lock, one option holding `expiry:nonce:pid:name`, ACT2),
  `@agent_context_pct` + `@agent_context_at` (the metric pair, ACT9), and
  `@agent_context_notified_at` (the context notify marker), all under the
  existing conditional-write grammar; no new store, and AD4's no-files rule
  stands unamended (ACT6 writes no logs, ACT9's tail persists no offset).
- Preserves the tier rule: the whole act path is tier 2; ACT6's detached
  completion uses the runtime notify primitive precisely so tier 3 stays
  optional.
- Mirrors AD2's philosophy: guards are deterministic and explainable
  (`--dry-run` is to actions what `debug explain` is to detection).
- ACT9 extends the evidence philosophy sideways: metrics are a parallel lane
  with the same manifest-declared, coverage-gated, fixture-tested discipline,
  and they never leak into the closed state vocabulary.

## Appendix: prior-art survey, context monitors (2026-07-27)

Condensed record of the web survey behind ACT9. Every row was verified against
the linked source on the survey date.

| tool | surface | context %? | mechanism |
|---|---|---|---|
| [agent-status-tmux](https://medium.com/google-cloud/agent-status-tmux-real-time-claude-code-gemini-cli-usage-in-tmux-f8a2fd0152f1) | tmux status-right | yes (Claude, Gemini) | patches `~/.claude/statusline.sh` to async-dump statusline JSON to a cache file; tmux polls it at 5s. Gemini via `AfterAgent` extension hook. One aggregate string, not per-pane |
| [ccstatusline](https://github.com/sirmalloc/ccstatusline), [ccusage statusline](https://ccusage.com/guide/statusline), [claude-powerline et al.](https://github.com/b-open-io/statusline) | Claude statusline | yes | statusline stdin JSON, `context_window.used_percentage`; ccstatusline infers 1M windows from model-name labels |
| [ocstatusline](https://github.com/amirlehmam/ocstatusline) | OpenCode statusline | yes | external daemon on the OpenCode SDK event stream (`opencode serve`); window sizes from `~/.cache/opencode/models.json`; event-driven repaint |
| [yolodeck / agentsd](https://github.com/paultyng/agentsd) | Stream Deck (Claude) | yes (`Context 146.2k / 967.0k (15%)`) | CLI-side session data; the macOS GUI variant shows context but exposes nothing externally |
| [Claude Control](https://eudaimoniasoftware.com/streamdeck) | Stream Deck (Claude) | yes (context ring) | permission prompts on keys plus live token/context meters; closest analogue to the threshold button, but no threshold-to-action loop |
| [AgentDeck](https://fernandobelotto.com/en/projects/agentdeck) | Stream Deck (Claude, Gemini, Cursor) | no (tokens/cost only) | agent hooks HTTP-POST to a local server on port 19847; session-log scanning for tokens |
| [VibeDeck](https://getvibedeck.com/) | Stream Deck, 6 agents | states + rate-limit usage, mechanism undisclosed | local-only; no per-agent extraction details published |
| [tmux-agent-indicator](https://github.com/accessd/tmux-agent-indicator) | tmux borders/titles | no | state only, output detection; confirms the per-pane context slot is empty |
| [ccusage codex](https://ccusage.com/guide/codex/) | CLI reports | no (cost focus) | parses `~/.codex/sessions` rollout JSONL `token_count` deltas |
| [Vibe Kanban](https://www.blog.brightcoding.dev/2026/07/17/vibe-kanban-the-revolutionary-ai-agent-manager-every-dev-needs) | web dashboard | yes (per-session bar) | per-agent session data; window inferred 200K/1M |

Channel facts that shaped ACT9, with the load-bearing sources:

- Claude statusline `context_window` field set and trigger model:
  [statusline docs](https://code.claude.com/docs/en/statusline); cumulative-fields
  bug fixed in v2.1.132:
  [#13783](https://github.com/anthropics/claude-code/issues/13783); window
  misdetection on 1M/custom models:
  [#36725](https://github.com/anthropics/claude-code/issues/36725),
  [#68522](https://github.com/anthropics/claude-code/issues/68522). Hook
  payloads carry no token counts; the statusline is the only live monitor.
- Codex rollout `token_count` events carry `total_token_usage` and
  `model_context_window`:
  [reverse-engineering writeup](https://dev.to/milkoor/reverse-engineering-codex-cli-rollout-traces-3b9b);
  missing in non-interactive sessions:
  [#9660](https://github.com/openai/codex/issues/9660); files reach gigabytes:
  [#24948](https://github.com/openai/codex/issues/24948); footer % replaced by
  a bar in v0.120.0: [#17874](https://github.com/openai/codex/issues/17874);
  opt-in statusline items merged:
  [#21324](https://github.com/openai/codex/issues/21324).
- Gemini exposes token telemetry via OTel
  ([telemetry docs](https://google-gemini.github.io/gemini-cli/docs/cli/telemetry.html)),
  cumulative counters only; live context rides the `AfterAgent` hook or
  session files under `~/.gemini/tmp`.
- Cursor: `stop` carries turn-granularity token counts, but the live per-turn
  gauge rides an undocumented `statusLine` command in `~/.cursor/cli-config.json`
  (`{"type":"command","command":"…","padding":N}`) whose stdin payload carries a
  `context_window` object (`total_input_tokens` + `context_window_size`) — the
  channel `cursor-statusline-json` parses. Real-but-undocumented (confirmed live
  2026-07-29, absent from Cursor's config reference), so it is the highest-churn
  channel and the parser fails safe (a missing field degrades to an absent gauge).
  Live-context-unexposed request predates the finding:
  [forum thread](https://forum.cursor.com/t/show-tokens-count-always-update-real-time-as-its-spent/129311).
- OpenCode native statusline is dead:
  [#8619](https://github.com/anomalyco/opencode/issues/8619) auto-closed, the
  implementing PR #13885 closed unmerged (2026-07-29 survey).

Gaps the survey confirmed unclaimed, which ACT9 targets: per-pane context as
addressable tmux state, one cross-agent normalized schema, and the
threshold-to-action loop.
