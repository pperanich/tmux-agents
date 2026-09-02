# tmux-agents

[![CI](https://github.com/pperanich/tmux-agents/actions/workflows/ci.yml/badge.svg)](https://github.com/pperanich/tmux-agents/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/pperanich/tmux-agents)](https://github.com/pperanich/tmux-agents/releases/latest)

`tma` is an agent state monitor for tmux. It detects coding agents running in
your tmux panes, shows at a glance which are blocked, working, or idle, and
jumps you to the one that needs you. State lives in tmux's own pane options, so
any `tmux show-options` or `#{@agent_state}` format string reads it directly and
`tma ls --json` gives a stable structured feed. There is no socket and no
required background process. [Why tma](docs/explanation/why-tma.md) is the short
case for that design.

## Supported agents

Six agents ship with detection out of the box. `tma` reads state from three
kinds of evidence: hook events the agent fires, its on-screen chrome, and the
process in the pane. The table below is the shipped coverage; the full,
evidence-backed record is in
[docs/reference/agent-coverage.md](docs/reference/agent-coverage.md).

| agent | hook-covered states | blocked signal | screen rules | identity |
|---|---|---|---|---|
| Claude Code | working / idle / blocked / lifecycle | hook + screen | working, idle, blocked | process `claude` |
| Codex CLI | working / idle / blocked / lifecycle | hook + screen | working, blocked | process `codex` |
| OpenCode | working / idle / blocked (registers on start; no end hook) | hook + screen | blocked | process `opencode` |
| Gemini CLI | working / idle / blocked / lifecycle | hook + screen | working, blocked | process `node`, narrowed by title |
| Cursor CLI | working / idle / lifecycle | screen only | working, blocked | process `node`/`agent`, title `Cursor Agent` |
| pi | working / idle / lifecycle | none (pi auto-approves tools) | working | process `node`/`pi`, title `π …` |

`blocked` is hook-covered for four agents and rides a screen rule for Cursor
(which exposes no permission hook). pi has no blocked state at all: it runs
tools without a permission prompt, so there is nothing to detect. Agents that
run under a generic process name (`node`) are disambiguated by their pane title.

Adding an agent `tma` does not ship is one TOML manifest, no code: identity,
screen rules, and a hook map in a file dropped in `~/.config/tma/agents/`. See
[add a custom agent](docs/how-to/add-a-custom-agent.md) and the
[manifest schema](docs/reference/manifest-schema.md).

## Quickstart

You need tmux 3.6 or newer (that is what `tma` is developed and tested against;
`tma doctor` warns on anything older).

Install the binary. From the first public release onward, one command does it:

```
curl -fsSL https://raw.githubusercontent.com/pperanich/tmux-agents/main/scripts/install.sh | sh
```

That fetches the prebuilt binary for your platform (macOS on Apple Silicon or
Intel, Linux on x86_64 or aarch64), checks it against the release's
`SHA256SUMS`, and installs it to `~/.local/bin`. Pass `TMA_VERSION` to pin a tag
and `TMA_INSTALL_DIR` to install somewhere else. With a Rust toolchain you can
build from a checkout instead:

```
cargo install --path crates/tma
```

The Nix flake and the Home Manager module are the other two supported paths; see
[install tma](docs/how-to/install-tma.md).

Then let the setup wizard do the wiring: it finds the agents you actually have
installed, wires each one's hooks, installs the keybindings, prints the
status-line entry to add, and finishes with a `tma doctor` report. Every write
shows you a diff first.

```
tma init
```

Prefer to do it a piece at a time? Wire one agent so its state is reported the
instant it changes (Claude Code here):

```
tma install-hooks claude
```

Either way, run the picker to see every agent pane and jump to one:

```
tma
```

For ambient state in your status line, add the driver to your tmux config
(`~/.tmux.conf` or `~/.config/tmux/tmux.conf`):

```tmux
set -g status-right '#(tma status) %H:%M'
```

`#(tma status)` is not just cosmetic: without a daemon it is also what keeps
pane state fresh. The [getting-started tutorial](docs/tutorial/getting-started.md)
walks the whole loop end to end, and [status line and
keybindings](docs/how-to/install-the-keybindings.md) covers the picker, the
temporary watch session, and the jump bindings.

## Configuration

`tma` reads an optional `config.toml` (`--config <path>`, then `TMA_CONFIG`,
then `$XDG_CONFIG_HOME/tma/config.toml`, then `~/.config/tma/config.toml`). Every setting has a working default, so
zero-config works; unknown keys are a loud parse error rather than a silent
typo. A minimal example:

```toml
[status]                     # `tma status` glyphs + colors
blocked = { glyph = "⚑", color = "red" }

[notify]                     # notifications
on = ["blocked", "done"]     # fire on blocked, and on working->idle completions
```

The full key reference is in
[docs/reference/configuration.md](docs/reference/configuration.md).

## Documentation

The docs are a [Diátaxis](https://diataxis.fr/) tree, also buildable as an
mdBook site (`mdbook build` from the repo root renders `docs/` into `book/`):

- **Tutorial**: [getting started](docs/tutorial/getting-started.md) takes you
  from an empty tmux to a working monitor.
- **How-to guides**: [install agent
  hooks](docs/how-to/install-agent-hooks.md), [add a custom
  agent](docs/how-to/add-a-custom-agent.md),
  [notifications](docs/how-to/notifications.md), [block a script on agent
  state](docs/how-to/block-a-script-on-agent-state.md), [run the
  daemon](docs/how-to/run-the-daemon.md), [show agents in your status
  line](docs/how-to/show-agents-in-your-status-line.md), [install the
  keybindings](docs/how-to/install-the-keybindings.md), [diagnose with
  `tma doctor`](docs/how-to/diagnose-with-doctor.md).
- **Reference**: [command-line interface](docs/reference/cli.md),
  [keybindings](docs/reference/keybindings.md),
  [configuration](docs/reference/configuration.md), [pane options and JSON
  contracts](docs/reference/pane-options-and-json.md), [agent
  coverage](docs/reference/agent-coverage.md), [manifest
  schema](docs/reference/manifest-schema.md).
- **Explanation**: [why tma](docs/explanation/why-tma.md),
  [architecture](docs/explanation/architecture.md), [the detection
  model](docs/explanation/detection-model.md), [the security
  model](docs/explanation/security-model.md).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) has the toolchain, the `mise` tasks, the
crate layout, and the architecture invariants a change has to hold.
[CHANGELOG.md](CHANGELOG.md) records what each release changed, breaking
changes first.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this work
shall be dual licensed as above, without any additional terms or conditions.
