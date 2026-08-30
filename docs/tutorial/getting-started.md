# Getting started

This tutorial takes you from an empty tmux to a working agent monitor. You will
build `tma`, wire one agent (Claude Code) so its state is reported the instant it
changes, and learn the loop the tool is built around: see who is blocked, jump to
them, and clear the flag. Every command below is real; run them as you read.

You need `tmux` (3.2 or newer), a Rust toolchain, and Claude Code installed. The
commands assume a POSIX shell.

## 1. Build the binary

Clone the repository and install from the clone:

```
$ git clone https://github.com/pperanich/tmux-agents
$ cd tmux-agents
$ cargo install --path crates/tma
```

This puts a `tma` binary on your `PATH` (in `~/.cargo/bin`). Check it:

```
$ tma --version
tma 0.5.5
```

If you would rather install from the Nix flake or the Home Manager module, take
the detour through [install tma](../how-to/install-tma.md) and come back with a
`tma` on your `PATH`.

Running `tma` with no subcommand opens the picker, and `tma --help` lists every
command. You will meet the important ones below.

## 2. Wire Claude Code

Wiring an agent installs state-reporting hooks into its config, so it reports
state as it works. For Claude Code that is one command:

```
$ tma install-hooks claude
tma: proposed change to ~/.claude/settings.json (agent hooks):
  - {}
  + {
  +   "hooks": {
  +     "SessionStart": [
  +       {
  +         "hooks": [
  +           {
  +             "type": "command",
  +             "command": "~/.cargo/bin/tma-hook claude SessionStart"
  +           }
  +         ]
  +       }
  +     ],
  +     "SessionEnd":       [ ... "tma-hook claude SessionEnd" ... ],
  +     "UserPromptSubmit": [ ... "tma-hook claude UserPromptSubmit" ... ],
  +     "PreToolUse":       [ ... "tma-hook claude PreToolUse" ... ],
  +     "PostToolUse":      [ ... "tma-hook claude PostToolUse" ... ],
  +     "Notification":     [ ... "tma-hook claude Notification" ... ],
  +     "Stop":             [ ... "tma-hook claude Stop" ... ],
  +     "SubagentStart":    [ ... "tma-hook claude SubagentStart" ... ],
  +     "SubagentStop":     [ ... "tma-hook claude SubagentStop" ... ]
  +   }
  + }
Apply this change? [y/N] y
tma: installed hooks for claude
```

