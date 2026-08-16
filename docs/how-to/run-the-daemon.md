# Run the daemon

Add the optional daemon tier for lower latency, fallback detection of hookless
agents, and deduplicated notifications. The daemon is strictly additive: every
surface works without it, so run it only when you want what it adds.

## The three tiers

The daemon is tier 3, the top of the three detection tiers (polling floor, hook
tier, daemon); for what each adds and why none is required, see
[the detection model](../explanation/detection-model.md#three-tiers-none-required).
Which tier a given pane is actually at, and why it is not higher, is what
`tma doctor` reports (below).

## Two setups where it stops being optional

"Strictly additive" assumes something else is driving the poll. Two common tmux
setups leave nothing driving it, and on those the daemon is the only thing keeping
state fresh.

**Detached sessions.** `#()` status jobs run only while a client is drawing the
status line. A session started with `new-session -d` and never attached — a
long-running agent you check on now and then, a fleet started by a script — has no
client, so the `#(tma status)` driver never fires and stamped state ages until you
run a `tma` command by hand. `tma doctor` names it:

```
clients: none attached — `#()` status jobs only run while a client draws the status line, so nothing polls this server (run the daemon or attach a client)
```

With a daemon running, the same line reads differently, because the gap is
covered:

```
clients: none attached — `#()` status jobs do not run detached; the daemon is keeping state fresh meanwhile
```

The other fix is an external poll: `tma status --format plain` from a bar or a
cron job reaches a detached server perfectly well (see [Drive an external
bar](drive-an-external-bar.md)). Either works; doing neither
is what leaves the server unpolled.

**`status off`.** Turning the status line off kills both tmux-side channels at
once: `#(tma status)` never runs (no status line to expand), and `display-message`
notifications have nowhere to render. Doctor flags it as a warning:

```
status:  the global `status` option is off — the `#(tma status)` driver never runs and `display-message` notifications are invisible (`tmux set -g status on`)
```

Here the daemon covers the freshness half by itself, and for the notification half
point `notify` at a channel that does not need a status line — `bell`, `osc`, or a
`command` hook (see [Set up notifications](notifications.md)).

Both of these do count against `tma doctor --exit-code`: a server with no attached
client and no daemon covering it is a warning, and so is `status off`. What
`--exit-code` deliberately ignores is a missing daemon on its own, which is a
runtime choice rather than a misconfiguration, so a wired agent sitting at tier 2
gates green. The two above are the cases where something that should be driving
the poll is not.

## Start it

`tma daemon --ensure` spawns a detached daemon for the current tmux server if none
is running, then exits. It is idempotent, so it is safe to run from a shell rc or
a tmux hook:

```
$ tma daemon --ensure
$ tma daemon --ensure     # already running: still exit 0, no second daemon
```

To run it in the foreground instead (for debugging), use `tma daemon` with no
flag. There is one daemon per tmux server, keyed by socket.

## Autostart

To have the daemon start automatically the first time you use any surface
(`ls`/`status`/`jump`/picker/`watch`/`wait`/`subscribe`), set it in config:

```toml
[daemon]
autostart = true
```

The cadence knobs (`sweep_secs`, `quiet_ms`, `demote_edges`, and others) also live
under `[daemon]`; see
[Configuration](../reference/configuration.md#daemon-tier-3-daemon-cadences).
They apply only while the daemon runs.

## Reload config without a restart

A running daemon re-reads its config and manifests on `tma reload` (or a SIGHUP),
swapping every derived setting in place while keeping its live state:

```
$ tma reload
tma: reloaded the daemon's config + manifests
```

If no daemon is running for this server it is a clean no-op (one-shot surfaces and
the picker reload on their own each cycle):

```
$ tma reload
tma: no daemon running for this server (nothing to reload; one-shots and the picker reload on their own)
```

An invalid config or manifest on reload is kept-old and logged: a reload never
kills or corrupts a running daemon. A user manifest that fails to parse is skipped
and logged individually; the daemon keeps serving on the rest of the set.

`tma reload` re-reads config and manifests, not the binary. After upgrading `tma`,
the daemon already running is still the old build, so stop it and start it again:

```
$ tma doctor
daemon:  running (<tmpdir>/tma/<server>.sock)
         version 0.1.0 differs from this CLI (0.2.0) — `tma reload` only re-reads config and manifests; stop the daemon and run `tma daemon --ensure` to pick up the new build
```

The daemon records its version next to its pid in the lock file, which is where
doctor reads it from. Until you restart it, events it cannot map with its older
manifests are refused rather than acknowledged, so the firing hook stamps them
itself — a skewed daemon costs latency, never a dropped transition.

## See the effective tier

`tma doctor` reports the tier per pane and the daemon's status. With the daemon
running, wired panes reach tier 3:

```
$ tma doctor
daemon:  running (<tmpdir>/tma/<server>.sock)
ambient: NOT polling — nothing invokes `tma status`; add `#(tma status)` to status-right (required ambient driver)
clients: 1 attached
watch:   no sidebar running (`tma watch` advertises for SIGUSR1 nudges)
hooks:   after-select-pane ✓  after-select-window ✓
wrapper: ~/.cargo/bin/tma-hook ✓
agents:  6 loaded, no issues
actions: 4 loaded, no issues

panes (2):
  %0   claude     s1:0.0       tier 3   working (hook, 40.6s ago)
       hooks: wired
  %1   claude     s2:0.0       tier 3   blocked (hook, 40.6s ago)
       hooks: wired
```

`agents:` is the manifest roster tma loaded; `panes:` is what it found running.

Without the daemon the same panes show tier 2 with the reason "daemon not
running". `tma doctor --json` emits the same diagnosis as a versioned schema for
scripting.

## Nested tmux sessions

There is one daemon per tmux server, so a nested tmux gets its own. Agents running
inside an inner server are invisible to a tma on the outer one: their processes are
not in the outer pane's tree, and their state options live on the inner server.
Doctor says so rather than leaving the pane unexplained:

```
nested:  1 pane(s) running a multiplexer client — agent state lives on the inner server; run tma there
  - %3 s1:0.1 (tmux)
```

Run `tma` from inside the nested session and it targets the inner server without
any flag: tmux sets `$TMUX` in every pane it owns, and the client reads its socket
from there. So `tma daemon --ensure`, `tma ls`, and the keybindings all work in the
inner session exactly as they do in the outer one, each with its own daemon.
