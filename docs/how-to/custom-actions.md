# Author a custom action

An action is a guarded thing `tma` does to an agent pane: send a key sequence, or
run a command with the pane's context in its environment. You declare one in a
TOML file, dry-run it until it looks right, then fire it from any surface. This
guide writes an `exec` action that asks a live Claude session for a progress
summary; for the full field list see
[Action manifest schema](../reference/action-manifest-schema.md).

## Drop the manifest

User actions live in `~/.config/tma/actions/`. The file stem is the action name,
so `summarize.toml` is `tma act summarize`:

```toml
# ~/.config/tma/actions/summarize.toml
min_engine_version = "0.1"
name = "summarize"
label = "Summarize progress"
kind = "exec"
agents = ["claude"]
when = { state = ["working", "idle"] }
requires = ["session"]
confirm = true
command = "~/.config/tma/actions/summarize.sh"
```

`agents` limits it to Claude panes. `when` gates it to `working` or `idle`.
`requires = ["session"]` refuses cleanly if the pane never registered an agent
session id, so the script never runs against an empty `TMA_SESSION_ID`.

## Write the command

The command is passed to `sh -c` verbatim. It learns which pane it serves only
through environment variables, never through the command string, so there is no
quoting injection from a pane title. Fork the session so the user's live TUI is
untouched:

```sh
#!/bin/sh
# ~/.config/tma/actions/summarize.sh  (chmod +x)
set -eu
claude -p --resume "$TMA_SESSION_ID" --fork-session \
  "Summarize progress and list open questions" \
  | tma-notify-or-your-own-sink
```

