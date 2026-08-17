# Install agent hooks

Wire an agent so it reports state through hooks instead of screen detection
alone. Every agent below follows the same three commands: install, verify,
uninstall. What differs is where the config lives and, for some agents, a
one-time trust step you must do inside the agent itself.

`tma install-hooks` is idempotent and additive: it prints a diff and asks before
writing, preserving unrelated config. Pass `--yes` to apply without the prompt.
The hook-event wiring points at the `tma-hook` wrapper, never the binary directly,
so rebuilds never break it.

## The statusline context shim (opt-in)

One piece of wiring is not installed by default: the statusline context shim for
Claude and Cursor. It is the only edit tma makes to a value you already own —
your `statusLine` command — rather than adding tma's own keys beside it, so you
have to ask for it:

```
tma install-hooks claude --statusline    # wire it
tma install-hooks claude --no-statusline # remove it, restoring what it wrapped
```

It buys one thing: the context-window gauge (`@agent_tokens`), which the compact
action gates on. That metric appears in no hook payload — the statusline payload
is the only place an agent reports it. Skip the shim and everything else works
unchanged, because state, jumps and notifications all come from the hook events.

Because an agent's `statusLine` takes exactly one command, the shim composes
rather than replaces: it reads the payload once, forwards a copy to `tma event
--kind context` in the background, and pipes the same bytes to the command you
already had, whose output is still what gets rendered. It cannot go through the
`tma-hook` wrapper for that reason, so it embeds the resolved `tma` path with a
`$PATH` lookup behind it: `[ -x "$_TMA_BIN" ] || _TMA_BIN=tma`. Move the binary
without a `$PATH` entry and the context gauge stops while your statusline keeps
working, which is the failure this shape is chosen for.

If you would rather own the composition yourself, point `statusLine` at a script
of your own that calls `tma-hook <agent> context` alongside your real statusline,
and leave the shim uninstalled — `tma-hook` is generic over the event name, and
`tma event` falls back to `$TMUX_PANE` from the environment.

You state the choice once. `--statusline` records the agent in
`statusline-state.toml` in tma's config dir, so a later plain `install-hooks`
keeps the shim current (re-pointing it at a moved binary) instead of reporting it,
and `--no-statusline` clears the record along with the shim.

`--check` reads the same record: with no flag it passes for an agent that opted in
and for one with no shim, and reports only a shim nobody asked for — which is what
an install from before this release looks like. `--check --statusline` requires the
shim regardless; `--check --no-statusline` requires its absence.

