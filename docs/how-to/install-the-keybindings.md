# Install the keybindings

Put the picker, the watch dashboard, and the jumps on your prefix key. What each key does
is in [Keybindings](../reference/keybindings.md); this page is how the bindings
get onto your tmux server and how to change them once they are.

## Install

```
tma install-keys
```

This writes the default bindings to a managed file (`~/.config/tma/tmux.conf`) and
adds one line to your tmux config. The line names the file through tmux's own
variable expansion, so a tmux config kept in a dotfiles repo works on every
machine; `-q` makes the `XDG_CONFIG_HOME` path a quiet miss when that variable is
unset, and the `$HOME` one loads instead:

```tmux
source-file -q "$XDG_CONFIG_HOME/tma/tmux.conf" "$HOME/.config/tma/tmux.conf" # tma keys
```

With `--config-dir` (or `TMA_CONFIG_DIR`) the line carries that literal path
instead, double-quoted so a dir with a space still parses.

It shows the diff and asks before writing, naming the file it resolved; pass
`--yes` to skip the prompt. Reload tmux afterward, pointing at whichever config
the diff named:

```
tmux source-file ~/.config/tmux/tmux.conf
```

## Which config gets the line

tma does not assume `~/.tmux.conf`. It marks the first of tmux's own config files
that exists, in tmux's load order:

1. `~/.tmux.conf`
2. `$XDG_CONFIG_HOME/tmux/tmux.conf`
3. `~/.config/tmux/tmux.conf`

(tmux 3.6 sources every one of those that exists, in that order, so a later file's
`set` wins; older tmux loads only the first.) If none exists, tma creates
`$XDG_CONFIG_HOME/tmux/tmux.conf` when `XDG_CONFIG_HOME` is set; with it unset it
creates `~/.config/tmux/tmux.conf` if `~/.config` is there, and `~/.tmux.conf`
otherwise. Creating only ever happens when you have no tmux config at all, so the
file tma creates can never shadow one you already have. `--conf <path>` overrides
all of this.

## Verify and undo

```
tma install-keys --check
tma install-keys --uninstall
```

`--check` confirms the managed file is current and that your tmux config sources
it exactly once; both resolve the config the same way install did. `--uninstall`
removes the managed file and the marked `source-file` line, and nothing else:
your own bindings live in your tmux config, tma's live only in the managed file,
so there is nothing of yours to corrupt.

## Rebind a key

`install-keys` claims only keys that are unbound in stock tmux (that is why the
watch window is on `G`: `g` is already `jump --blocked`). If one clashes with a
personal binding of yours, copy the line you want out of
`~/.config/tma/tmux.conf` into your own tmux config with a new key and drop the
managed file. Editing the managed file in place also works, but the edit survives
only until the next `install-keys` run, which rewrites the whole file from tma's
defaults.

## Clickable status segments

Each count `tma status` prints is wrapped in a tmux range marker, so a click can
be resolved to the class you clicked. The bindings that act on that are opt-in:

```
tma install-keys --mouse
```

They also need tmux's mouse mode, which tma never turns on for you, because it
changes selection and copy/paste in every pane (drag-select stops reaching your
terminal; hold `Shift` to get the terminal's own selection back). Turn it on
yourself if you want it:

```tmux
set -g mouse on
```

With both in place, the [mouse table](../reference/keybindings.md#mouse-bindings)
applies: the blocked count jumps to the longest-blocked agent, any other count
opens the picker popup, and a right-click on either opens the agent menu.

One thing a click cannot do is close the popup it opened. tmux delivers mouse
events to an open `display-popup` and drops everything outside it, so the second
click on the status line never reaches a binding. `Esc` closes it.

The range markers ship always. Without `--mouse`, or without `set -g mouse on`,
they are inert: tmux draws the counts exactly as before, and nothing is
clickable. `tma doctor` flags the half-wired case (bindings installed, `mouse`
off).

The cost of opting in: those four bindings claim tmux's status-line mouse keys
for the whole status line, not just tma's segments. A left-click elsewhere still
switches to the window you clicked (the binding ends with tmux's own
`switch-client -t=`), but a right-click on a window name no longer opens tmux's
window menu (`Alt`-right-click still does, since tmux binds that separately). If
you would rather keep the plain right-click, delete the two `MouseDown3` lines
from `~/.config/tma/tmux.conf`.

`tma install-keys --check --mouse` verifies the group is installed; a plain
`--check` accepts a file with or without it, so not opting in is never reported
as drift.

## The daemon launcher

Every install writes one line that is not a binding:

```
run-shell -b 'tma --socket-path "#{socket_path}" daemon --ensure >/dev/null 2>&1'
```

It starts the event-hub daemon for whichever server loads the file, so a new tmux
server is at tier 3 before you open a surface. It is safe on a re-source
(`--ensure` takes a single-instance lock) and needs no matching stop line,
because the daemon exits when its tmux server does.

To skip it:

```
tma install-keys --no-daemon
```

That is a standing choice rather than a one-off: a plain `--check` calls the
missing line drift, so use `--check --no-daemon` in whatever verifies your setup.
Details and the other ways to start a daemon in
[Run the daemon](run-the-daemon.md).

## Bind them by hand instead

If you would rather not let tma write config, add the bindings to your tmux
config yourself. These are the same shapes `install-keys` writes.

The picker opens best in a popup. `display-popup` does **not** format-expand its
command, so pass no `--client`: the picker resolves the client that opened the
popup itself (a `--client "#{client_name}"` here would arrive literal and target
a client that does not exist).

```tmux
bind-key a display-popup -E -w 80% -h 60% 'tma'
```

`tma watch` is a persistent dashboard for a normal pane, and the full-width table
wants the whole terminal, so bind it to a new window. `new-window` does **not**
format-expand, so again no `--client`. `--table` opens straight into the table;
`p` inside `tma watch` toggles it against the preview:

```tmux
bind-key G new-window 'tma watch --table'
```

If you would rather have it beside your work than in a window of its own, bind a
split instead — tma has no opinion about placement, and a narrow pane falls back
to the single-column list:

```tmux
bind-key W split-window -h -l 40 'tma watch'
```

Mind that a split follows nothing: jump to an agent in another window and the
pane stays behind in the window you left.

Jump straight to whoever needs you, no picker. `run-shell` does format-expand, so
pass the client: `tma` then switches the client that pressed the key and keys the
`--back` origin by it:

```tmux
bind-key j run-shell 'tma jump --attention --client "#{client_name}"'
bind-key g run-shell 'tma jump --blocked --client "#{client_name}"'
bind-key b run-shell 'tma jump --back --client "#{client_name}"'
bind-key h run-shell 'tma jump --home --client "#{client_name}"'
```

The action menu for the pane you are standing in, likewise expanded by
`run-shell`:

```tmux
bind-key A run-shell 'tma act --menu --pane "#{pane_id}"'
```

Should an old binding still pass an unexpanded `#{client_name}`, `tma` reads it
as no client at all and falls back to resolving the acting client itself.

`tma watch` runs in a normal pane, so `q`, `Esc`, or `ctrl-c` inside it quits and
takes the pane (or window) with it.

## What a binding can reach

A binding is a shell command running as you, so it can do anything `tma` can do,
which is anything you can do to your own tmux server. That is the whole boundary;
see [The security model](../explanation/security-model.md) for what follows from
it, particularly before you bind an action that writes.
