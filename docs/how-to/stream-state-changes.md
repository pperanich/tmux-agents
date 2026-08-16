# Stream state changes

`tma subscribe` is the push side of the read path. One long-running process
prints one JSON line per emission on stdout and holds a connection to nothing but
the `tma` binary, so a plugin, a bar, or a logger stops owning a polling timer:

```sh
tma subscribe --json
```

`--json` is required; it is the only emission today, and leaving it off is a
usage error (exit 2). Every flag is in [`tma
subscribe`](../reference/cli.md#tma-subscribe).

## Snapshots or transitions

The two modes answer different questions, and the choice decides everything else
on this page.

**Snapshots** are the default: each line is a complete `ls --json` schema-1
document, the same one [`tma ls --json`](../reference/cli.md#tma-ls) prints. Use
it when your consumer renders the current world and does not care how it got
there. A re-render needs no memory of the previous line.

```
$ tma subscribe --json
{"schema":1,"agents":[{"pane":"%5","agent":"claude","state":"blocked",…}]}
```

**`--events`** emits one record per state transition instead, one object per
line:

```
$ tma subscribe --json --events
{"schema":1,"at_ms":1786900866503,"pane":"%5","agent":"claude","from":"working","to":"blocked","detail":"permission","locator":"work:1.0","repo":"app","branch":"main"}
```

Use it when the transitions themselves are the data: a log, a counter, a
notification hook of your own. `from` and `to` are the disjoint reading of state,
so a finished-but-unreviewed pane is `done` rather than `idle`, and clearing
attention by jumping to it is a real `done` → `idle` edge. A pane that appeared
since the last cycle carries `"from": ""` and one that vanished carries `"to":
""`; the empty string rather than `unknown` is what lets you tell "the pane is
there and unreadable" from "there is no pane". A pane whose state held still
emits nothing even if its title or detail moved, because this is a transition
stream and not a change feed. The full key table is in
[`--events`](../reference/cli.md#--events).

## Do not repeat yourself with `--changes-only`

With no daemon the stream polls on `--interval` (default one second) and emits
every tick whether or not anything moved. A consumer that re-renders does not
care. A consumer that *appends* very much does: a daemonless logger writing to a
file collects 86,400 identical lines a day.

```sh
tma subscribe --json --changes-only
```

`--changes-only` makes the poll tick behave the way the push-mode belt already
does and emit only when the document differs from the last one sent. It is a
silent no-op in push mode and under `--events`, both of which are edge-triggered
by construction, so a script never has to know which mode it landed in. The entry
snapshot is always emitted.

## What the stream does not promise

Four properties are worth designing around before you build on the stream. They
are the same in push and poll mode, because every line is built from the
subscriber's own cycle rather than from the daemon socket.

- **There is no replay.** A subscriber sees what happens from the moment it
  starts. There is no backlog, no cursor, and no way to ask for what you missed
  while your consumer was restarting. Whatever happened while it was down is
  gone.
- **The first cycle is silent under `--events`.** No synthetic edges are invented
  for panes that were already running, so restarting a logger does not stamp a
  fresh "appeared" line on every long-lived agent. In snapshot mode the first
  line is the current document, not an event. If you need the state you began
  from, run `tma ls --json` once alongside the stream.
- **Fast flips can collapse.** Transitions inside the 100 ms coalescing window
  are observed as one cycle, so a state an agent held for 50 ms may never appear.
- **The degrade is silent.** No daemon, a daemon dying mid-stream, or a daemon
  too old to answer drops the stream to unconditional `--interval` polling with
  nothing on stderr and no exit. Only latency changes. [`tma
  doctor`](diagnose-with-doctor.md) is what tells you which mode you are in.

There is no heartbeat either: process death is the liveness signal. The stream
exits only on a signal or when its stdout closes, so a consumer that owns the
process respawns it on EOF.

## Drive a plugin or a bar

A surface stays a dumb reader here exactly as it does on the option-reading path.
Spawn the stream once, re-render on each line:

```sh
#!/bin/sh
# sketchybar item fed by the stream instead of a 5-second timer.
tma subscribe --json --changes-only | while IFS= read -r doc; do
  blocked=$(printf '%s' "$doc" | jq '[.agents[] | select(.state == "blocked")] | length')
  sketchybar --set agents label="⚑$blocked"
done
```

With a daemon running that repaints within milliseconds of the block rather than
at the next tick of a timer; with no daemon it repaints on the `--interval` poll,
from identical documents. Scope it with the [selector
flags](../reference/cli.md#selector-flags) when a surface only cares about part
of the fleet: `--repo app` narrows the `agents` array without changing the
cadence or the push/poll contract. Under `--events` the filter is applied
*before* the diff, so a pane leaving the selection reads as a departure and one
entering it as an appearance.

A plugin that also offers actions re-runs [`tma act --list --json --pane
%N`](custom-actions.md#control-surfaces) for a pane the emission shows changed,
and shells out to `tma act <name> --pane %N` to fire. It holds no policy of its
own either way.

The stream is an ambient driver for as long as it lives: its cycles stamp every
pane on the server, so a bar fed this way keeps state fresh for every other
reader and needs no `#(tma status)` beside it for freshness.

## Log every transition to jsonl

`tma wait` blocks for one thing. For a record of everything instead, what got
blocked, for how long, in which repo, `--events` appends straight to a jsonl
file:

```sh
tma subscribe --json --events >> ~/.local/share/tma/events.jsonl
```

Ordinary line tools work on it. How many blocks did each agent hit today:

```sh
jq -r 'select(.to == "blocked") | .agent' events.jsonl | sort | uniq -c
```

The stream is edge-triggered, so an idle server writes nothing at all and there
is no interval to tune for a quiet day. Run it under a supervisor that restarts
it, and remember that the gap while it was down is not recoverable.

To log a snapshot stream instead, one full document per change rather than one
record per transition, pair `--changes-only` with the default mode:

```sh
tma subscribe --json --changes-only >> snapshots.jsonl
```

## Next

- [Block a script on agent state](block-a-script-on-agent-state.md) when you want
  to wait for one thing rather than watch everything.
- [Read agent state from a status bar or
  script](read-agent-state-from-a-status-bar-or-script.md) for the pane-option
  path, which needs no `tma` process at all at read time.
- [Run the daemon](run-the-daemon.md) to put the stream on pushes instead of a
  poll.
