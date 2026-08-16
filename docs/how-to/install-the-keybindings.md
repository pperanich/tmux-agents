# Install the keybindings

Put the picker, the sidebar, and the jumps on your prefix key. What each key does
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

`install-keys` claims only keys that are unbound in stock tmux (that is why
`watch` is on `W`: `w` is tmux's own window chooser). If one clashes with a
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
applies. The `☰` icon is the one segment that is always there, so the sidebar is
reachable even on a status line showing no counts. Clicking it splits `tma watch`
beside the pane you are in, 40 columns wide, without moving your focus; clicking
it again kills that pane. It finds the running sidebar by the pid a `tma watch`
advertises on its own pane, so a sidebar you opened by hand (`prefix W`) is the
one the next click closes, and only sidebars in the clicking client's session are
touched.

Restyle the icon, or drop it entirely, with a `sidebar` entry under `[status]`:

```toml
[status]
sidebar = { glyph = "S", color = "blue" }   # or glyph = "" to remove the segment
```

The toggle is not mouse-only. `tma watch --toggle` works from the command line
and from a key binding, where `run-shell` format-expansion means it should be
passed the clicking or pressing client:

```tmux
bind-key T run-shell 'tma watch --toggle --client "#{client_name}"'
```

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

`tma watch` is a persistent sidebar for a normal pane. `split-window` does
**not** format-expand either, so again no `--client`:

```tmux
bind-key W split-window -h -l 32 'tma watch'
```

The full-width status table wants the whole terminal, so bind it to a new window
rather than a narrow split (a 32-column split would fall back to the single
list). `--table` opens straight into the table; `p` inside `tma watch` toggles it
against the preview:

```tmux
bind-key G new-window 'tma watch --table'
```

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

The sidebar is a normal pane, so the same key toggles it off by killing the pane
when you are in it (`q`, `Esc`, or `ctrl-c` also quit `tma watch`).

## What a binding can reach

A binding is a shell command running as you, so it can do anything `tma` can do,
which is anything you can do to your own tmux server. That is the whole boundary;
see [The security model](../explanation/security-model.md) for what follows from
it, particularly before you bind an action that writes.
