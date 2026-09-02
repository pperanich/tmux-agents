# Diagnose with `tma doctor`

`tma doctor` is the one command to run when a pane is not showing what you
expect. It is read-only: it identifies panes exactly the way the poll cycle does,
reports each one's effective tier and why, and never stamps anything.

```
$ tma doctor
daemon:  not running (/tmp/tma/7f665a9304f7e8ed.sock) — tier 3 needs a running daemon (`tma daemon --ensure`)
ambient: polling — `tma status` last ran 0.1s ago
clients: none attached — `#()` status jobs only run while a client draws the status line, so nothing polls this server (run the daemon or attach a client)
watch:   no watcher running (`tma watch` advertises for SIGUSR1 nudges)
hooks:   after-select-pane ✓  session-window-changed ✓
wrapper: /home/you/.local/bin/tma-hook ✓
agents:  6 loaded, no issues
actions: 4 loaded, no issues
remote:  1 pane(s) behind a remote shell — an agent there reports only if it can reach this tmux socket (see docs/how-to/agents-in-containers.md)
  - %10 work:4.0 (ssh)
ignored: 1 pane(s) excluded from detection — unset the option to bring one back (`tmux set-option -pu -t <pane> @agent_ignore`)
  - %1 work:1.0 (ignored via @agent_ignore = manual)

panes (2):
  %7   claude     work:2.0     tier 2   unknown (process, 0.1s ago)
       hooks: wired
       not tier 3: daemon not running (events direct-stamp; run `tma daemon --ensure` for the daemon tier)
  %8   codex      work:3.0     tier 1   unknown (process, 0.1s ago)
       hooks: not installed
       not tier 2: hooks not installed for codex (run `tma install-hooks codex`)
```

## The server-wide lines

The block above the blank line is about the server, not any one pane. Four lines
are always there and the rest appear only when they have something to say.

**`daemon:`** whether a tier-3 daemon is alive for this server, and the socket it
looked at. A daemon running a different build than the CLI adds a second line:
`tma reload` only re-reads config and manifests, so picking up a new build means
stopping the daemon and running `tma daemon --ensure` again.

**`ambient:`** whether anything is calling `tma status`. `polling — last ran Ns
ago` means a driver is alive, whether that is `#(tma status)` in `status-right`,
an external bar, or a cron job. `NOT polling` means nothing is, and with no daemon
that leaves pane state as stale as your last explicit command. See [Show agents in
your status line](show-agents-in-your-status-line.md) and [Drive an external
bar](drive-an-external-bar.md).

**`clients:`** how many clients are attached. A detached server is a warning only
when no daemon is covering for it, because `#()` status jobs run only while a
client is drawing the status line.

**`watch:`** how many `tma watch` instances are running, which is what receives the
focus-change nudge.

**`hooks:` and `wrapper:`** the tmux server hooks and the `tma-hook` wrapper. A
hook can read `✓`, or `✗` as `drifted` (it runs a different command than this
build installs, usually a moved binary), `wiped` (recorded but gone server-wide,
so the server restarted), or `missing`. Each non-present hook gets its own
indented reason line.

**`agents:` and `actions:`** the manifest and action rosters, with one `-` line
per file the loader skipped and per action naming an unknown agent. A
`process_names` entry longer than 15 characters is called out here too: that is
the width both macOS libproc and the Linux kernel truncate `comm` to, so such an
entry can never match a pane unless a truncated spelling sits beside it.

Four more appear only when the condition holds: `status:` (the global `status`
option is off, which kills both `#(tma status)` and `display-message`
notifications), `mouse:` (the clickable bindings are installed but `mouse` is
off), `notify:` (your `[notify] command` failed, with the reason and a pointer to
`tma debug notify-test`), and `procs:` (the `ps` walk itself failed, so detection
cannot see what runs in a pane and only hook-registered panes are listed below).

## The per-pane block

Each agent pane gets a header line and one or more continuation lines:

```
  %7   claude     work:2.0     tier 2   unknown (process, 0.1s ago)
       hooks: wired
       not tier 3: daemon not running (events direct-stamp; run `tma daemon --ensure` for the daemon tier)
```

The header is pane id, agent, locator, tier, and the current stamp: state, the
evidence source it came from (`hook`, `capture`, or `process`), and
how long ago that evidence was taken. A pane with no decodable stamp reads
`unstamped`.

The tier is what the pane is actually getting, not what it could get:

| tier | means |
|---|---|
| 3 | Hooks are on the hook path and a daemon is running. Nothing to improve; no reason line is printed. |
| 2 | Hooks are wired but no daemon is running, so events stamp directly instead of going through the hub. |
| 1 | The pane is not on the hook path at all: screen and process detection only. |

