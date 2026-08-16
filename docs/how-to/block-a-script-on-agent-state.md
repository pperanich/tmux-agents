# Block a script on agent state

Block a script until an agent reaches a state, then act on the result. `tma wait`
is the scripting primitive: it waits on a target pane, prints its final row, and
exits with a code you can branch on. This guide covers the common recipes and the
exit-code contract in practice.

`wait` is level-triggered: if the target is already in a requested state, it
returns immediately rather than waiting for a fresh transition. For every flag see
[`tma wait`](../reference/cli.md#tma-wait); the exit-code table is
[there too](../reference/cli.md#exit-codes).

## Block until an agent finishes

Wait for a specific pane to go idle, then run the next step:

```sh
tma wait --pane %5 --until idle && ./deploy.sh
```

`--until` takes a comma-separated set, so you can wake on any of several states.
Wait for the agent to either finish or get stuck:

```sh
tma wait --pane %5 --until idle,blocked
```

When the state is reached, `wait` prints the matched row (same columns as
`tma ls`) and exits 0:

```
$ tma wait --pane %1 --until blocked
%1	claude	blocked	permission	1786900866503	s2:0.0	web-ui	1	
```

Use `--json` for a structured result (one schema-1 object, same keys as an
`ls --json` row):

```
$ tma wait --pane %0 --until working --json
{"schema":1,"pane":"%0","agent":"claude","state":"working","detail":null,"since":1786900866412,"since_ms":1786900866412,"locator":"s1:0.0","title":"api-server","attention":false,"done":false,"session":"3f1c8a20-5b6d-4e77-9c11-8a2e4d0b6f93","context":31,"context_at_ms":1786900866438,"muted":false,"tokens":62400,"repo":"tmux-agents","branch":"main","worktree":false,"server":"/private/tmp/tmux-501/default","host":"devbox"}
```

## Target an agent by name

`--agent` waits on the pane running that agent. It pins to the first pane it
observes and then behaves as `--pane` on that id, so a second same-named agent
appearing mid-wait never flips the wait. If more than one pane matches at that
first observation, it is an error that tells you to target one explicitly, rather
than silently picking one:

```
$ tma wait --agent claude --until idle
tma: --agent "claude" matches 2 panes (%1, %0); target one with --pane
```

Narrow the match with the [selector flags](../reference/cli.md#selector-flags)
when you have the same agent in several places: `--session <name>`, `--repo`,
`--branch`, or `--state`. They scope the first observation (the one that pins),
not the pinned pane afterwards. `--any` waits on any agent pane in scope and
never pins, so it keeps waiting if one vanishes:

```sh
tma wait --any --repo tmux-agents --until done --timeout 900
```

## Wait on a fleet

`--all` is a barrier: it returns only when every agent pane in scope is in a
target state, and prints all of their rows. Use it to join a fan-out before a
merge step:

```sh
tma wait --all --repo tmux-agents --until idle,done --timeout 1800 && ./collect.sh
```

Membership is pinned at the first observation, so an agent someone launches
halfway through does not extend the barrier — the fleet is the one you started
over. A member whose pane dies ends the wait at exit 3 rather than quietly
shrinking the barrier to the survivors. An `--all` whose scope matches no pane at
all is exit 2 (there is nothing to wait for), never a vacuous success.

`--count <n>` is the looser form, a quorum: it returns once n panes in scope are
in a target state, re-reading the scope every cycle, so panes may come and go
under it. Use it to start work as soon as enough agents are free:

```sh
tma wait --count 2 --agent claude --until done --timeout 600
```

Both print one `tma ls` line per satisfied pane, and both take `--json`, where
they emit the same schema-1 `agents` document `tma ls --json` does (rather than
the single row object the one-pane targets emit).

## Drive a supervisor loop

`wait` is level-triggered, which is what makes it safe to call at any moment —
and what makes a naive loop spin. If you wait for `blocked`, act, and loop, the
second `wait` returns instantly: the pane is still blocked (or just became idle
in a way that satisfies you again) from the episode you already handled.
`--since` fixes that by requiring the state to have BEGUN after a timestamp you
carry forward:

```sh
#!/bin/sh
# Feed one agent a queue of tasks, one per idle episode.
set -eu
pane=%5
since=0
while read -r task; do
  row=$(tma wait --pane "$pane" --until idle --since "$since" --json --timeout 900) || exit $?
  since=$(printf '%s' "$row" | sed 's/.*"since_ms":\([0-9]*\).*/\1/')
  tma act queue-next --pane "$pane" --arg "$task" --yes
done < tasks.txt
```

Each pass blocks until the pane enters an idle episode strictly newer than the
one it just serviced, so the loop advances exactly once per episode. The floor is
exclusive (`since_ms > --since`), which is why feeding back the row's own
`since_ms` is correct. `--since` composes with every target, including `--all`
and `--count`. The `queue-next` action it fires is written in
[Author a custom action](custom-actions.md#pass-a-value-in---arg).

## Gate CI on agent state

In a headless run, launch the agent, then block a build step on its completion
with a timeout so a hung agent fails the job instead of hanging it. `--timeout`
follows the `timeout(1)` convention and exits 124 on expiry:

```sh
#!/bin/sh
set -e
# ... launch the agent in a tmux pane %agent ...
if tma wait --pane "%agent" --until idle --timeout 600; then
  echo "agent finished"
else
  code=$?
  [ "$code" = 124 ] && echo "timed out" && exit 1
  [ "$code" = 3 ]   && echo "pane vanished" && exit 1
  [ "$code" = 4 ]   && echo "agent died" && exit 1
  exit "$code"
fi
```

You can also compose with `timeout(1)` itself as an external belt when you want a
hard wall-clock ceiling regardless of `wait`'s own logic:

```sh
timeout 600 tma wait --pane %5 --until idle
```

## Exit codes in practice

Each code below was produced by a real `wait` invocation. Branch on them as in the
CI recipe above.

```
$ tma wait --pane %1 --until idle --timeout 2      # never reaches idle
tma: timed out after 2s waiting for idle (exit 124)

$ tma wait --pane %5 --until idle                  # pane killed mid-wait
tma: the waited-on pane %5 vanished before reaching idle (exit 3)

$ tma wait --pane %1 --until bogus                 # bad state token
error: invalid value 'bogus' for '--until <STATES>': unknown --until state "bogus" (expected one of: idle, working, blocked, unknown, done)
```

For the full code list and its semantics, see
[the exit-code table](../reference/cli.md#exit-codes).

## Waiting before an agent exists

A `--pane` target that is not yet an agent does not fail fast: `wait` blocks by
design (the agent may launch later), printing a one-time hint to stderr if the
pane looks like a typo.

## When the agent crashes

Once the wait has seen the pane carrying an agent, a missing agent row means the
opposite: the process died and the pane is still sitting there. That ends the
wait at exit 4 rather than blocking until `--timeout`, so a supervisor can
restart the agent instead of waiting out a ceiling meant for slow work:

```
$ tma wait --pane %5 --until idle --timeout 900
tma: the agent on pane %5 exited before reaching idle (exit 4)
```

Branch on it next to 124: `4` means "restart it", `124` means "it is still
running and taking too long". `--any` and `--count` ignore a departure (they are
waiting on whoever else is left); `--all` ends on a member's agent death the same
way it ends on a member's pane vanishing, and forwards that member's own verdict
rather than flattening the two. So a barrier exits 4 when a member's agent died
and 3 when a member's pane went away, which is the distinction the branch above
depends on.

## Watching everything instead

`tma wait` blocks for one thing. For a running record of every transition, see
[Stream state changes](stream-state-changes.md).