The available variables are `TMA_PANE`, `TMA_AGENT`, `TMA_STATE`, `TMA_DETAIL`,
`TMA_SESSION_ID`, `TMA_CWD`, `TMA_PID`, `TMA_LOCATOR`, `TMA_TITLE`, and
`TMA_ACTION`, plus the caller's `--arg` values (below). Quote every one of them: `"$TMA_TITLE"`, not `$TMA_TITLE`. A title
is text the agent printed, so an unquoted expansion re-parses hostile input. The
command runs in tma's own working directory, not the pane's; a script that needs
the agent's directory uses `TMA_CWD` explicitly (`cd "$TMA_CWD"`). [The security
model](../explanation/security-model.md#why-action-context-arrives-as-environment)
explains what the env transport does and does not protect.

## Dry-run, then fire

`--dry-run` resolves everything and executes nothing: the context env with each
value's age, the gate verdict, and the command that would run. It is to actions
what `tma debug explain` is to detection.

```sh
$ tma act summarize --pane %5 --dry-run
action:  summarize
pane:    %5
agent:   claude
gate:    fireable
effect:  command: ~/.config/tma/actions/summarize.sh
context:
  TMA_SESSION_ID  0d1a...  (1200 ms old)
  TMA_CWD         /home/you/proj  (live)
```

Edit, dry-run, fire, with no rebuild. When it looks right, fire it. `confirm =
true` means a non-interactive fire needs `--yes`:

```sh
$ tma act summarize --pane %5 --yes
```

## Fire past the gate: `--force`

`--force` skips the `when` gate, and only the `when` gate. It is for the case
where you know better than the stamp: the pane reads `working` because a hook
missed its `Stop`, and you want to act anyway rather than wait for the next cycle
to correct it.

```sh
$ tma act summarize --pane %5 --force --yes
```

Everything else still holds. `requires` is still checked, so an action needing a
session id still refuses `requires-unmet` (exit 4) on a pane that never registered
one. The single-flight lock is still taken, so `--force` cannot run two copies at
once. Identity still applies: an action declaring `agents = ["claude"]` still
refuses on a codex pane. `--force` is not `--yes`, either; a `confirm` action needs
both.

One thing `--force` skips that is easy to miss: a `keys` action normally
re-verifies a stale pane on demand before gating. Under `--force` there is no gate
to verify for, so no re-verification happens and the keys go out against whatever
the pane looks like now.

## Pass a value in: `--arg`

Some actions need a payload from the caller, not just the pane's context. `--arg`
carries one (repeat it for more), and it travels the same way everything else
does — as environment, never spliced into `command`:

```toml
# ~/.config/tma/actions/queue-next.toml
min_engine_version = "0.1"
name = "queue-next"
label = "Queue the next task"
kind = "exec"
agents = ["claude"]
when = { state = ["idle"] }
requires = ["session"]
confirm = true
command = "~/.config/tma/actions/queue-next.sh"
```

```sh
#!/bin/sh
# ~/.config/tma/actions/queue-next.sh  (chmod +x)
set -eu
[ -n "${TMA_ARG:-}" ] || { echo "queue-next needs --arg <task>" >&2; exit 2; }
# The value is data: it reaches the agent as an argument, never as shell source.
tmux send-keys -t "$TMA_PANE" -l -- "$TMA_ARG"
tmux send-keys -t "$TMA_PANE" Enter
```

The script gets `TMA_ARG` (the first value), `TMA_ARG_1..N` and `TMA_ARG_COUNT`
when several were passed, and nothing at all when none was. A value that contains
`$(reboot)` stays those nine characters: nothing expands it, because nothing ever
builds a command string out of it. Quote it anyway (`"$TMA_ARG"`), as you would
`TMA_TITLE`.

`keys` actions refuse `--arg` (exit 2) on purpose. A `keys` sequence lives in the
manifest, which is what makes it reviewable; an action that types caller text into
a live session is an `exec` action whose script owns that decision — and should
set `confirm = true`, because it writes.

Driving it from a wait loop is the orchestrator shape:

```sh
#!/bin/sh
set -eu
pane=%5
since=0
while read -r task; do
  row=$(tma wait --pane "$pane" --until idle --since "$since" --json --timeout 900) || exit $?
  since=$(printf '%s' "$row" | sed 's/.*"since_ms":\([0-9]*\).*/\1/')
  tma act queue-next --pane "$pane" --arg "$task" --yes
done < tasks.txt
```

`--since` is what keeps that loop honest: without it the second `wait` returns on
the same idle episode it just fed. See
[Block a script on agent state](block-a-script-on-agent-state.md#drive-a-supervisor-loop).

## Fire on a whole fleet

`--all` turns the [selector flags](../reference/cli.md#selector-flags) into the
target set instead of a uniqueness requirement, firing on each matched pane in
turn. Dry-run it first: with `--all`, `--dry-run` prints the resolved targets and
what each verdict would be, which is the blast radius before it happens.

```sh
$ tma act summarize --all --repo tmux-agents --dry-run
targets: 3
%1       claude       would fire
%4       claude       refused: gated
%7       claude       refused: locked

$ tma act summarize --all --repo tmux-agents --yes
```

Each target runs the full broker sequence on its own — its own single-flight
lock, its own gate re-verification at fire time — so one pane's refusal neither
skips nor weakens the others. The confirmation is asked once for the batch, and
the process exits with the worst target's code, so `&&` still means "all of them
acted".

The broker re-verifies the pane is still a Claude pane in a gated state, acquires
a single-flight lock so a double-press cannot run two summaries at once, spawns
the command, and releases the lock. Exit `0` means the child exited `0`; a gate
refusal is exit `4`, a held lock exit `5`. The full table is in
[`tma act`](../reference/cli.md#tma-act).

## Bound the run: `timeout_ms`

A synchronous `exec` action runs under a deadline. `timeout_ms` is it, in
milliseconds, defaulting to `30000`. A child still alive at the deadline is killed
and the action ends with outcome `timeout` and exit `124`, the same code
`timeout(1)` uses, so a caller branches on it the way it already branches on a
`tma wait` timeout.

```toml
timeout_ms = 120000   # two minutes for a slow summary
```

Set it to what the command genuinely needs. The value also sets the single-flight
lock's expiry (the deadline plus a few seconds of slack), so an over-generous
timeout on a command that hangs leaves the pane locked for that long against other
fires, and a too-tight one turns a slow success into a `124`. `detach = true` uses
`detach_timeout_ms` instead; both are in the [action manifest
schema](../reference/action-manifest-schema.md).

## Long-running actions

An SDK call can take minutes. Set `detach = true` and the broker returns
immediately (exit `0`, outcome `spawned`) while a tma-owned supervisor holds the
lock, kills the process group at `detach_timeout_ms`, and fires a completion
notification through your `[notify]` command when the child exits. A detached
action is fire-and-forget: its exit code says nothing about the child's outcome,
which arrives on the completion payload instead. Use a synchronous action (the
default) when a script needs to branch on the result.

## Recommend `confirm` for anything that writes

tma cannot inspect what your script does. Set `confirm = true` for any action
that injects into a live session or mutates a repo, so a stray keypress or a
script cannot fire it unattended. It costs one `--yes` (or one menu keystroke) and
is the honest place to declare "this one is not idempotent".

## Control surfaces

Every surface fires the same verb, so an action is written once and reachable four
ways: a `tma act` shell line, the tmux menu (`tma act --menu`, wired to a key by
[`tma install-keys`](../reference/cli.md#tma-install-keys)), a keybinding, and a
hardware deck. Nothing works on a deck that a keyboard-only tmux user cannot reach
through the menu; if a flow needs hardware, it is a bug.

From the picker and from `tma watch`, `a` is the triage key: it opens the menu
for the agent under the cursor rather than the pane you are standing in, so a
screenful of blocked agents is answered from one place. Two things follow from
the menu being computed fresh, on the target pane. If nothing is fireable there
right now, no menu opens at all, and `tma act --list --pane <id>` says why. And
the menu is handed to tmux rather than run as a child of the dashboard, so it
outlives the surface that asked for it, which is what lets the popup-hosted
picker offer the key: a tmux menu replaces the popup on screen and the action
still fires.

The menu opens over your client and captures your keystrokes until you pick an
entry or dismiss it; the target pane keeps running underneath, untouched. That
focus steal is deliberate and the menu never refuses to open on a `working`
pane: interrupting a working agent is the flagship menu use, and the entries
already show only what is fireable right now. If you were mid-sentence into the
pane when you opened the menu, finish the menu first — keys you type go to it,
not the pane.

A surface stays a dumb reader on the act path exactly as on the read path. A deck
or plugin enumerates actions with `tma act --list --json --pane %N` and renders
them: fireable ones lit, gated ones dark, each carrying a `reason` so the plugin
can gray-out-temporarily (`gated`) versus gray-out-permanently (`no-coverage`).
It then shells out to `tma act <name> --pane %N` and contains zero policy. The
document's exact key set is in
[Pane options and JSON contracts](../reference/pane-options-and-json.md#tma-act-list-document).

To know *when* to re-render, spawn `tma subscribe` instead of running your own
polling timer: see [Stream state changes](stream-state-changes.md).
