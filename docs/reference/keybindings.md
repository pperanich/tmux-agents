# Keybindings

Every key tma binds or reads, in one place. The prefix and mouse bindings are the
ones `tma install-keys` writes; for installing, rebinding, or removing them see
[Install the keybindings](../how-to/install-the-keybindings.md). The rest are keys
the live surfaces read for themselves, so they need no binding at all.

## Prefix bindings

Written to the managed file `~/.config/tma/tmux.conf`. All are on your tmux
prefix.

| key | tmux command | does |
|---|---|---|
| `a` | `display-popup -E -w 80% -h 60% 'tma'` | Open the picker in a popup. |
| `G` | `new-window 'tma watch --table'` | Open the full-width status table in a new window. |
| `j` | `run-shell 'tma jump --attention --client "#{client_name}"'` | Jump to whoever wants you: blocked first, then finished-unreviewed. |
| `g` | `run-shell 'tma jump --blocked --client "#{client_name}"'` | Jump to the longest-blocked agent. |
| `b` | `run-shell 'tma jump --back --client "#{client_name}"'` | Return one step along the jump trail. |
| `h` | `run-shell 'tma jump --home --client "#{client_name}"'` | Return to the trail's oldest origin. |
| `A` | `run-shell 'tma act --menu --pane "#{pane_id}"'` | Open the action menu for the active pane. |

`G` rather than `g`: `g` here is already `jump --blocked`. `install-keys` claims
only keys that are unbound in stock tmux.

Only the `run-shell` bindings carry `--client "#{client_name}"`, because only
`run-shell` format-expands its command. `display-popup` and `split-window` do not,
so the flag would arrive as the literal `#{client_name}`; from inside a popup or a
pane, `tma` resolves the acting client itself.

## Mouse bindings

Opt-in (`tma install-keys --mouse`), bound in tmux's root table so they need no
prefix, and inert without `set -g mouse on`. Each row is a click on one of the
`#[range=user|tma:…]` segments `tma status` prints.

| click | does |
|---|---|
| left-click the blocked count | `tma jump --blocked`: go to the longest-blocked agent. |
| left-click any other tma count | Open the picker popup, the same one `prefix a` opens. |
| right-click any tma segment | `tma jump --menu`: a tmux menu of every agent. |
| left-click outside a tma segment | tmux's own `switch-client -t=`, so clicking a window name still switches to it. |
| right-click outside a tma segment | Nothing. `Alt`-right-click still opens tmux's own window menu. |

Four bindings carry all of that: `MouseDown1Status`, `MouseDown1StatusRight`,
`MouseDown3Status`, and `MouseDown3StatusRight`. The left-click chain matches in
the order listed above, first match wins.

There is no click that dismisses the popup the counts open. tmux drops every
mouse event that lands outside an open `display-popup`, so a second click on the
status line never reaches a binding — `Esc` closes it.

## Keys inside the picker

The picker's own keys, once it is open. The pane you opened it from is never in
the list, so jumping to where you already are is not offered.

| key | does |
|---|---|
| `enter` | Jump to the highlighted agent, clear its attention flag, close the picker. |
| `tab` | Open the tmux action menu for the highlighted agent. |
| `ctrl-s` | Toggle the scope between every session and the invoking one. |
| `↑` / `↓` | Move the selection (wraps at both ends). |
| `backspace` | Delete the last query character. |
| any printable character | Append to the fuzzy query. |
| `esc`, `ctrl-c` | Close. |

Every printable key types, with none held back for a shortcut — an agent called
`auth` and a branch called `2fa` both have to be searchable, so the action menu
sits on `tab` and there is no digit quick-select.

The mouse works inside the popup too, with `set -g mouse on` (no `install-keys`
needed — the surface asks the terminal for reports itself):

| gesture | does |
|---|---|
| move the pointer over a row | Underline it: what a click would take. |
| click a row | Select it, the same as moving the highlight there with `↑`/`↓`. |
| click the selected row again | Jump to it and close, the same as `enter`. Two clicks, so a stray one cannot move your client. |
| wheel up / down | Move the selection three rows, stopping at each end rather than wrapping. |

Hover is an underline and selection is the reversed block, because they are
different claims: the pointer's and the keyboard's. Any keypress drops the hover
so only one row is marked, and the next pointer move brings it back.

A press anywhere else in the popup (the border, the preview half, the query line)
does nothing.

The live preview needs a popup at least 76 columns wide, the same threshold `tma
watch` uses. Narrower than that, the list takes the whole popup and nothing is
captured.

## Keys inside `tma watch`

| key | does |
|---|---|
| `enter` | Jump to the highlighted agent and clear its attention flag; the watcher stays open where it is. |
| `a`, `tab` | Open the tmux action menu for the highlighted agent. (`tab` is the picker's spelling; both work here.) |
| `p` | Swap the live preview for the full-width status table, and back. Wide body only. |
| `g` | Flatten the repo grouping, and regroup. Wide body only. |
| `k` / `j`, `↑` / `↓` | Move the selection (wraps at both ends). |
| `q`, `esc`, `ctrl-c` | Quit. |

Both `p` and `g` change the wide body, which the pane gets at 76 columns or more.
Below that the body is a single flat list and neither key changes what you see.

Note that `a` targets the pane under the cursor, not the pane `tma watch` itself
runs in, which is what lets a screenful of blocked agents be answered from one
place. See [Author a custom action](../how-to/custom-actions.md).

`tma watch` takes the same mouse gestures as the picker (hover underlines, click
selects, click again jumps — the list stays open, exactly as `enter` does;
wheel moves three rows, and any key drops the hover). Group headers are not
selectable, so hovering or clicking a `▸ repo` line does nothing.

A jump moves your client, never the watcher. Give it a window of its own
(`prefix G`) or a second terminal and the list stays put and stays visible while
you work in the window you landed in; run it in a split beside your work and a
cross-window jump leaves it behind in the window you came from. That is the
trade-off in placing it, and tma places nothing for you.

While `tma watch` is running, that pane's mouse belongs to tma: tmux's own
drag-to-select and scroll-into-copy-mode do not apply inside it (hold `shift` for
your terminal's native selection). Every other pane is untouched — tmux routes a
mouse event by where the pointer is, and only the pane under it decides.

## Keys inside `tma jump --menu`

The menu is tmux's own `display-menu`, so tmux owns the keys.

| key | does |
|---|---|
| `1`-`9` | Fire the nth entry. The tenth and later carry no digit. |
| `↑` / `↓`, `enter` | Move and fire. |
| `q`, `esc` | Dismiss. |

`tma act --menu` renders the same way, over the actions fireable on the target
pane.

## Jump directions

Which [`tma jump`](cli.md#tma-jump) flag each key runs, and the one with no key.

| flag | key | does |
|---|---|---|
| `--attention` | prefix `j` | The next agent that wants you: blocked first, then finished-unreviewed. |
| `--blocked` | prefix `g`, left-click the blocked count | The longest-blocked agent. |
| `--next` | none | The next agent after the current pane, in session then window then pane order. This is the default when no direction flag is given. |
| `--back` | prefix `b` | One step back along the return trail. |
| `--home` | prefix `h` | The trail's oldest origin, clearing the trail. |
| `--pane <ID>` | none | A named pane. What a menu entry and the picker's Enter both run. |
| `--menu` | right-click any tma segment | A tmux menu of every agent. |
