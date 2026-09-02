# Configuration

`tma` reads an optional TOML config. Every setting has a working default, so
zero-config works: an absent or partial file yields exactly the defaults shown
below. Unknown keys are a loud parse error (a typo'd table fails every
subcommand rather than being silently ignored), so this page lists the full set.

## File location and precedence

The config path is resolved in this order, highest first:

1. `--config <path>`
2. the `TMA_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/tma/config.toml`
4. `~/.config/tma/config.toml`

An absent file at every source is the zero-config floor (all defaults). A
present but malformed file fails loudly, naming the file and the offending key,
rather than falling back to defaults.

One-shot surfaces (`status`, `ls`, `event`, `jump`, `doctor`) read the file once
per invocation, so they always reflect the current file. The daemon re-reads
config and manifests on SIGHUP (or `tma reload`), and the picker re-reads on its
refresh tick; an invalid reloaded file is kept-old, never fatal.

## Full example (built-in defaults)

The values shown are the defaults. Copy any subset; omitted sections and keys
keep their defaults.

```toml
[fold]                       # state-machine tuning (seconds)
dwell_secs = 3               # anti-flicker dwell before a working->idle drop
hook_decay_secs = 60         # how long a hook claim outweighs screen evidence
blocked_decay_secs = 300     # the same window for a blocked claim (a prompt sits silent)
freshness_secs = 3           # stamp-freshness window

[status]                     # `tma status` glyphs + colors; partial entries keep other defaults
blocked = { glyph = "⚑", color = "red" }
working = { glyph = "●", color = "yellow" }
idle    = { glyph = "○", color = "green" }
done    = { glyph = "✓", color = "magenta" }   # idle pane still flagged for attention
unknown = { glyph = "?", color = "colour244" }

[picker]                     # ratatui picker glyphs + colors; same shape as [status]
unknown = { glyph = "?", color = "darkgray" }

[notify]                     # notifications
from_event = false           # daemonless direct-fire opt-in
# command = "my-notify-hook" # optional notification hook command
on = ["blocked"]             # transitions that fire; add "done" for working->idle completions
bell = false                 # also ring the firing pane's terminal bell
osc = false                  # also post an OSC 9 desktop notification to the pane's tty
# log = "~/.local/state/tma/notifications.jsonl"  # append one JSON line per fired notification
# context_high = { threshold = 75 }  # also fire once when a pane's context crosses this percent
# blocked = { command = "..." }      # per-trigger routing; unset falls back to `command`
# done = { command = "..." }

[act]                        # the action broker's audit record
# log = "~/.local/state/tma/acts.jsonl"  # append one JSON line per fired action

[focus]                      # attention-clear posture
events = false               # set true to also install a pane-focus-in hook (needs `focus-events on`)

[install]                    # what install-hooks writes into your agent configs
wrapper_ref = "bare"         # writes just `tma-hook`, resolved off $PATH; "absolute" writes the path

[tmux]                       # which tmux-compatible binary tma spawns
# bin = "tmate"              # default: plain `tmux` off PATH; env TMA_TMUX_BIN overrides this

[telemetry.windows]          # model names tma doctor recognizes; sizes parse but are ignored
# "gemini-2.5-pro" = 1048576 # shipped defaults seed only the raw-token agents that need a table

[daemon]                     # tier-3 daemon cadences
sweep_secs = 45              # reconciliation-sweep cadence
quiet_ms = 1000              # per-pane active->quiet capture trigger
zero_member_recheck_secs = 1 # clientless liveness recheck
demote_edges = 5             # hook-liveness demotion threshold
autostart = false            # auto-start the daemon on first use of a surface
restart_on_upgrade = true    # let a newer tma replace an older resident daemon

[[agent]]                    # per-agent overrides (repeatable)
name = "claude"
enabled = true
process_names = ["claude"]   # extra launcher basenames to match
```

## `[fold]`: detection tuning

State-machine tuning, in seconds. These feed the pure fold that resolves
evidence into a verdict.