Below tier 3 the block ends with a `not tier N: <reason>` line naming the next
tier up and what it would take. At tier 1 that reason is one of three: hooks are
not installed for this agent, the agent is hookless (screen detection only, so
there is no hook tier to reach), or tma ships no `install-hooks` adapter for it
and you would wire it by hand. When a daemon is running, the reason picks up `; a
daemon is running and provides fallback capture (tier 3)`.

Three other continuation lines show up when they apply. `demoted:` is the
interesting one: the pane registered through a hook, but its *current* state came
from capture, because output kept arriving that its hooks did not account for.
That is a suspect-wiring signal, not a proof: the usual cause is an agent
restarted without the wiring or a missing wrapper, so run
`tma install-hooks --check`. A hook claiming `working` accounts for the pane's
output until capture contradicts it, so a long tool call does not demote a
healthy pane. `model:` names an unrecognized model string. `api:` flags a pending
permission request with no reachable endpoint.

## A pane that is not listed at all

If the pane you care about has no line under `panes`, doctor has already told you
one of three things somewhere above (it is behind a remote shell, it carries
`@agent_ignore`, or the `procs:` line says the process walk failed) or the answer
is that identity did not resolve. `tma debug explain` prints the whole decision
for one pane:

```
$ tma debug explain %0
pane      %0  (work:0.0)
command   zsh
title     dev-box
flags     alternate_on=false scrolled=false history_view=false window_activity=1786903086
agent     (none — no manifest process_names matched)
process   1 procs in pane tree
```

The `agent (none — …)` line is the verdict, and it stops there: with no agent
identified there is nothing to fold. The three usual causes are all readable from
those lines. The pane's foreground is a shell and the agent is not running in it.
The agent's process name is spelled differently from every manifest's
`process_names` (compare against the `process` count and check the manifest). Or
the pane carries `@agent_ignore`, which `explain` names directly.

On a pane that *did* resolve, the same command keeps going: it prints the prior
stamp, the evidence records, every screen rule with a `[match]` or `[  -  ]`
marker beside it, and the verdict with the winning evidence source. That is the
tool for "detected, but as the wrong state" as opposed to "not detected". See
[The detection model](../explanation/detection-model.md).

## Remote and ignored panes

Both are reported, and neither is a warning.

```
remote:  1 pane(s) behind a remote shell — an agent there reports only if it can reach this tmux socket (see docs/how-to/agents-in-containers.md)
  - %10 work:4.0 (ssh)
```

A pane whose foreground is `ssh`, `mosh`, `docker`, `podman`, or `kubectl` is out
of scope by classification: neither the process walk nor a capture crosses that
boundary. Running an agent elsewhere is a choice, not a misconfiguration, so
doctor names it rather than complaining. A pane that still carries stamps from
before the boundary went up gets `; its @agent_* options are held, not refreshed`
appended, which is the honest description: nothing is updating them. To make an
agent behind one of those actually report, give its hooks a route back to this
socket, which is [Run an agent in a container](agents-in-containers.md).

```
ignored: 1 pane(s) excluded from detection — unset the option to bring one back (`tmux set-option -pu -t <pane> @agent_ignore`)
  - %1 work:1.0 (ignored via @agent_ignore = manual)
```

`@agent_ignore` is your own opt-out, so doctor shows the value you set and the
command that undoes it. Nothing else about that pane is evaluated.

A third section, `nested:`, lists panes running another multiplexer client. Agent
state lives on the inner server, so run `tma` there.

## Gate CI on the report

`--exit-code` turns the findings into a build failure, which is what a dotfiles
job wants: it catches hook drift a config change introduced, not just an absent
install.

```sh
tma install-hooks claude --yes
tma doctor --exit-code || exit 1
```

The verdict goes to stderr, so `--json` on stdout stays parseable:

```
tma: doctor: 2 warning(s), 1 pane(s) below the tier their manifest supports
```

Counted as a warning: a missing wrapper, each tmux hook that is not present, each
skipped manifest and each action naming an unknown agent, each unreachable
`process_names` entry, each undecodable stamp, a detached server with no daemon,
`status` off, mouse bindings without `mouse on`, a failed notify command, and per
pane, incomplete hook wiring, a hook demotion, and a pending permission with no
endpoint.

Deliberately not counted: a daemon that is not running, a daemon version skew, no
ambient poll, no watcher, and the `nested`/`remote`/`ignored` sections. Those
are runtime choices, so a wired agent sitting at tier 2 gates green.

The second number counts panes below the tier their manifest supports, which is 2
for an agent tma can wire and 1 for a hookless or adapter-less one. An unwired
hook-capable agent counts; a wired one at tier 2 for want of a daemon does not.

Note that a detached scratch server counts as a warning unless a daemon is
running, since nothing there would drive the polling floor. When the CI server
has no agent panes to diagnose, `tma install-hooks --check` is the narrower gate
over the wiring alone, with the same 0/1 contract.
