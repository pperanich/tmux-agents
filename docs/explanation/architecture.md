# Architecture

This page explains why `tma` is shaped the way it is: seven small crates in one
workspace, three dependency rules the compiler enforces, and a single binary
that ships them all. The precise contracts live in the
[reference](../reference/cli.md) section; the full decision record is kept in the
repository's `docs/internal/` notes rather than on this site.

## One binary, seven crates

`tma` is one executable. Splitting it into crates buys nothing at the command
line, so why bother? Because three boundaries inside the code carry real
invariants, and before the split only convention kept them honest. Making each
boundary a crate edge hands the policing to `cargo`: a violation stops
compiling. The rest of this page is those three rules and the crates that
express them.

```
tma-core ← tma-tmux ← tma-runtime ← tma-daemon
                              ↑          ↑
              tma-ui-core ← tma-ui  tma ─┘
                          ↑    ↑     │
                          └────┴─────┘
```

Arrows point from a crate to what it depends on. The graph is acyclic, which
`cargo` guarantees, so the layers can only ever stack one way.

| crate | owns |
|---|---|
| `tma-core` | The pure detection library: snapshot and evidence types, the manifest schema, identity resolution, and the verdict fold. No tmux, no I/O, no clock. The bundled agent manifests live here as compiled-in TOML, with their fixture tests beside them. |
| `tma-tmux` | The only crate that spawns `tmux`. The read path (`list-panes` / `capture-pane`, the `ps` process walk), the control-mode client pool, and the guarded write adapter that stamps pane options. |
| `tma-runtime` | Tier 2: config, manifest loading, the poll cycle, on-demand capture, the `tma event` hook bridge, the wire protocol, the single-fire notification primitive, and the pass-through `ui` module that is the only tmux surface display code may call (pane capture, focus and the active-client reads, the jump trail, attention clearing, `display-menu`, the watcher's pid advertisement). |
| `tma-daemon` | Tier 3, and only tier 3: the serve loop and notification dispatch. Nothing below it depends on it. |
| `tma-ui-core` | The pure interaction core for the two live surfaces: each is an Elm-style fold from events (keys, ticks, refreshed rows) to requested effects, so selection, refresh gating, and preview caching are unit-tested without a terminal. |
| `tma-ui` | The display layer: the shell loop driving both folds (input mapping, drawing, executing their effects), plus cross-session jump and the `ls` / `status` surfaces. It reads snapshots and never touches tmux directly. |
| `tma` | The binary: `clap` dispatch, hook installation, and the `--json` value formatting the surfaces do not. |

An eighth crate, `tma-test-support`, holds the shared integration-test harness
(a scratch tmux socket, the daemon lock gate). It is a dev-dependency and never
ships.

## Rule 1: the core is pure

`tma-core` takes a snapshot and a set of evidence records in, and returns a
verdict out. It reads no clock, opens no socket, and spawns no process. Every
timestamp it reasons about is injected by a caller, never read from the wall
clock inside the fold.

The payoff is testability. The whole detection decision, the part most likely
to be subtly wrong, is a pure function over data, so its fixture tests need no
tmux server and no agent running. Every bundled screen rule ships with a
redacted capture that proves it fires. When detection is a bug, the failing test
is a `.txt` fixture and a function call, not a flaky end-to-end run.

## Rule 2: one tmux choke point

Every byte that goes to or comes from `tmux` passes through `tma-tmux`. It is
the one crate that shells out to the `tmux` command, holds the control-mode
clients, and performs the guarded option writes. Nothing above it constructs a
tmux command line.

This matters most for the write path. Concurrent producers (a status-line
poll, a hook firing, the daemon) all stamp the same pane options, and tmux has
no transactions. The safety comes from server-side conditional writes, and
concentrating them in one adapter means there is exactly one place that shape
can be right or wrong. It also makes everything above the choke point mockable
without a live server: `tma-runtime` drives detection against a `Tmux` handle,
and a test can hand it a scratch one.

The choke point also bounds failure. Every one-shot tmux command runs under a
short timeout (about three seconds), so an unresponsive server degrades to a
stale status segment and a skipped cycle rather than a hung process, and the
focus-change hooks can never wedge the invoking client.

## Rule 3: the tier boundary is strictly additive

`tma` runs at one of three tiers (the [detection model](detection-model.md)
covers what each adds), and the top tier, the background daemon, is strictly
additive: it may lower latency but is never required. Before the split the code
contradicted that promise, because the hook-installer imported the event code
which imported the daemon code. The tier story said "tier 3 is optional" while
the dependency graph said "everything needs it".

Now `tma-daemon` is a leaf that only the binary's `tma daemon` subcommand
reaches. Every other code path in the binary depends on `tma-runtime` and
`tma-tmux` only. The wire protocol and the single-fire notification primitive
live in `tma-runtime`, not the daemon, precisely so a daemonless `tma event`
can reach them. The daemon imports those for its server side. The result is that
"tier 3 is never required" is now a fact `cargo` enforces: a stray tier-3 import
into a non-daemon module would not compile, and a source-guard test catches the
one legitimate edge drifting.

The same promise shapes the delivery acknowledgement. A hook hands its event to
the daemon and only skips its own stamp when the daemon acknowledges it, so the
acknowledgement has to mean "I produced a write plan", not "I recognized the
agent name". A daemon is a long-lived process carrying the manifests it was
compiled with, so after an upgrade the resident daemon can be older than the CLI
firing at it. When its manifests map an event to nothing it refuses delivery and
the hook stamps the pane itself. A refusal that *is* a decision, the subagent
ownership guard declining a foreign session's claim, is acknowledged instead:
re-applying that on the client would write exactly the state the daemon just
protected the pane from. The distinction is carried in the plan the mapping
produces, not inferred from whether anything was written.

## Why the UI reads snapshots only

Display code (`tma-ui`) reads a cycle report plus config, and never calls
`tma-tmux`. It has no dependency edge to it at all. Every tmux touchpoint the
UI genuinely needs, capturing a preview, moving focus, clearing attention, the
jump trail, the watch-pid advertisement, goes through a named helper in
`tma-runtime::ui`. Two layers enforce this at two strengths. The pure fold crate
(`tma-ui-core`) has no runtime edge at all, so `Tmux` is not even nameable there
and the compiler forbids it tmux entirely. The shell crate (`tma-ui`) does carry
a runtime-only edge, so `Tmux` reaches it through runtime's re-export and the
compiler alone cannot stop a stray `tmux.set_option(...)`; the `tma-runtime::ui`
helper surface plus a source-guard test (`crates/tma-ui/tests/ui_boundary.rs`,
which fails on any direct `tmux.<method>(` call) hold that boundary instead. The
picker cannot accidentally grow its own detection logic or its own write path;
it can only render what the runtime already decided. This keeps the surfaces
dumb, which is the same property that lets a user's raw `tmux show-options` read
the exact same state the picker shows.

## Agents in popups are invisible, by construction

One consequence of reading everything from tmux's own enumeration is worth
stating outright. Run an agent inside a `display-popup` and tma will never see
it. A popup's process lives in a hidden internal pane: `$TMUX_PANE` is empty
inside it, and `list-panes -a`, the one enumeration every tma surface starts
from, does not return it. So there is no pane id to stamp, no row to list, and
nothing to jump to. This is not a limitation tma can lift; the pane is not in the
model tmux exposes.

Reading from a popup is fine, which is why the picker binding is one: `tma` in a
popup lists panes, previews them, and jumps by switching the *client*, which it
asks tmux for directly (never from `$TMUX_PANE`, for exactly this reason). It is
the agent that must live in a real pane.

That is also why `tma watch` is bound to a split rather than a popup: popups are
modal and vanish on the next overlay, and a persistent dashboard needs a pane.

## Distribution stays one binary

The crates are an internal seam, not a distribution story. The workspace builds
a single `tma` executable, installed once and invoked as one command. Prefixed
crate names (`tma-core`, `tma-tmux`, and so on) keep the door open to publishing
them on crates.io later without a rename, but that door is merely unlocked, not
walked through. A separate daemon binary was considered and rejected: `tma
daemon` is a subcommand of the same executable, so there is only ever one thing
to install and keep on `PATH`.

The full numbered decision records, each with the options weighed and the
condition that would reopen it, live in the repository:
[`docs/internal/ARCHITECTURE.md`](https://github.com/pperanich/tmux-agents/blob/main/docs/internal/ARCHITECTURE.md)
and
[`docs/internal/DAEMON.md`](https://github.com/pperanich/tmux-agents/blob/main/docs/internal/DAEMON.md).
