# Run an agent in a container

tmux on the host, the agent in a devcontainer. tma runs on the host and knows
nothing about containers, yet the pane still shows state, because the agent
reports it rather than being inspected.

## The plumbing

**1. Put `tma` and a `tmux` client in the image.** `tma` shells out to `tmux` for
every read and write, so the binary has to be there too. Match the host's tmux
major version: tmux refuses a client whose protocol version differs from the
server's, and the failure is total, not degraded.

```dockerfile
RUN apt-get update && apt-get install -y tmux \
 && curl -fsSL <tma release url> -o /usr/local/bin/tma \
 && chmod +x /usr/local/bin/tma
```

**2. Bind-mount the tmux socket when the container is created.** Mounts are a
create-time thing, so this goes in your `docker run` or `devcontainer.json`, not
in the `exec`. Ask tmux for the path rather than guessing it: it varies by
platform and with `$TMUX_TMPDIR`.

```sh
sock=$(tmux display -p '#{socket_path}')

docker run -d --name dev \
  --user "$(id -u):$(id -g)" \
  -v "$sock:/tmp/tmux.sock" \
  myimage sleep infinity
```

In a devcontainer, export the path first (`export TMUX_SOCK=$(tmux display -p
'#{socket_path}')`) and mount it from the environment:

```json
"mounts": ["source=${localEnv:TMUX_SOCK},target=/tmp/tmux.sock,type=bind"]
```

**3. Hand over the socket and the pane at exec time**, as environment:

```sh
docker exec -it \
  -e TMA_SOCKET_PATH=/tmp/tmux.sock \
  -e TMUX_PANE="$TMUX_PANE" \
  dev claude
```

Three things are load-bearing here:

- `TMA_SOCKET_PATH` is how tma inside the container finds the server. Passing
  `TMUX` through instead also works, but only if the socket is mounted at exactly
  its host path: the variable carries an absolute path that must resolve in the
  container too. The explicit variable is the version that does not care where you
  mount it.
- `TMUX_PANE` is the pane whose options get stamped. Every `tma event` in the
  container writes to that id and nothing else, so pass the pane the agent is
  actually running in.
- `--user "$(id -u):$(id -g)"` on the container matters because the socket lives
  in a directory tmux creates `0700` and owns. Bind-mounting it does not change
  who owns it, so the container process needs your uid to open it.

**4. Install the hooks inside the container**, where the agent reads its config:

```sh
docker exec dev tma install-hooks claude
```

The hooks reference the `tma-hook` wrapper, which resolves `tma` at fire time and
exits silently if it is missing, so a container without tma is quiet, not broken.

Run `tma install-hooks` on the host as well if you want the attention-clear tmux
server hooks: those are set on the tmux server, and a container invocation
installs them only if you also give it the socket env from steps 2 and 3.

## Check the wiring

The pane is invisible until the first hook fires. With step 4 skipped nothing
ever registers and the pane has no row at all; once a hook has fired, `tma debug
explain` shows the arrangement:

```
$ tma debug explain %5
pane      %5  (dev:0.0)
command   docker
agent     claude (pid 0, foreground_is_agent=false, registered behind remote shell docker)
boundary  remote shell docker — the cycle holds this pane's stamps and captures nothing; hook events are its only evidence
prior     working / - src=hook evidence_at=… since=…
```

`tma doctor` shows the same pane at tier 2, or tier 3 with a daemon, with its
hooks wired.

## What you get, and what you do not

You get everything the hook tier carries: state and transitions, `@agent_since`,
attention, notifications, `tma wait`, the picker, the sidebar, and actions that
send keys (tmux delivers those to the pane's tty, which the container process is
reading, so the boundary does not matter). Context telemetry rides the same
route: the statusline shim runs inside the container and pushes to `tma event
--agent <agent> --kind context --pane "$TMUX_PANE" --payload -` over the same
socket, so the gauge crosses the boundary with the events.

You do not get the tiers that inspect the process: no screen-rule fallback when a
hook is missed, and no process-walk identity. Practically that means a hook the
agent never fires stays unreported, where a host-run agent would have had its
state read off the screen. Doctor's report looks clean, because nothing *is*
misconfigured; the missing capability is a property of the arrangement.

## Why it works at all

tma's other tiers cannot cross a container boundary. The process walk reads the
host's `ps`, where the agent's process does not exist; the screen fold needs the
pane's foreground command to *be* the agent, and it is the container client.

The hook tier does not inspect anything. `tma event` is a stateless one-shot: it
takes a pane id, maps one event through a manifest, writes tmux pane options, and
exits. It keeps no memory between runs, holds no connection, and needs no daemon,
because all the state is in the options on the host's tmux server. So an agent
inside a container can stamp a host pane directly, provided it can reach the
socket and knows the pane id. Both are things you hand it when you start it.

The one part that is not obvious is why the pane stays in scope. A pane whose
foreground command is `docker`, `podman`, `kubectl`, `ssh`, or `mosh` is
classified as a remote shell and taken out of scope, precisely because the process
walk on such a pane comes back empty while the screen sits there inviting a false
match. Registration is the exception: `@agent_session` on the pane names the
agent that owns it, and a hook could only have set that from inside. The
registration outranks the classification, so the pane keeps its stamps and its
row, and the poll cycle stops capturing it, since nothing readable crosses the
boundary anyway.

The container client in the pane's process tree is what keeps the stamp alive
afterwards. A pid-less registration whose pane holds nothing but a shell is
reaped after 30 seconds, which is how a container that exits leaves a clean pane
instead of a frozen row.