For which states each agent's hooks cover and why, see
[Agent coverage](../reference/agent-coverage.md). For every flag and path
override, see [`tma install-hooks`](../reference/cli.md#tma-install-hooks).

## Claude Code

Config: `~/.claude/settings.json` (`hooks` block).

```
tma install-hooks claude
tma install-hooks claude --check
tma install-hooks claude --uninstall
```

`--check` reports `tma: hooks OK` when the wiring is complete. No trust step:
Claude loads the hooks on next start.

## OpenCode

Config: a JS plugin in `~/.config/opencode/plugin/tma.js`.

```
tma install-hooks opencode
tma install-hooks opencode --check
tma install-hooks opencode --uninstall
```

The plugin forwards OpenCode's event-bus events to `tma-hook`. There is no
session-end event, so deregistration rides pane close rather than a hook; nothing
you need to configure.

## Codex CLI

Config: two channels, both written at once: `notify` in
`$CODEX_HOME/config.toml` and a Claude-style `$CODEX_HOME/hooks.json` (default
`~/.codex/`).

```
tma install-hooks codex
tma install-hooks codex --check
tma install-hooks codex --uninstall
```

Caveat: the installer prints a trust step, and it is load-bearing:

```
tma: codex trust gate: the hooks.json entries stay INERT until you open codex, run /hooks, and trust the tma-hook entries (codex silently skips untrusted hooks). The notify signal works without this step.
tma: installed hooks for codex
```

Codex silently skips any untrusted hook, so after installing you must open codex,
run `/hooks`, and trust the tma entries before the `hooks.json` events fire. Trust
is recorded against the hook's exact definition, so if you later move the wrapper
you must re-trust. The `notify` channel is a plain config value and is not
trust-gated, so idle detection works immediately.

## Gemini CLI

Config: `~/.gemini/settings.json` (`hooks` object, same shape as Claude's).

```
tma install-hooks gemini
tma install-hooks gemini --check
tma install-hooks gemini --uninstall
```

Caveat: Gemini gates local config behind a per-folder trust prompt:

```
tma: gemini folder-trust gate: the settings.json hooks load only after you trust the working folder in gemini (it prompts "Trusting a folder allows Gemini CLI to load its local configurations, including … hooks …" on first run there). Once the folder is trusted the hooks fire; there is no separate per-hook trust step.
tma: installed hooks for gemini
```

Trust the working folder when Gemini prompts on first run there; after that the
hooks fire with no further step.

## Cursor CLI

Config: two files. `~/.cursor/hooks.json` carries the hooks (cursor's own shape,
not the Claude shape); `~/.cursor/cli-config.json` carries the `statusLine`
context shim. One command writes both, and `--uninstall` removes both.

```
tma install-hooks cursor
tma install-hooks cursor --check
tma install-hooks cursor --uninstall
```

Each file is parsed and rewritten on its own, so unrelated keys in either survive.
An absent `cli-config.json` is created on install and never created by uninstall.
Override the paths with `--cursor-hooks` / `TMA_CURSOR_HOOKS` and
`--cursor-cli-config` / `TMA_CURSOR_CLI_CONFIG`.

Caveat: the hooks are **user-level**. Cursor fires hooks only from
`~/.cursor/hooks.json`, not a project-level `.cursor/hooks.json`, so the wiring is
global to your user rather than per-repository. `tma` writes cursor's schema
(`{"version": 1, "hooks": {"<event>": [{"command": "…"}]}}`) and preserves any
unrelated hooks already there. Cursor exposes no permission hook, so `blocked` is
detected from the screen rather than a hook.

## pi

Config: a self-contained JS extension at
`~/.pi/agent/extensions/tma.js` (default; `$PI_CODING_AGENT_DIR/extensions/` if
set).

```
tma install-hooks pi
tma install-hooks pi --check
tma install-hooks pi --uninstall
```

Caveat: pi has no JSON hook block. It auto-discovers extension modules from
`~/.pi/agent/extensions/`, so `tma` drops a `tma.js` file there that subscribes to
pi's events and shells out to `tma-hook` fire-and-forget. The extension is inert
outside tmux and never blocks pi. pi auto-runs tools with no approval prompt, so
there is no `blocked` state for it at all.

## Verifying everything at once

A bare `--check` inspects every known agent plus the shared wrapper and tmux
hooks, and its exit code reflects drift (0 = wired, 1 = incomplete):

```
$ tma install-hooks --check
tma: hooks OK
```

If something is missing it names it, for example after an uninstall removed the
tmux server hooks:

```
$ tma install-hooks --check
tma: hook wiring incomplete:
  - tmux hook after-select-pane missing (config reload?)
  - tmux hook after-select-window missing (config reload?)
run `tma install-hooks <agent>` to reinstall
```

The tmux hooks are runtime server state, so a `kill-server` or a reboot drops them
even though the agent config is untouched. `--check` calls that case out
separately ("installed but not present on this server, likely restarted"); see
[making the hooks survive a restart](#making-the-tmux-hooks-survive-a-server-restart).

## The attention-clear tmux hooks

`tma install-hooks <agent>` also installs two tmux server hooks so a pane's
attention flag clears the moment you look at it. You do not add these yourself;
they are shown here so you recognize them in `show-hooks`:

```
$ tmux show-hooks -g | grep clear-attention
after-select-pane[0] run-shell "if [ -x '/usr/local/bin/tma' ]; then '/usr/local/bin/tma' clear-attention '#{hook_pane}'; else tma clear-attention '#{hook_pane}' 2>/dev/null || true; fi"
after-select-window[0] run-shell "if [ -x '/usr/local/bin/tma' ]; then '/usr/local/bin/tma' clear-attention '#{hook_pane}'; else tma clear-attention '#{hook_pane}' 2>/dev/null || true; fi"
```

The command names the binary tma was installed from and falls back to whatever
`tma` is on `$PATH` when that path is gone, so a rebuild or a move does not leave
a dead hook behind. `tma install-hooks --check` compares each installed hook
against the command this build would write and reports a mismatch as stale; the
next `tma install-hooks <agent>` rewrites it in place.

On every `select-pane` and `select-window`, `tma clear-attention` drops
`@agent_attention` on the focused pane, so the `done`/blocked flag reverts to
plain idle as soon as you jump to (or manually switch to) that pane. That is how
the attention lifecycle closes.

If your tmux has `focus-events on`, you can also clear attention on terminal focus
changes (switching into the tmux window from another app) by opting in:

```toml
[focus]
events = true
```

This installs an additional `pane-focus-in` hook. It is off by default because it
requires `focus-events on` to be set in tmux.

### Making the tmux hooks survive a server restart

`set-hook` writes *runtime* server state. A `kill-server`, a reboot, or the last
client detaching from a server started with `exit-empty on` takes the hooks with
it, and nothing reinstalls them: `tma install-hooks` is the only writer, and the
next `tma` command does not re-run it. `tma install-hooks --check` and `tma doctor`
name that state on its own ("installed but not present on this server, likely
restarted") so it is not confused with never having installed them.

To make them durable, put the same commands in your tmux config, substituting your
own `tma` path:

```tmux
set-hook -ga after-select-pane "run-shell \"if [ -x '/usr/local/bin/tma' ]; then '/usr/local/bin/tma' clear-attention '#{hook_pane}'; else tma clear-attention '#{hook_pane}' 2>/dev/null || true; fi\""
set-hook -ga after-select-window "run-shell \"if [ -x '/usr/local/bin/tma' ]; then '/usr/local/bin/tma' clear-attention '#{hook_pane}'; else tma clear-attention '#{hook_pane}' 2>/dev/null || true; fi\""
```

Two things matter here:

- Use `-ga`, not `-g`. An unindexed `set-hook -g` *replaces the whole hook array*,
  so it deletes tma's entry (and anyone else's) on every `source-file`, the same
  hazard `--check` reports as a wiped hook.
- Keep the command byte-identical to what tma installs. Copy it out of
  `tmux show-hooks -g | grep clear-attention` rather than retyping it: `--check`
  compares against the command this build writes, so a hand-shortened variant is
  reported as stale, and re-running install rewrites the runtime copy while your
  conf line puts the old one back at the next restart.

Alternatively, skip the conf entirely and re-run `tma install-hooks <agent>` after
a server restart; `tma doctor` tells you when that is needed.

## What the last uninstall cleans up

Uninstalling the last wired agent also clears tma's `@agent_*` pane options from
every pane on the server. Nothing refreshes them once the wiring is gone, so a
`#{@agent_state}` left in a border or status format would otherwise show one
frozen state forever.

One thing it does not touch: a status-line entry you added yourself, such as

```tmux
set -g status-right '#(tma status)'
```

tma never wrote that line, so it never edits it; the uninstall prints a reminder
and leaves your config alone.

## Sharing one agent config between machines

By default every wiring names the wrapper by its absolute path:

```json
"command": "/Users/you/.local/bin/tma-hook claude Stop"
```

That path is correct on the machine that wrote it and wrong on any other, so a
`~/.claude/settings.json` you sync between a Mac and a Linux box points at a home
directory that does not exist on one of them. Switch to the portable form:

```toml
# ~/.config/tma/config.toml
[install]
wrapper_ref = "bare"
```

Then re-run `tma install-hooks <agent>` for each wired agent and the entries
become `tma-hook claude Stop`, which every machine resolves off its own `$PATH`.
`--wrapper-ref bare` does the same for a single run without touching config.

`$HOME` is deliberately not an option here. Half the wiring never reaches a
shell: Codex's `notify` is an argv array, and the OpenCode plugin and pi extension
call `spawn()` directly, so `$HOME/.local/bin/tma-hook` would be taken as a
literal filename with a dollar sign in it. A bare name works everywhere because
`execvp` searches `$PATH` the same way a shell does.

What you give up is the guarantee that the wrapper is findable. A GUI-launched
editor often inherits a narrower `$PATH` than your shell, and a wrapper an agent
cannot find fails silently by design. Two things guard that: `install-hooks`
refuses to wire anything when `tma-hook` is not on the `$PATH` it can see, and
`tma doctor` reports the reference rather than the file:

```
wrapper: tma-hook ✓ on $PATH (/home/you/.local/bin/tma-hook)
```

A `✗ not on $PATH` there means the wiring is intact and inert. Put the wrapper's
directory on the agent's `$PATH`, or switch back to `wrapper_ref = "absolute"` and
re-install.

## Agents that run in a container

Install the hooks where the agent's config lives, which is inside the container,
and give it the tmux socket plus the pane id so its events reach the host server.
The full recipe, including the one identity carve-out that will otherwise wipe the
pane's state, is [Run an agent in a
container](agents-in-containers.md).

## Overriding paths

For a non-default config location (test isolation, an XDG-relocated home), every
path has a flag and a matching environment variable, listed under
[`tma install-hooks`](../reference/cli.md#tma-install-hooks). For example
`--codex-hooks <path>` / `TMA_CODEX_HOOKS`, `--gemini-settings <path>` /
`TMA_GEMINI_SETTINGS`, `--pi-extension <path>` / `TMA_PI_EXTENSION`.

Two more variables are read by the installed `tma-hook` wrapper itself, at fire
time rather than at install time, so setting either changes what an already-wired
agent does:

| variable | effect |
|---|---|
| `TMA_BIN` | The `tma` binary to run. Taken only when it is set and executable; otherwise the wrapper falls back to a `tma` sitting next to itself, then to `$PATH`. That resolution happens on every fire, which is why a rebuild or a move never surfaces to the agent as a hook failure. |
| `TMA_HOOK_SOCKET` | Pin the tmux server by name, as `tmux -L <name>` does. Unset, the wrapper passes no socket flag and `tma` uses the `$TMUX` the pane inherited, which is what you want for a normal install. It exists for the test suite and for setups running more than one server. |

Neither is written by `install-hooks`; export them in the environment the agent
starts in.