| key | default | meaning |
|---|---|---|
| `dwell_secs` | `3` | Anti-flicker dwell before a working-to-idle drop is published. |
| `hook_decay_secs` | `60` | How long a working or idle hook claim outweighs later screen evidence. |
| `blocked_decay_secs` | `300` | The same window for a `blocked` hook claim. Longer because a permission prompt sits silent for minutes; only positive contrary chrome on a screen that can see `blocked` may expire it. |
| `freshness_secs` | `3` | Stamp-freshness window: a stamp older than this is stale. |

## `[status]` and `[picker]`: glyphs and colors

`[status]` styles the `tma status` one-liner; `[picker]` styles the fuzzy
picker. Both take one entry per state class: `blocked`, `working`, `idle`,
`done`, and `unknown`. Each entry is a `{ glyph, color }` table; a partial entry
keeps the other class defaults.

- `glyph` is the character rendered for that class.
- `color` is a tmux color string: a name (`red`), an indexed color
  (`colour244` / `color12`), or hex (`#ff8800`). For `[status]` it is embedded
  verbatim in `#[fg=...]` and validated by tmux.

The `done` class is an idle pane whose output is still unreviewed (it carries
`@agent_attention`); its underlying `@agent_state` token stays `idle`, only the
surface split changes.

## `[notify]`: notifications

| key | default | meaning |
|---|---|---|
| `from_event` | `false` | Opt in to daemonless direct-fire from `tma event`. |
| `command` | unset | Optional notification hook command. Receives the notify payload on stdin (see [Pane options and JSON contracts](pane-options-and-json.md)). |
| `on` | `["blocked"]` | Which transitions fire a notification. Add `"done"` for working-to-idle completions. |
| `bell` | `false` | Also ring the firing pane's terminal bell. |
| `osc` | `false` | Also write an OSC 9 desktop notification (`<agent> <state>`) to the firing pane's tty. Off by default because emulator support varies; it crosses ssh/mosh/tmate, since the emulator at your end renders it. See [Set up notifications](../how-to/notifications.md#post-a-desktop-notification-from-the-terminal). |
| `log` | unset | Path to a JSONL file; every fired notification appends one line (the hook payload plus an `at` epoch). `~` is expanded and parent directories are created; the file is created `0600`. Errors are silent, never failing a hook. |
| `include_title` | `false` | Send the pane title to the notify carriers. Off by default: a pane title routinely holds a branch name, a repo path or a prompt fragment, and `command` pipes the payload to whatever you configured (ntfy, Pushover, a Shortcut), so the title would reach that service's operator. Turning it on restores the payload's `title` key, the `TMA_TITLE` variable and the audit line's title together. The host-local `display-message` always shows the title and is unaffected. |
| `blocked` | unset | A sub-table routing the `blocked` trigger: `{ command = "..." }`. |
| `done` | unset | The same for the `done` trigger. |
| `context_high` | unset | A sub-table `{ threshold = <percent>, command = "..." }`. When present, fire once when a pane's context utilization crosses `threshold`; naming the sub-table is the opt-in, so `threshold` is required (`command` is not). Unset means no context notifications. |

`context_high` is separate from `on` because it rides its own armed flag
(`@agent_context_notified_at`), not the state lane's marker: it fires once on the
crossing, holds while the gauge stays high, and rearms only after the gauge dips
below `threshold - 10`. See [Set up notifications](../how-to/notifications.md#notify-on-high-context).

### Per-trigger routing

Each trigger may name its own command in a `[notify.<trigger>]` sub-table. A
trigger with no sub-table (or a sub-table with no `command`) falls back to the
global `notify.command`, so routing one trigger elsewhere leaves the others
alone. Here a blocked agent pushes to your phone while completions only append to
a log:

```toml
[notify]
on = ["blocked", "done"]

[notify.blocked]
command = "curl -s -d \"$TMA_AGENT blocked in $TMA_LOCATOR\" https://ntfy.sh/your-topic"

[notify.done]
command = "cat >> ~/.local/state/tma/done.jsonl"
```

An unknown key inside a sub-table is a loud parse error like everywhere else, so
a mistyped override never silently falls back to the global command. The
`TMA_NOTIFY_CMD` environment variable outranks all of them: when set it replaces
every trigger's command (it exists so a test or CI run funnels every fire into
one sink).

