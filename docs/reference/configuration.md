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

[focus]                      # attention-clear posture
events = false               # set true to also install a pane-focus-in hook (needs `focus-events on`)

[install]                    # what install-hooks writes into your agent configs
wrapper_ref = "absolute"     # "bare" writes just `tma-hook`, resolved off $PATH (portable)

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
| `log` | unset | Path to a JSONL file; every fired notification appends one line (the hook payload plus an `at` epoch). `~` is expanded and parent directories are created. Errors are silent, never failing a hook. |
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

## `[focus]`: attention-clear posture

| key | default | meaning |
|---|---|---|
| `events` | `false` | Set `true` to also install a pane-focus-in hook. Requires tmux `focus-events on`. |

## `[install]`: how agent configs name the wrapper

| key | default | meaning |
|---|---|---|
| `wrapper_ref` | `"absolute"` | `"absolute"` writes the wrapper's full path into each agent config. `"bare"` writes the name `tma-hook` and lets the agent resolve it off `$PATH`. |

The default is machine-specific by construction: `/Users/you/.local/bin/tma-hook`
on macOS is `/home/you/.local/bin/tma-hook` on Linux, so a `~/.claude/settings.json`
synced between the two carries a path that only works on one. `"bare"` writes one
string that works on both.

A `$HOME`-relative string is not offered, because it would only work for half the
agents. Three of the six wiring mechanisms spawn the wrapper as argv with no shell
involved (Codex's `notify` array, the OpenCode plugin's and pi's `spawn`), and
those would pass `$HOME/.local/bin/tma-hook` through as a literal filename. A bare
name is resolved by all six, since `execvp` searches `$PATH` exactly like a shell
does.

The cost of `"bare"` is that the wrapper's directory has to be on the `$PATH` each
agent inherits, which for a GUI-launched editor is often narrower than your
shell's. `tma install-hooks` refuses to wire anything when `tma-hook` is not
findable, and `tma doctor` reports the reference and whether `$PATH` still answers
it:

```
wrapper: tma-hook ✓ on $PATH (/home/you/.local/bin/tma-hook)
```

Set the key before installing, or pass `--wrapper-ref bare` for one run. The two
postures write different strings, so after switching, run `tma install-hooks
--all` to repoint every agent already wired: until you do, `--check` reports the
old wiring as stale.

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
| `demote_edges` | `5` | Hook-liveness demotion threshold. |
| `autostart` | `false` | Auto-start the daemon on first use of a surface (`ls`/`status`/`jump`/picker/`watch`/`wait`/`subscribe`). |

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
