# Add a custom agent

Teach `tma` an agent it does not ship a mapping for. This is a data-only
extension: you write one TOML manifest, drop it in `~/.config/tma/agents/`, and
wire your agent's hook to `tma`. No code change, no rebuild.

A manifest is the complete description of an agent: how to recognize its pane, how
its hook events map to states, and (optionally) how to read its screen. This guide
builds a minimal hook-only manifest end to end. For the full field reference, see
[Manifest schema](../reference/manifest-schema.md).

## 1. Write the manifest

Create `~/.config/tma/agents/myagent.toml`. The floor is an `[identity]` block, a
`[hooks]` block mapping your agent's event names to state claims, and a
`[capture]` block (present, may be empty):

```toml
min_engine_version = "0.1"

[identity]
process_names = ["myagent"]

[hooks]
covers = ["working", "idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[[hooks.map]]
event = "Run"
claim = { state = "working" }

[[hooks.map]]
event = "Wait"
claim = { state = "blocked", detail = "permission" }

[[hooks.map]]
event = "Done"
claim = { state = "idle" }
turn_end = true

[capture]
```

- `[identity].process_names` lists the `#{pane_current_command}` basenames that
  flag a candidate pane. If your agent runs under a generic launcher (many run as
  `node`), add `title_patterns` to narrow the match by pane title, or rely on the
  hook registration below, which marks the pane regardless of process name.
- `[hooks].covers` declares which states your hooks report. The engine uses it to
  know what a screen-capture fallback would still need to watch for.
- Each `[[hooks.map]]` maps one event to a claim: a state claim
  (`{ state = "working" }`, optionally with a `detail`) or a lifecycle claim
  (`{ lifecycle = "start" }` / `{ lifecycle = "end" }`). State routing is fixed:
  a manifest maps into `idle`/`working`/`blocked`/`unknown`, it cannot invent a
  state.
- Mark your agent's turn-end event `turn_end = true`, and only that one. It is
  what raises the done mark on a completion tma had no other way to see — a turn
  that ends without the pane ever having been observed working draws no state edge
  at all. An event that merely reports the agent is idle (a nag notification) must
  leave it off, or the mark would come back every time it fired.
- `[capture]` is required even when empty; with no `[[rules]]`, detection is
  hook-only.

The file stem is the agent name. A stem that matches a bundled agent (`claude`)
shadows it; a new stem adds a new agent.

## 2. Wire your agent's hook to `tma`

Point your agent's hook at the `tma-hook` wrapper, passing the agent name and the
event name, with the hook payload on stdin:

```
tma-hook myagent Boot     # on session start
tma-hook myagent Run      # when a turn starts
tma-hook myagent Wait     # when it needs approval
```

`tma-hook <agent> <event>` is the stable contract. The wrapper forwards the stdin
payload to the internal `tma event --agent <agent> --kind <event>` and resolves
the binary at fire time. If your agent delivers the payload as a trailing argument
instead of on stdin (some notify-style programs do), pass it as a third argument;
the wrapper feeds it to `tma` on stdin either way. Install the wrapper with any
`tma install-hooks <bundled-agent>`, or point at it directly.

The event name in the hook must match the `event` field in your manifest. `tma`
resolves the agent against the loaded manifest set and applies that manifest's
map; an event with no matching entry, or an agent with no manifest, is a clean
no-op (exit 0, nothing stamped).

## 3. Verify

Start your agent so its `Boot`-equivalent hook fires and registers the pane, drive
a turn, then list agents. The states come straight from your manifest's map:

```
$ tma ls
%5	myagent	blocked	permission	1785114189508	ma2:0.0	myproj	1
%4	myagent	working		1785114163134	ma:0.0	myproj	
```

To see the raw stamp your hook wrote, read the pane's options directly:

```
$ tmux show-options -p -t %5 | grep '^@agent_'
@agent_attention 1
@agent_detail permission
@agent_evidence_at 1785114189508
@agent_name myagent
@agent_pid 0
@agent_session fe9a1234-0000-4000-8000-0000000000bb
@agent_since 1785114189508
@agent_source hook
@agent_stamped_at 1785114189508
@agent_state blocked
```

`@agent_source hook` and the recorded `@agent_session` confirm the hook path
registered and mapped the pane. For the full trace, `tma debug explain <pane>`
prints the identity result, every matched and failed rule, and the final verdict;
`tma doctor` reports the pane's effective tier and whether its hooks are wired.

If your manifest never seems to load, run `tma doctor`: a file that fails to parse
is skipped rather than failing the whole set, and doctor's `agents:` line names it
with the error.

```
$ tma doctor
agents:  6 loaded, 1 skipped:
  - ~/.config/tma/agents/myagent.toml: TOML parse error at line 4, column 1
```

## Testing without a live agent

To exercise a manifest in isolation before wiring the real agent, load only your
manifest directory and fire an event by hand with `$TMUX_PANE` set to a target
pane:

```
tma --manifest-dir ~/.config/tma/agents event --agent myagent --kind Boot --payload -
```

`--manifest-dir` loads exactly that directory as a closed set, so a typo in the
manifest surfaces immediately rather than being masked by the bundled corpus. This
is how `tma`'s own custom-agent integration test drives a brand-new agent name end
to end.
