# Why tma

Coding agents run as long-lived interactive TUI processes. Anyone running more
than one hits the same failure: an agent goes blocked on a permission prompt in
some background window and sits there for twenty minutes while you work in
another session, unaware. The state you need is on a screen nobody is looking at.

There are three ways to answer that: replace the multiplexer, wait for each agent
vendor to ship its own remote, or read the multiplexer you already run. tma takes
the third. This page is the comparison and the one architectural choice that
follows from it.

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

## Wait for the vendor

The other answer is the agent vendor's own remote: a first-party app or web surface
that reaches the session you started on your machine. Claude Code has one. It is a
better fit than tma for one thing, which is that vendor's own agent, and it is
worth using for that.

Two properties keep it from being the whole answer. It covers one vendor's agent,
and a working week is usually more than one: Claude Code in this repo, Codex in
that one, OpenCode on the box in the closet. And what a first-party remote lets you
do remotely is not everything the terminal lets you do. Claude Code's Remote
Control cannot answer a permission dialog from the phone; the dialog is a terminal
surface, so the answer has to be typed where the terminal is. The prompt sitting
unanswered is exactly the state that stopped the work.

That is the case tma covers: answer the prompt, from any agent, from anywhere.

## Where tma sits

tma keeps tmux and adds three things to it. Agent-agnostic discovery, a process
walk plus per-agent manifests, means a hookless agent is detected anyway from its
process and its screen. Hook integration means a cooperative agent reports state
the instant it changes instead of a poll later. Cross-session navigation means
the picker lists and jumps to agents anywhere on the server, not only in the
session you are attached to.

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

## What this shape gives you that the others do not

Four things follow from being cross-agent and self-hosted, and none of them is
available from a monitor built around one vendor's agent or one vendor's server.

**One fleet, six agents.** `tma ls`, the picker, the status line and `tma act` take
the same arguments whichever agent is in the pane. Adding a seventh is a TOML
manifest, not a patch.

**Transcripts without cooperation.** `tma transcript` reads what four of the six
agents (claude, codex, gemini, pi) have been writing, out of the files they already
keep on disk, normalized so the panes answer in the same vocabulary. None of those
agents was asked for an API, a plugin, or a protocol. The two that are refused are
refused by name, because "nothing happened" is a claim and it would be false.

**Consent that is never context-free.** An approval is only meaningful if the thing
being approved is in front of you. `approve` and `deny` are gated on `detail =
permission`, so a dialog that wants you to choose an option rather than grant a
request offers no approve key; the gate is re-asserted inside the pane's action lock
against the same read it was quoted from, so a pane that moved on refuses instead of
landing your answer on the prompt that replaced it; and a notification navigates to
the pane rather than acting on it.

**No third party on the path.** State is written to your tmux server and read back
from it. Nothing is relayed through a service, there is no account to have, and the
transcript reader opens files that were already on your disk.

## What tma is not

- Not a multiplexer, a terminal emulator, or a session manager. It never owns a
  PTY.
- Not a project navigator. It navigates agents; a sessionizer navigates repos,
  and they coexist under different keybindings.
- Not an orchestrator. It observes agents and answers prompts you aim at them; it
  does not spawn them or drive them at each other.
- Not a replacement for a vendor's own remote control of that vendor's own agent.
  Where one exists it is the better tool for that agent, and the two coexist.
- Not a way to watch agents outside tmux. A pane is the unit, and an agent in a
  `display-popup` is invisible for the same reason: tmux does not enumerate it.

## See also

- [Architecture](architecture.md) for the crate boundaries that hold these rules
  up.
- [The detection model](detection-model.md) for how a verdict is actually
  reached.
- [Getting started](../tutorial/getting-started.md) to see the whole loop in a
  terminal.
