# tmux-agents

`tma` is an agent state monitor for tmux. It detects coding agents running in
tmux panes, shows which are blocked, working, or idle, and jumps you to the one
that needs you. State lives in tmux pane options, so any `tmux show-options` or
`#{@agent_state}` format string reads it directly and `tma ls --json` gives a
stable structured feed.

If you are sizing `tma` up rather than using it yet, read [Why
tma](explanation/why-tma.md) first: the problem, the two other shapes this tool
could have taken, and the one choice everything else follows from.

This site is a [Diátaxis](https://diataxis.fr/) tree, organized in four parts:

- **Tutorial** takes you from an empty tmux to a working monitor:
  [getting started](tutorial/getting-started.md).
- **How-to guides** are task-oriented recipes: installing `tma` itself,
  installing hooks, adding a custom agent, running an agent in a container,
  running tma over ssh,
  notifications, authoring a custom action, blocking a script on agent state,
  streaming state changes, running the daemon, showing agents in your status
  line, driving an external bar, installing the keybindings, diagnosing with
  `tma doctor`, and reading agent state from a status bar or script.
- **Reference** documents the contracts precisely: the command-line interface,
  the keybindings, the `config.toml` keys, the pane options and JSON schemas,
  per-agent hook coverage, and the agent and action manifest schemas.
- **Explanation** covers the why: [why tma](explanation/why-tma.md), the
  [architecture](explanation/architecture.md), the [detection
  model](explanation/detection-model.md), and the [security
  model](explanation/security-model.md).

The development record (the daemon and architecture decision notes, the numbered
requirements) is not part of this site. It lives in the repository under
[`docs/internal/`](https://github.com/pperanich/tmux-agents/tree/main/docs/internal)
for contributors, as design history rather than user documentation.

In a hurry? `tma init` is the fast path: it detects the agents you have
installed, wires their hooks, installs the keybindings, prints the status-line
entry, and ends with a `tma doctor` report ([reference](reference/cli.md#tma-init)).

New here? Start with the [getting-started tutorial](tutorial/getting-started.md),
which walks the same ground one command at a time.
Looking for a specific contract? Jump to the
[command-line interface](reference/cli.md) or
[configuration](reference/configuration.md).
