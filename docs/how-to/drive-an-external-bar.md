# Drive an external bar

Your agent panes live in tmux but your status bar does not: sketchybar, waybar,
polybar, starship, a Prometheus scrape, a tmux-less terminal. Poll `tma status
--format plain` on whatever interval that bar already uses. It prints the same
counts as the tmux driver with the color codes dropped, since those bars do their
own styling:

```
$ tma status --format plain
⚑1 ●2 ✓1
```

## The recipes

sketchybar, in a plugin script:

```sh
sketchybar --set agents label="$(tma status --format plain)"
```

waybar, a `custom/tma` module with `"exec"` and an `"interval"`:

```json
"custom/tma": { "exec": "tma status --format plain", "interval": 5 }
```

starship, a `custom` module in `starship.toml`:

```toml
[custom.tma]
command = "tma status --format plain"
when = true
format = "[$output]($style) "
```

For a bar that would rather have numbers than glyphs, `--format json` gives the
same counts as a one-line schema-1 document. Scope any of these with the
[selector flags](../reference/cli.md#selector-flags): `tma status --format plain
--repo app` is one repo's counts.

## Why a poll is enough on its own

**A poll from an external bar is a first-class ambient driver.** Every `tma
status` invocation, whatever its `--format`, runs the full poll cycle and stamps
every agent pane on the server before it prints; the format only decides how the
counts are rendered. A sketchybar item polling every 5 seconds keeps state as
fresh as a 5-second `status-interval` would, so you do not also need `#(tma
status)` in `status-right` for freshness. You may still want it for the inline
glyphs.

That holds for a scoped poll too: the selector narrows what is printed, never
what the cycle refreshes.

**No attached client is needed.** Unlike `#()`, which only runs while a client is
drawing the status line, an external poll reaches a detached server perfectly
well: `tma` connects to the socket, cycles, and exits. A tmux session you started
with `new-session -d` and never attached to still gets refreshed state, which is
exactly the case [`tma doctor`](diagnose-with-doctor.md) warns about when nothing
else is driving.

## Export to Prometheus

`tma status --format prom` writes the Prometheus text exposition format, which is
what a node_exporter [textfile
collector](https://github.com/prometheus/node_exporter#textfile-collector) reads.
Write it atomically (rename into place) so the collector never scrapes a
half-written file:

```cron
* * * * * tma status --format prom > /var/lib/node_exporter/tma.prom.$$ && mv /var/lib/node_exporter/tma.prom.$$ /var/lib/node_exporter/tma.prom
```

The same caveat as any cron `tma` invocation applies: cron gives you no `$TMUX`,
so pass `--socket-name`/`--socket-path` when your agents run on a named server,
and make sure the crontab user is the one who owns the tmux socket.

Two families come out of it: `tma_agents{state="…"}` (the counts, all five classes
always present) and `tma_agent_state_seconds{pane,agent,state}` (how long each
pane has held its current state). The second is the one worth alerting on: a
`blocked` agent whose age climbs past a few minutes is one nobody has answered.

```yaml
- alert: AgentBlockedTooLong
  expr: tma_agent_state_seconds{state="blocked"} > 600
  annotations:
    summary: '{{ $labels.agent }} in {{ $labels.pane }} has been blocked 10 minutes'
```

Because the cron run is itself an ambient driver, a Prometheus export doubles as
the polling floor on a server with no attached client.

## Next

- [Show agents in your status line](show-agents-in-your-status-line.md) if you
  also want the counts inside tmux.
- [Read agent state from a status bar or
  script](read-agent-state-from-a-status-bar-or-script.md) for the
  option-reading path that needs no `tma` process at all.
