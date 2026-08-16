# Read agent state from a status bar or script

Every verdict tma reaches is a tmux pane option. Anything that can ask tmux a
question can read it — a status bar, a prompt, a shell script, another TUI — with
no API, no socket, and no dependency on tma at read time.

## Read the option

Two forms, same data. A format string, anywhere tmux expands one:

```tmux
set -g pane-border-format '#{pane_index} #{@agent_state}'
set -g window-status-format '#I:#W #{@agent_summary}'
```

Or a one-shot read from a script:

```sh
$ tmux show-options -pqv -t %5 @agent_state
blocked
$ tmux show-options -pqv -t %5 @agent_detail
permission
```

`-q` keeps an unset option quiet (an agentless pane simply prints nothing), and
`-v` drops the key so you get a bare value. The full option list, with the grammar
of each value, is in [Pane options and JSON
contracts](../reference/pane-options-and-json.md#pane-option-schema). Values are
machine tokens (`idle`, `working`, `blocked`, `unknown`) and epoch milliseconds,
never glyphs — the rendering is yours to do.

For rollups, two options save you the aggregation: `@agent_summary` on each window
and `@agent_session_summary` on each session, both carrying the same
`<state>:<count>` grammar in a fixed order with zero counts omitted:

```sh
$ tmux show-options -qv -t dev @agent_session_summary
blocked:1 working:2
```

They are maintained by the same writers that stamp the panes, so a per-session
indicator costs one format expansion and no process at all.

## Do not shell out to `tma` per redraw

`tma status` and `tma ls` are not readers. Every invocation runs a full poll
cycle: it lists panes, walks the process table, captures screens where the fold
needs them, and stamps what it finds, and only then prints. That is exactly what
you want on a 1-to-5-second interval — it is how state stays fresh — and exactly
what you do not want on every prompt redraw or every keystroke in a fast loop.

The rule of thumb: read the **options** as often as you like (a tmux format
expansion, no process), and run **`tma`** on a timer.

## When you want the whole row

The options carry one pane's fields; `tma ls --json` carries the resolved row —
locator, title, repo and branch labels, the context gauge, the `done` surface —
as a versioned schema-1 document. Scope it to one pane with `--pane`:

```sh
$ tma ls --pane %5 --json
{"schema":1,"agents":[{"pane":"%5","agent":"claude","state":"blocked",…}]}
```

It prints an empty `agents` array (exit 0) for a pane with no agent, so a reader
never has to special-case a missing pane. The key set and its null rules are
pinned in [Pane options and JSON
contracts](../reference/pane-options-and-json.md#tma-ls---json); keys are only
ever added, never renamed or dropped.

If what you want is a stream rather than a poll, `tma subscribe` emits one line
per change and is a driver itself for as long as it runs; see [Stream state
changes](stream-state-changes.md).

## The freshness caveat

Pane options are a store, not a feed. They hold the last verdict some producer
reached, and **nothing refreshes them on your read**: `show-options` does not run
a cycle, and neither does a format expansion. If nothing else is driving tma on
that server — no `#(tma status)` in the status line, no daemon, no other tma
command — you are reading a snapshot of whenever the last one ran, which may be
minutes or hours old.

Two facts make this manageable:

- **Every stamp is dated.** `@agent_stamped_at` is the per-pane freshness marker,
  and a reader that cares can compare it against the clock and grey out a value it
  no longer trusts. The `watch` table does exactly that with the context gauge.
- **Any `tma` invocation is a driver.** A slow-timer `tma status` in your bar is
  not just a printer; it runs the same cycle the status line does, so it keeps
  every pane on the server fresh for every other reader, including your
  option-reading ones. So does `tma ls`, or a cron `tma status --format prom`.

The practical shape, then: one cheap driver on a timer, everything else reading
options. If your bar already polls `tma status --format plain` every five seconds,
your other readers are covered by it and need no timer of their own. If nothing
polls, `tma doctor` says so:

```
ambient: NOT polling — nothing invokes `tma status`; add `#(tma status)` to status-right (required ambient driver)
```

See [Drive an external bar](drive-an-external-bar.md)
for the driver recipes, and [Run the daemon](run-the-daemon.md) for the two setups
where a driver alone is not enough.
