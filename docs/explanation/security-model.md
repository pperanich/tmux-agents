# The security model

tma has one security boundary, and it is your user account. Everything below
follows from that: what the event channel does and does not check, why the act
broker verifies state twice, and why an action's context arrives as environment
rather than as text spliced into a command.

## `tma event` is a cooperative channel, not an authenticated one

Worth knowing before you build on it: `tma event` authenticates nothing. It takes
the pane from `$TMUX_PANE`, maps the event through the named agent's manifest, and
stamps — so **any process running as you, on a tmux server you can reach, can
stamp any pane's state**. There is no caller check, no token, and none is planned;
the only filter in the path is the subagent guard, which compares a payload's
`session_id` against the pane's stored `@agent_session` and exists to stop an
agent's own subagents from clobbering the parent's row, not to stop you.

That is a deliberate consequence of the design rather than a gap in it. tma's
state lives in tmux pane options, which any same-user process can already write
with `tmux set-option -p`; an authenticated event path would guard the front door
of a house with no walls. What it buys is that anything able to run a command can
report state — a shell script, a CI step, an agent in a container — with no
daemon, no port, and no registration.

The daemon's socket is gated the same way and no further: its directory is
created `0700` and the socket `chmod`ed `0600` (both best-effort), so the local
user reaches it and nobody else does. It checks no peer credentials. It does
re-derive state from the raw `(kind, payload)` through the same mapping the
direct path uses rather than trusting a pre-computed state off the wire, which is
integrity of the mapping, not authorization of the sender.

Anyone who can run processes as you can also drive tma; nobody else can reach it
at all.

## Why the act broker verifies twice

The failure that matters when firing an action is a stale one. A surface painted
`blocked` at some instant, you pressed two seconds later, and the agent left
`blocked` one second in. A blind `y` Enter now answers a different prompt, and
possibly a destructive one.

So the gate is checked twice: once when the menu is built or the fire is
requested, and again under the held pane lock immediately before the keys go out.
For a `keys` action the first check does not trust a stale stamp either: if the
pane's `@agent_stamped_at` is older than the freshness bound (three seconds by
default, the status-line cadence plus slack), the broker runs one on-demand
detection cycle on that pane and gates on the result.

**A residual window remains, and it is accepted rather than closed.** tmux has no
transactional send, so nothing local can make "read the state" and "send the
keys" one operation. What the second check buys is shrinking that window from the
seconds a surface repaint cycle allows down to the gap between one option read
and one `send-keys`. The stale-paint case, which is the common one, is eliminated
entirely; what is left is the same residue every interactive user lives with when
they type into a pane.

`--force` skips the `when` gate only. It never skips `requires`, and never skips
the single-flight lock: `requires` is a correctness precondition rather than a
staleness guard, and a forced action with an empty `TMA_SESSION_ID` is exactly
the half-run it exists to prevent.

## Why action context arrives as environment

An exec action's `command` string is handed to `sh -c` verbatim. tma substitutes
nothing into it. Everything the action needs to know about its target arrives as
environment variables instead: `TMA_PANE`, `TMA_AGENT`, `TMA_STATE`,
`TMA_DETAIL`, `TMA_SESSION_ID`, `TMA_CWD`, `TMA_PID`, `TMA_LOCATOR`, `TMA_TITLE`,
`TMA_ACTION`, and the caller's `--arg` values as `TMA_ARG`, `TMA_ARG_1..N`, and
`TMA_ARG_COUNT`.

Interpolating any of that into the command string (`command = "summarize.sh
{pane}"`) would be a quoting injection waiting to happen. A pane title is
attacker-influenced text: an agent prints whatever its tool output tells it to,
and tool output can come from a repository, a web page, or a model. Environment
variables cross the exec boundary without interpolation and without shell
re-parsing, so a hostile title is inert data on the way in.

**Inert on the way in is not inert on the way through.** The env transport
protects exactly one boundary, tma's. A script of yours that expands one of those
variables unquoted re-parses it in your shell, which hands the hostile value back
its teeth:

```sh
echo "$TMA_TITLE"      # data
echo $TMA_TITLE        # re-parsed by your shell
```

So quote every `TMA_*` expansion, `--arg` values included, and pass values to
other programs as arguments rather than building a command string out of them.
The same reasoning is why the action command runs in tma's own working directory
rather than the pane's: a script that wants the agent's directory says so with
`cd "$TMA_CWD"`.

## See also

- [Author a custom action](../how-to/custom-actions.md) for the authoring side of
  the two rules above.
- [`tma act`](../reference/cli.md#tma-act) for the gate, the lock, and the exit
  codes.
- [Architecture](architecture.md) for the crate boundaries that keep the write
  path in one place.
