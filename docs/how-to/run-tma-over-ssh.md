# Run tma over ssh

Two arrangements get called "tma over ssh" and they are not the same problem.
Work out which one you have first, because only the second needs anything from
this page.

## tmux on the far side

You ssh to a host, start tmux there, and run agents in its panes. tma belongs on
that host too, beside the agents: install it there and everything works exactly
as it does locally, because nothing is remote from tmux's point of view. Your
terminal is a client and tma never sees it.

The only thing your connection changes is where a notification lands. `osascript`
and `notify-send` fire on the host running tmux, which is not the machine you are
sitting at, and neither fails loudly. [Notifying from a remote
host](notifications.md#notifying-from-a-remote-host) covers the three sinks that
do cross a connection.

## ssh from a local pane

tmux runs on your machine, one of its panes runs `ssh`, and the agent lives on
the far side. tma classifies that pane as **remote** and takes it out of scope,
along with `mosh`, `docker`, `podman`, and `kubectl`. The reason is that neither
of the two inspecting tiers survives the hop: the process walk reads your local
`ps`, where the agent does not exist, and a capture would match screen rules
against output the agent merely painted through a terminal it does not control.
An empty walk plus a plausible-looking screen is how false positives are made.

Concretely, on such a pane:

- No identity from the process tree, and no screen rules.
- Hook events are its only possible evidence.
- Any `@agent_*` options it still carries from before the hop are **held**, not
  refreshed. Nothing is updating them.

`tma doctor` reports it, and does not call it a problem, because running an agent
elsewhere is a choice:

```
remote:  1 pane(s) behind a remote shell — an agent there reports only if it can reach this tmux socket (see docs/how-to/agents-in-containers.md)
  - %10 work:4.0 (ssh)
```

See [Remote and ignored panes](diagnose-with-doctor.md#remote-and-ignored-panes)
for the rest of that report.

## Make a remote agent report

The hook tier inspects nothing, which is why it is the one tier that can cross.
`tma event` takes a pane id, maps one event through a manifest, writes tmux pane
options, and exits; it keeps no state between runs and needs no daemon. So an
agent on the far side can stamp a pane on your tmux server, provided it can reach
the socket and knows the pane id.

That is the same shared-socket arrangement [Run an agent in a
container](agents-in-containers.md) spells out step by step. Read it for the
details; over ssh only the transport differs. Instead of a bind mount, OpenSSH
forwards a unix socket the other way:

```sh
ssh -o StreamLocalBindUnlink=yes \
    -R /tmp/tma-tmux.sock:"$(tmux display -p '#{socket_path}')" buildbox
```

Pick a remote path you can write, and keep `StreamLocalBindUnlink=yes`: without
it a socket left behind by an earlier session makes the forward fail silently
while the shell comes up fine.

The rest carries over unchanged. `tma` and a `tmux` client whose major version
matches your server go on the remote host; the agent's environment there gets
`TMA_SOCKET_PATH` pointing at the forwarded path and `TMUX_PANE` set to the local
pane id it should stamp; and `tma install-hooks <agent>` runs on the remote host,
where the agent reads its config.

**Weigh this before you wire it.** A forwarded tmux socket is your tmux server,
reachable from the remote host. Anything running there that can open it can drive
every pane on your machine, not only the one you meant. tma adds no check of its
own: the [security model](../explanation/security-model.md) is your user account,
and forwarding widens what counts as your user account. Do it for hosts you
control.

What you get is the hook tier and nothing else: state, transitions, attention,
notifications, `tma wait`, the picker, the sidebar. What you do not get is the
fallback, so an event the agent never fires stays unreported, where a local agent
would have had its state read off the screen.

## There is no cross-host view

tma has no aggregation across machines. One invocation talks to one tmux server,
chosen by `--socket-name` or `--socket-path`, and there is no command that merges
several. Agents on three boxes, each with its own tmux server, means running tma
on each box.

What exists instead is enough provenance to merge the output yourself. Every
`tma ls --json` row carries `host` and `server`, so `(host, server, pane)` keys a
combined set where a bare `%5` would collide; see [`server` and
`host`](../reference/pane-options-and-json.md#server-and-host-merging-rows-from-more-than-one-place).
Collecting those rows, and deciding what to do with them, is yours to build.

## Next

- [Run an agent in a container](agents-in-containers.md) for the shared-socket
  pattern in full.
- [Diagnose with `tma doctor`](diagnose-with-doctor.md) to see how a pane was
  classified.
- [Set up notifications](notifications.md#notifying-from-a-remote-host) for the
  sinks that reach you rather than the host.
