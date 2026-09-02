# Show agents in your status line

Put the agent counts in tmux's own status line, and get the ambient state driver
in the same move. This is one line you add yourself; `tma install-keys` never
edits your `status-right`.

## Add the driver

```tmux
set -g status-right '#(tma status) %H:%M'
```

`#(tma status)` is not optional decoration. It is the ambient driver: it runs
every `status-interval`, refreshes each pane's stamped state, and prints the
counts. Without it, ambient surfaces (window flags, per-window summaries) render
nothing, and with no daemon nothing keeps state fresh between your explicit
commands.

`tma status` prints state counts with glyphs and tmux color codes, which tmux
renders inline. One blocked and one working agent:

```
$ tma status
#[range=user|tma:blocked]#[fg=red]⚑1#[norange] #[range=user|tma:working]#[fg=yellow]●1#[norange]
```

The order is fixed (`blocked working done idle unknown`), zero-count classes are
omitted, and a server with no agents prints nothing at all, so tma adds nothing
to your status line until it has something to say. The `#[range=…]` markers are what make
each segment clickable; tmux draws nothing for them, and without the opt-in mouse
bindings nothing acts on them ([Install the
keybindings](install-the-keybindings.md#clickable-status-segments)). Glyphs and
colors come from `[status]` config; see
[Configuration](../reference/configuration.md#status-and-picker-glyphs-and-colors).

## What a refresh costs

The driver is not a read-only renderer. Refreshing a pane's stamped state means
`tmux set-option`, and tmux redraws every attached client in full on any option
write, even one whose value did not change. That is a whole-screen re-emission
rather than the single status row a plain status tick writes, so it is worth
knowing when it happens:

- **An idle agent costs nothing.** With nothing written to its window since its
  last stamp, the cycle reuses the stored verdict and issues no write at all.
- **A working agent costs one redraw per cycle.** Its screen keeps changing, so
  it is restamped every time. The stampede hint rides that same invocation
  instead of costing a second one.
- **The status string itself is cheap.** tmux skips the write when the expanded
  `status-right` is identical to what is already drawn, so a static string sends
  your terminal nothing no matter how short `status-interval` is. A `%H:%M`
  clock costs one status-row write per minute, about 165 bytes.

A full redraw puts every wrapped line back through your terminal's own wrap
logic, so a terminal that disagrees with tmux about a glyph's width can visibly
re-wrap a pane during one. If you see that, upgrade tmux: 3.7 corrects the
redrawing of wide characters when they are overwritten and lets
`codepoint-widths` accept ranges. The upstream report for the agent-TUI side of
this is
[anthropics/claude-code#91182](https://github.com/anthropics/claude-code/issues/91182).

## Roll up the agents in each window

The driver also maintains a window-scoped `@agent_summary` option, so a
per-window rollup costs no extra process:

```tmux
set -g window-status-format '#I:#W #{@agent_summary}'
```

## Scope it to one session

The counts `tma status` prints obey the [selector
flags](../reference/cli.md#selector-flags), so scoping the driver to the current
session is one flag:

```tmux
set -g status-right '#(tma status --session #{session_name}) %H:%M'
```

The cheaper alternative, if all you want is counts per session, is the option the
driver already maintains:

```tmux
set -g status-right '#{@agent_session_summary} %H:%M'
```

It carries the same `<state>:<count>` grammar as the per-window `@agent_summary`,
in machine tokens rather than glyphs (see [Pane options and JSON
contracts](../reference/pane-options-and-json.md#pane-option-schema)).

Two things to know before scoping the `#()` driver:

- **tmux caches `#()` jobs per expanded command string.** Each distinct
  `tma status --session <name>` is a separate long-lived job, so N attached
  sessions means N `tma` processes on every `status-interval` instead of one.
  That is fine for a handful of sessions and wasteful for dozens.
- **A filtered driver still refreshes everything.** The selector narrows only
  what is printed; the cycle behind it stamps every agent pane on the server. So
  one scoped driver in one session keeps every other session fresh too.

The same holds for the other scopes: `#(tma status --repo app)` counts one repo's
agents, and `tma watch --repo app` opens a dashboard for it.

## Next

- [Drive an external bar](drive-an-external-bar.md) if your status bar is not
  tmux's.
- [Install the keybindings](install-the-keybindings.md) to make the segments
  clickable and put the picker on a key.
- [Run the daemon](run-the-daemon.md) for the two setups where a status-line
  driver alone is not enough.