`tma` prints the exact diff and applies it (pass `--yes` to skip the confirmation
in scripts). The command also installs two tmux server hooks (they show up in
step 8's `tma doctor` output).

Verify the wiring:

```
$ tma install-hooks claude --check
tma: hooks OK
```

Now start Claude Code in a tmux pane and give it a task. Its `SessionStart` hook
registers the pane, and every prompt, tool call, and permission prompt updates
the pane's state as it happens.

## 3. See state in `tma ls`

Open two or three tmux windows and run Claude in a couple of them. Give one of
them a long task so it stays busy. Then provoke a blocked agent on purpose
instead of waiting for one: in the other pane, ask Claude to run a shell command,
say `run "ls -la" for me`. Claude Code stops and asks for approval before running
a command it has no standing permission for, and that halt is the `blocked`
state. Leave the prompt sitting there unanswered, switch to a third window, and
list the agent panes:

```
$ tma ls
%1	claude	blocked	permission	1786900866503	s2:0.0	web-ui	1		web-ui	main	
%0	claude	working		1786900866412	s1:0.0	api-server			api	fix/timeout	
```

One tab-separated line per agent pane:
`pane  agent  state  detail  since  session:window.pane  title  attention  muted
repo  branch  worktree`. Here `%1` is blocked on a permission prompt and `%0` is
working. The `1` after the blocked row's title is the attention flag (step 8);
the empty column after it is [`tma mute`](../reference/cli.md#tma-mute), which
neither pane is under. The last three columns are the pane's git checkout, all
empty for a pane that is not in one. For a machine-readable feed, add `--json`:

```
$ tma ls --json
{"schema":1,"agents":[{"pane":"%1","agent":"claude","state":"blocked","detail":"permission","since":1786900866503,"since_ms":1786900866503,"episode_ms":1786900866503,"locator":"s2:0.0","title":"web-ui","attention":true,"done":false,"session":"b47e5d18-2a90-4c3f-8de6-71f0c9a2b845","context":18,
…
```

That is one row, cut short: each carries twenty-one keys, the ones above plus
`context_at_ms`, `muted`, `tokens`, `repo`, `branch`, `worktree`, `server`, and
`host`. The full column and JSON contract, with the type and meaning of every key,
is in [Pane options and JSON contracts](../reference/pane-options-and-json.md).

## 4. Put the counts in your status line

`tma status` prints a one-line summary with glyphs and tmux color codes:

```
$ tma status
#[range=user|tma:blocked]#[fg=red]⚑1#[norange] #[range=user|tma:working]#[fg=yellow]●1#[norange]
```

That is one blocked and one working agent. tmux renders the `#[fg=...]` codes as
color when this runs from `status-right`, and draws nothing for the `#[range=…]`
markers, which only matter if you opt into [clickable
segments](../how-to/install-the-keybindings.md#clickable-status-segments). Add it to
your tmux config (`~/.tmux.conf` or `~/.config/tmux/tmux.conf`):

```tmux
set -g status-right '#(tma status) %H:%M'
```

Reload tmux so it picks the line up, naming whichever file you just edited:

```
$ tmux source-file ~/.config/tmux/tmux.conf
```

The right-hand end of your status line should now read `⚑1 ●1`: a red flag for
the agent still parked on its approval prompt, and a yellow dot for the one
working. tmux redraws it on its own `status-interval` (10 s by default), so give
it a moment.

Keeping it in `status-right` does more than display counts; the tier check below
and [run-the-daemon](../how-to/run-the-daemon.md) cover why.

## 5. Open the picker

Run `tma` with no arguments (or bind it to a key, step 7):

```
tma
```

The picker is a fuzzy list of every agent pane across all your sessions, blocked
ones sorted first. Each row leads with the state glyph, then the agent name,
`session:window.pane`, and time in state, then a dimmed branch label when the pane
resolves one, and finally the pane title. A live preview of the highlighted pane
sits beside the list. Type to filter, use the arrow keys to move, `Enter` to jump
to the highlighted agent, `tab` for that agent's action menu, `Esc` to cancel.
Every printable key types, so any agent name is searchable from the first
keystroke.

## 6. Open the watch dashboard

The picker is modal: it closes when you jump. For an always-on view, `tma watch`
is a persistent dashboard. Give it a tmux window of its own:

```
$ tmux new-window 'tma watch'
```

It shows the same rows as the picker, refreshing every second and immediately
when you change panes. `Enter` jumps the acting client to the highlighted agent
and leaves the dashboard running where it is; `q`, `Esc`, or `ctrl-c` quit. Step
7 puts it on a key.

A window is one placement, not the only one. A spare terminal outside tmux works
(`tma watch` talks to the server over its socket), and so does a split beside
your work — with the caveat that a split stays in the window you opened it in
when you jump somewhere else. tma does not place it for you.

## 7. Jump to whoever needs you

The whole point is closing the gap between "an agent is blocked" and "you are
looking at it". `tma jump --blocked` moves focus to the longest-blocked agent
across every session, no picker needed. Rather than hand-writing bindings, have
`tma` install its own:

```
$ tma install-keys
tma: proposed change to ~/.config/tma/tmux.conf (tma keybindings):
  @@ -0,0 +1,9 @@
  +# tma keybindings, managed by `tma install-keys`. Do not hand-edit; re-run to update,
  +# or `tma install-keys --uninstall` to remove.
  +bind-key a display-popup -E -w 80% -h 60% 'tma'
  +bind-key G new-window 'tma watch --table'
  +bind-key j run-shell 'tma jump --attention --client "#{client_name}"'
  +bind-key g run-shell 'tma jump --blocked --client "#{client_name}"'
  +bind-key b run-shell 'tma jump --back --client "#{client_name}"'
  +bind-key h run-shell 'tma jump --home --client "#{client_name}"'
  +bind-key A run-shell 'tma act --menu --pane "#{pane_id}"'
Apply this change? [y/N] y
tma: proposed change to ~/.config/tmux/tmux.conf (tma keys source-file):
  @@ -0,0 +1 @@
  +source-file -q "$XDG_CONFIG_HOME/tma/tmux.conf" "$HOME/.config/tma/tmux.conf" # tma keys
Apply this change? [y/N] y
tma: installed keybindings (~/.config/tma/tmux.conf). Reload with `tmux source-file ~/.config/tmux/tmux.conf`.
tma: reminder: add `#(tma status)` to your `status-right` for the ambient state driver (tma does not edit status-right).
```

The bindings live in their own managed file; your tmux config gets one line that
sources it, and `tma install-keys --uninstall` takes both away again. The closing
reminder is unconditional, and you already did that part in step 4. Reload the
config it named:

```
$ tmux source-file ~/.config/tmux/tmux.conf
```

Press `prefix g` and you land on the blocked pane. `prefix j` goes to whoever
wants you next (blocked first, then finished-but-unreviewed), `prefix b` returns
to where you jumped from, `prefix a` opens the picker in a popup, and `prefix G`
opens the step-6 dashboard in a window, straight into its full-width table. See
the [keybindings reference](../reference/keybindings.md) for the rest of the set,
and [Install the keybindings](../how-to/install-the-keybindings.md) for rebinding
any of them.

## 8. Understand the attention lifecycle

Look again at the `tma ls` output in step 3: the blocked row ended in `1`, the
attention flag. It marks a pane that changed to something you have not looked at
yet, and it clears the moment you focus the pane. Jump to a flagged pane (or
switch to it manually) and watch its glyph revert: a blocked agent's `⚑` or a
finished agent's **done** `✓` drops back to a plain idle `○`. That is the loop, an
agent flags for attention, the surfaces show it, you jump, the flag clears. For
how attention is set and why it is not a fifth state, see
[the detection model](../explanation/detection-model.md#four-states-plus-detail-plus-attention).

Confirm the tier you are running at with `tma doctor`:

```
$ tma doctor
daemon:  not running (<tmpdir>/tma/<server>.sock) — tier 3 needs a running daemon (`tma daemon --ensure`)
ambient: polling — `tma status` last ran 3.5s ago
clients: 1 attached
watch:   1 watcher running (nudged on focus change)
hooks:   after-select-pane ✓  session-window-changed ✓
wrapper: ~/.cargo/bin/tma-hook ✓
agents:  6 loaded, no issues
actions: 4 loaded, no issues

panes (2):
  %0   claude     s1:0.0       tier 2   working (hook, 3.1s ago)
       hooks: wired
       not tier 3: daemon not running (events direct-stamp; run `tma daemon --ensure` for the daemon tier)
  %1   claude     s2:0.0       tier 2   blocked (hook, 3.0s ago)
       hooks: wired
       not tier 3: daemon not running (events direct-stamp; run `tma daemon --ensure` for the daemon tier)
```

The `ambient: polling` line is the status-right edit from step 4 doing its second
job. Had you skipped it, that line would read `NOT polling` and the ambient
surfaces would go stale between hook events.

You now have hook-fresh state (tier 2) with no background process. That is the
whole tool for a single-user setup.

## Where to go next

- To cover more agents, or agents `tma` does not ship a mapping for, see
  [install-agent-hooks](../how-to/install-agent-hooks.md) and
  [add-a-custom-agent](../how-to/add-a-custom-agent.md).
- To get desktop notifications and blocked-agent alerts even when you are looking
  elsewhere, see [notifications](../how-to/notifications.md) and
  [run-the-daemon](../how-to/run-the-daemon.md).
- To understand *how* `tma` decides a pane is blocked, and why it trusts hooks
  over the screen, read [the detection model](../explanation/detection-model.md).
- For the crate layout, the tier story, and where state actually lives, read
  [the architecture](../explanation/architecture.md).