`TMA_NOTIFY_FROM_EVENT` is its sibling on the other knob. Whenever it is set at
all it decides `from_event` on its own: exactly `1` turns the daemonless direct
fire on, and any other value (including the empty string) turns it off, so
exporting `TMA_NOTIFY_FROM_EVENT=0` overrides a `from_event = true` in config.
Only an unset variable leaves the config value in charge. Like `TMA_NOTIFY_CMD` it
exists so a test or CI run can flip the fire path without writing a config file.

## `[act]`: the action audit log

| key | default | meaning |
|---|---|---|
| `log` | unset | Path to a JSONL file; every `tma act` fire appends one line, refusals included, naming the surface that asked. `~` is expanded and parent directories are created; the file is created `0600`. Errors are silent, never failing an action. Key set and rationale: [The act audit log](cli.md#the-act-audit-log). |

## `[focus]`: attention-clear posture

| key | default | meaning |
|---|---|---|
| `events` | `false` | Set `true` to also install a pane-focus-in hook. Requires tmux `focus-events on`. |

## `[install]`: how agent configs name the wrapper

| key | default | meaning |
|---|---|---|
| `wrapper_ref` | `"bare"` | `"bare"` writes the name `tma-hook` into each agent config and lets the agent resolve it off `$PATH`. `"absolute"` writes the wrapper's full path. |

`"bare"` is the default because it fails loudly. One string, `tma-hook`, is
correct on every machine, so a `~/.claude/settings.json` synced between a Mac and
a Linux box works on both. Its failure mode is that the wrapper's directory has to
be on the `$PATH` each agent inherits, and that failure is caught at the one moment
there is a person to tell: `tma install-hooks` refuses to wire anything when
`tma-hook` is not findable, and `tma doctor` reports whether `$PATH` still answers
it:

```
wrapper: tma-hook ✓ on $PATH (/home/you/.local/bin/tma-hook)
```

`"absolute"` fails silently by comparison. `/Users/you/.local/bin/tma-hook` on
macOS is `/home/you/.local/bin/tma-hook` on Linux, and a synced config carrying the
wrong one produces no error anywhere: the wrapper simply never runs, and hooks
never fire. Choose it when an agent is launched with a `$PATH` you cannot widen:
a GUI-launched editor (Cursor started from the dock) inherits the desktop
session's `$PATH`, not your login shell's.

When it is chosen, `"absolute"` writes one path that is not always the wrapper's
own: when the wrapper lives in a package store (`/nix/store`, `/gnu/store`), the
config gets the stable path that reaches it instead, your profile's `bin`, found
by walking `$PATH` for a `tma-hook` outside the store that resolves to the same
file. A store path names one build and is deleted with it, so writing one would
break your hooks at the next upgrade. `tma doctor` prints the reference first and
the file it points at in parentheses.

A `$HOME`-relative string is not offered, because it would only work for half the
agents. Three of the six wiring mechanisms spawn the wrapper as argv with no shell
involved (Codex's `notify` array, the OpenCode plugin's and pi's `spawn`), and
those would pass `$HOME/.local/bin/tma-hook` through as a literal filename. A bare
name is resolved by all six, since `execvp` searches `$PATH` exactly like a shell
does.

Set the key before installing, or pass `--wrapper-ref bare` / `--wrapper-ref
absolute` for one run.

### Switching between the two

Wiring already installed under the other posture keeps working, and `--check` and
`tma doctor` say so: drift is judged by what a reference RESOLVES to, not by how it
is spelled. An absolute path and the bare name that finds the same file are the
same wiring, so an install made before the default changed does not start
reporting as stale. Only a reference that resolves to a different file, or to
nothing at all, is drift.

Run `tma install-hooks --all` when you want the configs rewritten to the posture
you chose. One cost is worth knowing before you do: codex pins its `hooks.json`
trust to the exact command string (`trusted_hash` per entry in
`~/.codex/config.toml`), so rewriting those entries makes them inert until you
open codex, run `/hooks`, and trust them again. Codex's `notify` channel and every
other agent are unaffected.

## `[tmux]`: which tmux binary to spawn

| key | default | meaning |
|---|---|---|
| `bin` | `tmux` | The tmux-compatible binary every spawn uses: a `PATH` name (`tmate`), or a path (`/opt/homebrew/bin/tmux`) — anything containing a `/` is used as-is. |

Precedence, highest first: the `TMA_TMUX_BIN` environment variable, then this
key, then plain `tmux`. The env wins so one shell can be pointed at another tmux
without editing config:

```sh
TMA_TMUX_BIN=tmate tma --socket-path /tmp/tmate-501/default ls
```

A configured binary that does not resolve is reported as the same
not-installed error an absent `tmux` gives, naming what to install — never a
per-spawn "no such file".

**When you need this.** A tmux client only talks to a server built from the same
protocol version. Point tma at a tmate socket with the ordinary `tmux` client and
every command fails with a protocol-version mismatch; tma reports that as its own
error naming this key, rather than passing tmux's terse line through:

```
$ tma --socket-path /tmp/tmate-501/default ls
tma: tmux protocol version mismatch: the `tmux` client and this server were built
from different versions (are you inside tmate, or is a second tmux first on PATH?);
point tma at the matching client with `[tmux] bin` in config.toml or TMA_TMUX_BIN
```

The fix is to spawn tmate's own client (`bin = "tmate"`). The same applies to a
second tmux from another package manager sitting first on `PATH` — name the one
that matches your server. Setting `bin` covers control mode too, so the daemon
attaches with the same client everything else spawns.

## `[daemon]`: tier-3 daemon cadences

The daemon is strictly additive; these knobs apply only when it runs.

| key | default | meaning |
|---|---|---|
| `sweep_secs` | `45` | Reconciliation-sweep cadence. |
| `quiet_ms` | `1000` | Per-pane active-to-quiet capture trigger, in milliseconds. |
| `zero_member_recheck_secs` | `1` | Clientless-session liveness recheck cadence. |
| `demote_edges` | `5` | Hook-liveness demotion threshold: activity edges the pane's hooks do not account for before its coverage is treated as suspect. An edge landing on a fresh hook claim does not count, and neither does one on a pane whose hooks last said `working` and have not yet been contradicted by capture, so a single long tool call cannot demote a healthy pane. |
| `autostart` | `false` | Auto-start the daemon on first use of a surface (`ls`/`status`/`jump`/picker/`watch`/`wait`/`subscribe`). |
| `restart_on_upgrade` | `true` | Replace a resident daemon whose build is **strictly older** than the binary running the check. Runs from every user surface, from `tma event`, and from `tma daemon --ensure`. Set `false` to opt out. |

### `restart_on_upgrade`

A daemon keeps the detection code it started with, so after upgrading `tma` the
one already running is still the old build until something replaces it. Nothing
about a package upgrade touches a resident process, so without this the daemon
serving your tmux server can be days behind the CLI you are typing.

On by default. The check runs before every user-invoked surface
(`ls`/`status`/`jump`/picker/`watch`/`wait`/`subscribe`), from `tma event` (the
hook path), and from `tma daemon --ensure`, so an upgrade is picked up on your
next command rather than at the next tmux server restart. It does not need
`autostart`, and it is not affected by it.

It only ever REPLACES. With no daemon running it does nothing: starting one
unasked is `autostart`'s job, and that is still off by default. Opt out with:

```toml
[daemon]
restart_on_upgrade = false
```

`tma daemon --restart` remains the on-demand, direction-free version (it is how a
deliberate downgrade is served).

The rule is deliberately one-directional, which is what makes it safe to leave on.
**Strictly newer replaces older.**
Equal never restarts, and an older `tma` never touches a newer daemon — that is
the direction of skew the wire protocol tolerates anyway (a capability the old
peer does not know is a discriminant it rejects cleanly). Because the relation is
strict, no two builds can ever replace each other, so two `tma` installs sharing
one tmux server cannot take turns evicting each other's daemon.

Three further conditions have to hold, all of them fail-safe:

- Both versions must parse as `MAJOR.MINOR.PATCH`. Anything else never restarts.
- The pid in the lock file must still be alive. A lock file keeps its body after
  the daemon exits, and a dead pid is nothing to replace.
- No automatic restart may have fired for this server in the last 60 seconds. The
  version rule cannot loop, but a new build whose daemon will not stay up can
  flap; this bounds that to once a minute. `tma daemon --restart` is never
  subject to it.

A restart costs about 35 ms with nothing listening and a couple of seconds where
the socket is bound but the daemon is still running its control-mode probe.
Nothing is lost across either: a hook that cannot reach a daemon stamps the pane
itself, `tma wait` degrades to polling, and notification de-duplication lives in
a pane option that outlives the process.

The common case, where the versions already match, costs one small file read and
one liveness probe per command. That is the whole price of leaving it on.

## `[telemetry.windows]`: recognized model names

A set of model names `tma doctor` recognizes. Nothing else reads it.

The table was originally a `model -> window size` lookup for a telemetry channel
that reported raw token counts with no window of its own. No shipped channel
does: Claude precomputes its context percent, Codex carries
`model_context_window` in its rollout, and pi and Cursor each send the window
their payload's own numbers are divided by. A channel with no usable window
stamps nothing rather than guessing one, so no gauge has ever been sized here.

What is left is name recognition. `tma doctor` reports a stamped `@agent_model`
that no entry names as unrecognized; that is a label, not a warning, and it does
not affect `doctor --exit-code`. Adding an entry only quiets that line.

Three `gemini-*` names ship as recognized, left over from the sizing era; your
entries add to them. The TOML shape is unchanged and the sizes still have to
parse, so an existing config keeps loading; the numbers are ignored.

```toml
[telemetry.windows]
"gpt-5-codex" = 272000        # any number; only the name is read
```

## `[[agent]]`: per-agent overrides

A repeatable table for enabling or disabling a bundled or user manifest, and for
extending a manifest's identity match with extra launcher basenames. This is the
supported extension surface for adjusting a shipped agent; adding a brand-new
agent is a manifest (see [Manifest schema](manifest-schema.md)).

| key | default | meaning |
|---|---|---|
| `name` | (required) | The agent name this override applies to. |
| `enabled` | `true` | Set `false` to disable detection for this agent. |
| `process_names` | `[]` | Extra `#{pane_current_command}` basenames to match, added to the manifest's own list (so a wrapper binary or renamed build is recognized as this agent). |

Arbitrary custom hook-to-state mapping is not a config surface: the
process-name extension above is the configuration extension point, and a hook
map belongs in a manifest.

## `[api.<name>]`: per-agent API endpoint

A fallback server base URL for the action broker's API lane (the manifest side of
that lane is the
[`[api]` transport](action-manifest-schema.md#api-per-agent-api-channel-transports)).
A table keyed by agent name, separate from the `[[agent]]` override array above.
Only used when the pane carries no plugin-stamped
`@agent_api_endpoint` — for OpenCode the plugin normally stamps it, so this is the
manual override for a non-standard `opencode serve` address.

```toml
[api.opencode]
api_base = "http://127.0.0.1:4096"   # answer permission prompts against this server
```

| key | default | meaning |
|---|---|---|
| `api_base` | (none) | The `http://host:port` base the broker POSTs a `permission-reply` to when the pane has no stamped endpoint. |
