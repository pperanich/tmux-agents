# Why tma

Coding agents run as long-lived interactive TUI processes. Anyone running more
than one hits the same failure: an agent goes blocked on a permission prompt in
some background window and sits there for twenty minutes while you work in
another session, unaware. The state you need is on a screen nobody is looking at.

Three designs answer that, and tma is the third. This page is the comparison and
the one architectural choice that follows from it.

## Replace the multiplexer

herdr makes the monitor a terminal multiplexer with agent detection built in. It
owns the PTYs, so it sees
everything: byte-level activity, the live screen, process lifetime, no polling
anywhere.

It works, and the cost is that it replaces tmux. For anyone with an established
tmux workflow (a sessionizer, worktrees as windows, custom keybindings, a
status-line integration) adopting it means nesting one multiplexer inside
another: two prefix keys, two detach models, two session stores, and panes the
outer tmux cannot see.

The part worth keeping is not the multiplexer. It is the detection model:
identify the agent by process name, read its state from the terminal screen
through declarative per-agent rules, and arbitrate between evidence sources.
Everything built to support that (PTY ownership, session persistence, pane
management) tmux already provides.

## Two projects that each solved half

Two MIT-licensed projects kept tmux and each demonstrated one half of what tma
needs.

**`tmux-agent` (`ta`, by Trent Davies)** is a one-shot stateless CLI with an
embedded fuzzy picker, invoked from a keybinding. It shows
that the layered evidence model works with no resident process at all: a
hook-stamped tmux option beats a screen scrape, which beats a pane title. Its
hook installer writes Claude Code hooks that shell out to stamp a tmux option
directly, which is the daemonless path running in the wild. Its detection rules
live in Rust source, so tuning one is a recompile, and its hooks are wired for a
single agent with no notion of what a given agent's hooks do and do not cover.

**`tmux-agent-sidebar` (by hiroppy)** is a tmux plugin plus a persistent sidebar.
It shows hook-driven ingress with tmux pane
options as the public bus, and several patterns tma adopts outright: a stable
wrapper script the agent config points at, so the agent's config never names the
binary and a missing binary is quiet rather than broken; one tested resolver for
state priority; a guard against an agent's subagents clobbering the parent's row,
since they share `$TMUX_PANE` and fire hooks carrying a foreign session id; and
signal nudges from tmux focus hooks for instant refresh without a daemon. Its
limit is discovery: a pane is an agent only if a hook stamped it, so an agent
with no hooks to install cannot exist for it.

## Where tma sits

tma occupies the union neither of those covers. Agent-agnostic discovery, a
process walk plus per-agent manifests, means a hookless agent is detected anyway
from its process and its screen. Hook integration means a cooperative agent
reports state the instant it changes instead of a poll later. Cross-session
navigation means the picker lists and jumps to agents anywhere on the server, not
only in the session you are attached to.

The three tiers stack rather than compete: one-shot commands work alone, hooks
cut latency to zero for the agents that have them, and the daemon is strictly
additive on top. Consumers cannot tell which tier produced a verdict, because all
three write the same place. Which brings up the choice the whole design rests on.

## tmux is the state store

Every verdict tma reaches is written back onto tmux as pane and window user
options:

```
set -p -t %13 @agent_name  claude
set -p -t %13 @agent_state blocked
set -w -t mysession:2 @agent_summary "blocked:1"
```

Once state lives there, integration is ordinary tmux configuration rather than a
private protocol. `window-status-format` colors a window red when its
`@agent_summary` says an agent is blocked. `status-right` renders a fleet
summary. tmux hooks and `if -F` conditionals react to a state change. Any other
tool reads the same options with `show-options`, and needs no agreement with tma
about anything.

This is what a monitor with its own client socket cannot offer, and the
difference is not throughput. It is that there is no protocol to version: tmux
formats are the API, and they were stable before tma existed. A reader written
against `#{@agent_state}` keeps working across every tma release, because tma is
not in the read path at all. The [pane option
schema](../reference/pane-options-and-json.md) writes that promise down, and
`tma ls --json` is the same contract for consumers that want a resolved row
rather than a raw option.

It also means the store outlives the writer. Kill every tma process and the last
verdict is still on the panes, still readable, still rendering in your status
line. Nothing has to be running for state to exist.

## What tmux already provides

Each capability a PTY-owning monitor has to build has a tmux equivalent tma reads
instead:

| what a PTY-owning monitor builds | what tma reads |
|---|---|
| pane process probe | `#{pane_pid}` and a process-tree walk |
| bottom-of-buffer screen snapshot | `capture-pane -p -e -t %id -S -<N>` |
| OSC title, where agents put spinners and state | `#{pane_title}` |
| PTY activity signal | `#{window_activity}`, and control-mode `%output` edges |
| state storage and event bus | pane and window user options |
| session persistence, detach and attach | tmux itself |

The detection core reduces to pure functions over a snapshot, which is why the
part most likely to be subtly wrong is testable without a tmux server or a
running agent. Every bundled screen rule ships with a captured fixture that
proves it fires.

## What it costs

tmux tells tma less than owning the PTY would, and the honest accounting is
short. Activity is window-granular rather than per-pane. Capture is poll-based
rather than streamed. A pane scrolled into copy mode is showing history, so tma
freezes its state rather than matching against it.

The residual risk is a working agent with a quiet screen and no title spinner
reading as idle for a cycle or two. That is accepted, because the state worth
being right about is `blocked`, and blocked is the one an agent makes loud: it
paints a prompt on the screen and, for most agents, fires a hook as well. [The
detection model](detection-model.md) covers the arbitration in full, including
where it deliberately holds a stale answer instead of guessing.

## What tma is not

- Not a multiplexer, a terminal emulator, or a session manager. It never owns a
  PTY.
- Not a project navigator. It navigates agents; a sessionizer navigates repos,
  and they coexist under different keybindings.
- Not an orchestrator. It observes agents and answers prompts you aim at them; it
  does not spawn them or drive them at each other.
- Not a way to watch agents outside tmux. A pane is the unit, and an agent in a
  `display-popup` is invisible for the same reason: tmux does not enumerate it.

## See also

- [Architecture](architecture.md) for the crate boundaries that hold these rules
  up.
- [The detection model](detection-model.md) for how a verdict is actually
  reached.
- [Getting started](../tutorial/getting-started.md) to see the whole loop in a
  terminal.
