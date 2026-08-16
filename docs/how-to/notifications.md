# Set up notifications

Get alerted when an agent needs you, even while you are looking at another
window. `tma` fires a notification on a state transition you choose. The signal
can be a terminal bell, an external command you supply, or both.

Notifications are configured under `[notify]` in your config file. For the full
key reference see [Configuration](../reference/configuration.md#notify-notifications).

## Choose the triggers

`notify.on` is the set of transitions that fire. The default is blocked only; add
`done` to also fire when a working agent finishes (goes idle with unreviewed
output):

```toml
[notify]
on = ["blocked", "done"]
```

`blocked` covers "an agent is waiting on you"; `done` covers "an agent finished
while you were elsewhere". These are the two moments worth interrupting for.

## Ring the terminal bell

The simplest signal needs no external tooling. It rings the bell of the pane the
agent is in, which most terminals and tmux surface as a visual or audible alert:

```toml
[notify]
on = ["blocked", "done"]
bell = true
```

## Post a desktop notification from the terminal

`notify.osc` writes an [OSC 9](https://iterm2.com/documentation-escape-codes.html)
notification sequence to the firing pane's tty, which the terminal emulator turns
into a real desktop notification. Like the bell it travels down the connection, so
it works over ssh, mosh, and tmate: the emulator you are sitting at is what
renders the banner, no matter where tmux runs.

```toml
[notify]
on = ["blocked", "done"]
osc = true
```

It is off by default because emulator support varies (WezTerm, kitty, and iTerm2
handle OSC 9; an emulator that does not understand it ignores the sequence
silently). The text is short and fixed, `<agent> <state>` — for example `claude
blocked`. It deliberately omits the pane title: a title is written by whatever
runs in the pane, and an escape sequence is a poor place for text tma does not
control.

The tmux status-line message that accompanies every fire now goes to **every**
attached client, so both terminals in a pairing setup see it, not just the one
that was active most recently.

## Run a command

For a real notification (desktop banner, phone push), set `notify.command`. `tma`
runs it and pipes a JSON object to its stdin describing the transition. The
payload carries metadata only, never captured screen content; its exact key set
(`agent`, `pane`, `state`, `detail`, `session`, `locator`, `title`, `repo`,
`branch`, `since_ms`, `context_pct`, plus a `schema` version) is documented in
[Notification hook payload](../reference/pane-options-and-json.md#notification-hook-payload).
`repo` and `branch` come from the pane's working directory, so a message can say
which checkout is waiting on you without your hook shelling out to git.

```toml
[notify]
on = ["blocked", "done"]
command = "~/.local/bin/tma-notify"
```

### Example: macOS desktop banner

Save this as `~/.local/bin/tma-notify` and `chmod +x` it. It reads the payload and
posts a native notification with `osascript`:

```sh
#!/bin/sh
# reads tma's notify payload on stdin
payload=$(cat)
agent=$(printf '%s' "$payload" | jq -r '.agent')
state=$(printf '%s' "$payload" | jq -r '.state')
locator=$(printf '%s' "$payload" | jq -r '.locator')
osascript -e "display notification \"$agent is $state\" with title \"tma\" subtitle \"$locator\""
```

### Example: push to your phone with ntfy

The same script shape, pushing to an [ntfy](https://ntfy.sh) topic so a blocked
agent buzzes your phone:

```sh
#!/bin/sh
payload=$(cat)
agent=$(printf '%s' "$payload" | jq -r '.agent')
state=$(printf '%s' "$payload" | jq -r '.state')
locator=$(printf '%s' "$payload" | jq -r '.locator')
curl -s \
  -H "Title: $agent is $state" \
  -H "Tags: warning" \
  -d "$locator" \
  https://ntfy.sh/your-topic-name > /dev/null
```

### Send each trigger somewhere different

`notify.command` is the fallback for every trigger. Each one can also name its
own command in a `[notify.<trigger>]` sub-table, which is what you want when the
triggers are not equally urgent: a blocked agent is worth a phone push, a
completion is worth a line in a file.

```toml
[notify]
on = ["blocked", "done"]

[notify.blocked]
command = "curl -s -d \"$TMA_AGENT blocked in $TMA_LOCATOR\" https://ntfy.sh/your-topic"

[notify.done]
command = "cat >> ~/.local/state/tma/done.jsonl"
```

`context_high` takes a `command` the same way, beside its `threshold`. A trigger
with no sub-table, or a sub-table that sets no `command`, falls back to the global
`notify.command`, so routing one leaves the others alone. An unknown key inside a
sub-table is a parse error rather than a silent fallback.

`tma debug notify-test --trigger done` runs whichever command that trigger
actually resolves to, which is the quickest way to confirm the routing landed.

### When nothing arrives

A fire runs your command in the background and discards its output, so a typo'd
path or a script that exits non-zero produces silence rather than an error. Two
places surface it:

```sh
tma debug notify-test --trigger blocked   # run it now, see stderr and the exit code
tma doctor                                # reports the last failure a real fire hit
```

`notify-test` builds a representative payload, runs the command the trigger
resolves to, waits for it, and prints what happened. Whenever a real fire's
command cannot start or exits non-zero, tma records that one failure; `tma doctor`
prints it (with the reason and the command), and the next clean fire clears it.

## Notify on high context

`context_high` fires when a pane's context-window utilization crosses a threshold,
so you learn a session is nearly full without watching a gauge. It is separate
from `on`: name it as a sub-table with a `threshold` percent.

```toml
[notify]
on = ["blocked"]
context_high = { threshold = 75 }
```

It fires once on the crossing and then holds: staying high does not re-ring, and
it rearms only after the gauge dips below `threshold - 10` (a shallow compact that
lands inside that band leaves it silent, by design). The payload's `state` field
carries `context_high` so your hook can tell it from a `blocked` or `done` alert.
The gauge itself comes from a telemetry channel the agent's manifest declares, so
`context_high` is silent for an agent with no context coverage. It rides the same
marker and command as the other triggers; the full key reference is in
[Configuration](../reference/configuration.md#notify-notifications).

## Silence one pane for a while

Config decides what fires everywhere; `tma mute` silences a single pane you are
not currently interested in.

```sh
tma mute                       # the current pane, until you clear it
tma mute --for 30m             # …for half an hour (45s / 2h / 1d also parse)
tma mute --session build       # every agent pane in that session
tma mute --clear --session build
```

Mute suppresses the *fire* and nothing else: every sink stays quiet (the
`display-message` line, `bell`, `osc`, the `[notify] command`, `context_high`
included) while detection, stamping, and the `tma status` counts carry on
exactly as before, so the pane still reads `blocked` in `tma ls` and in the JSON
(where the row gains `"muted": true`). Nothing is queued either — a mute that
expires mid-episode does not then ring for the transition it silenced. A detached
action's completion still reports, since that one you asked for.

The deadline is stored in the pane itself (`@agent_mute_until`), so it outlives a
`tma` restart, a daemon stop, and a `tma reload`; killing the pane is the other
way to end it. Full flag reference: [`tma mute`](../reference/cli.md#tma-mute).

## Keep a history

Two records answer two different questions.

**What was sent.** `notify.log` appends one JSON line per fired notification:

```toml
[notify]
on = ["blocked", "done"]
log = "~/.local/state/tma/notifications.jsonl"
```

Each line is the hook payload plus an `at` field with the fire time in epoch
milliseconds; a detached action's completion is logged the same way, carrying
`action` and `outcome` where a state line carries `state`. It is written by whichever process fired (daemon or hook), the
parent directory is created for you, `~` is expanded, and the file is appended to,
never rewritten. A log that cannot be written is skipped silently, since a hook
must never fail on its notifications.

```sh
jq -r 'select(.state=="blocked") | "\(.at) \(.repo)@\(.branch) \(.locator)"' \
  ~/.local/state/tma/notifications.jsonl
```

**What changed.** The daemon keeps a richer in-memory ring of the last 256 state
transitions, including the ones that never fired a notification:

```sh
tma debug transitions          # human-readable, oldest first
tma debug transitions --json   # {"schema":1,"cap":256,"recorded":N,"transitions":[...]}
```

That ring is daemon memory: it needs a running daemon and starts empty after a
restart. The log file is the durable one. Use the ring to answer "what did tma
observe", the log to answer "what did it tell me about".

## Notifying from a remote host

`notify.command` runs on the machine running tmux. Over ssh that is the remote
box, which is why the usual desktop recipes go quiet: `osascript` has no
Aqua session to talk to, and `notify-send` has no D-Bus session bus. Neither
errors in a way you would notice, so it looks like tma stopped firing.

Three things do work across a connection:

- **The bell.** `bell = true` writes a BEL to the pane's tty, which travels down
  the ssh/mosh/tmate connection like any other output, and your local terminal
  (or tmux's `monitor-bell`) reacts.
- **The OSC sink.** `osc = true` does the same with an OSC 9 sequence, so a
  supporting emulator raises a real desktop notification on the machine you are
  sitting at. Zero configuration on the remote side.
- **A push service.** Send the notification out over the network instead of to a
  desktop. [ntfy](https://ntfy.sh) is the smallest version — the whole payload
  is on stdin, so a one-liner works:

  ```toml
  [notify]
  on = ["blocked", "done"]
  command = "curl -s -H \"Title: $TMA_AGENT $TMA_STATE\" -d \"$TMA_REPO $TMA_LOCATOR\" https://ntfy.sh/your-topic-name > /dev/null"
  ```

  Pick an unguessable topic name: an ntfy topic is public to anyone who knows it.

If you are attached to the remote tmux from a local one, remember that the
status-line message goes to every attached client, so a second terminal watching
the same session sees it too.

## Daemonless vs daemon

The command fires from whichever process observes the transition, and by default
only a running daemon dispatches notifications (it is the resident process that
can watch for a transition and fire it). To fire from a hook directly with no
daemon, opt in:

```toml
[notify]
from_event = true
on = ["blocked", "done"]
command = "~/.local/bin/tma-notify"
```

With `from_event = true`, `tma event` fires the notification itself as the hook
lands, before exiting. This covers hook-capable agents with no background process.
For hookless agents, and for deduplicated notifications across every detection
path, run the daemon; see [run-the-daemon](../how-to/run-the-daemon.md).
