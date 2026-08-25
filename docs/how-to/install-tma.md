# Install tma

Four ways to get the binary: the install script, `cargo install` from a clone,
the Nix flake, or the Home Manager module. All four land the same single `tma`
executable, so pick the one that matches how you manage the rest of the machine.

You need tmux 3.6 or newer. That is the release `tma` is developed and tested
against; older servers load configs in a different order and expand
`display-popup` differently, so the keybindings and the picker can misbehave.
`tma doctor` warns when the server it is talking to is older, and keeps working.

Installing the binary is the whole install. Hooks (`tma install-hooks <agent>`)
and keybindings (`tma install-keys`) are separate commands you run afterward,
because both edit files you own and both show you the diff first.
[`tma init`](../reference/cli.md#tma-init) runs that whole sequence for the
agents it finds on your `PATH`, if you would rather do it in one command.

## With the install script

The shortest path, and the one that needs no toolchain. It works from the first
public release onward, because it downloads a release artifact:

```
$ curl -fsSL https://raw.githubusercontent.com/pperanich/tmux-agents/main/scripts/install.sh | sh
```

The script picks the build for your platform, verifies the tarball against the
release's `SHA256SUMS`, installs the binary to `~/.local/bin`, and prints a
`PATH` hint if that directory is not on yours. It never uses `sudo`. To upgrade,
run it again: the new binary replaces the old one in place.

Prebuilt binaries cover macOS on Apple Silicon and Intel, and Linux on x86_64
and aarch64. The Linux builds are static musl binaries, so your distribution's
glibc does not come into it. On any other platform the script stops and points
you at the cargo path below.

It also installs shell completions, for whichever of bash, zsh, and fish it
finds on your machine, into that shell's per-user directory. zsh needs one more
line in your `~/.zshrc` for its directory to be searched at all, which the
script prints when it applies.

Three environment variables change what it does:

| variable | effect |
|---|---|
| `TMA_VERSION` | Install this tag instead of the latest release. |
| `TMA_INSTALL_DIR` | Install here instead of `~/.local/bin`. Created if it does not exist. |
| `TMA_NO_COMPLETIONS` | Set to anything to install only the binary. |

They belong on the `sh`, not on the `curl`, or the pipeline hands them to the
wrong process:

```
$ curl -fsSL https://raw.githubusercontent.com/pperanich/tmux-agents/main/scripts/install.sh \
    | TMA_VERSION=v0.5.2 TMA_INSTALL_DIR=~/bin sh
```

If piping a script into a shell is not your habit, [read it
first](https://github.com/pperanich/tmux-agents/blob/main/scripts/install.sh)
and run it from disk.

## From a clone, with cargo

You need `git` and a Rust toolchain (1.88 or newer):

```
$ git clone https://github.com/pperanich/tmux-agents
$ cd tmux-agents
$ cargo install --path crates/tma
```

The workspace builds one binary. Check it landed on your `PATH`:

```
$ tma --version
tma 0.5.2
```

To upgrade, `git pull` and run the same `cargo install` again; it replaces the
binary in place.

`cargo install` places a binary and nothing else, so wire the completions
yourself — `tma completions <shell>` writes the script for one shell to stdout,
and [`tma completions`](../reference/cli.md#tma-completions) says where each
shell wants it. The install script and the Nix package both do this for you.

## From the Nix flake

The flake builds for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, and
`aarch64-darwin`, and exposes:

| output | what it is |
|---|---|
| `packages.<system>.tma`, `packages.<system>.default` | the `tma` package, versioned from the workspace `Cargo.toml` |
| `overlays.default` | adds `tma` to a nixpkgs instance |
| `homeModules.default`, `homeModules.tma` | the Home Manager module below (`homeManagerModules` is an alias for older setups) |
| `devShells.default` | the contributor shell: the package's build inputs plus `clippy`, `rustfmt`, `rust-analyzer`, `tmux`, `mise`, and `mdbook` |
| `checks.<system>.tma` | the package build, whose test suite spawns its own scratch tmux server |
| `formatter.<system>` | `nixfmt-rfc-style`, so `nix fmt` formats the Nix files |

Try it without installing anything, or install it for real:

```
$ nix run github:pperanich/tmux-agents -- --version
$ nix profile install github:pperanich/tmux-agents
```

There is no `apps` output; `nix run` resolves `packages.default`, whose
`meta.mainProgram` is `tma`.

To pull the package into your own flake, use the overlay:

```nix
{
  inputs.tma.url = "github:pperanich/tmux-agents";

  # wherever you build your nixpkgs instance:
  nixpkgs.overlays = [ inputs.tma.overlays.default ];  # then: pkgs.tma
}
```

### The hook wrapper

`tma install-hooks` writes a small `tma-hook` wrapper next to the `tma` binary and
points the agent's config at that path. That directory is the read-only store
here, so the Nix package installs the wrapper itself: `install-hooks` finds its
own script already in place and writes nothing. Nothing to set, and `tma-hook`
comes onto your `PATH` with the binary.

What lands in the agent config is a path outside the store: your profile's
`~/.nix-profile/bin/tma-hook`, or `/etc/profiles/per-user/<user>/bin/tma-hook`
under Home Manager. tma will not write the store path itself, because that path
names one build and is deleted when the build is collected, which would break
your hooks at the next `nix flake update` rather than at anything you did. It
finds the profile entry by walking `$PATH` for a `tma-hook` outside any store
that resolves to the same file, so the substitution only happens when the two are
provably the same install. `tma doctor` shows both, reference first:

```
wrapper: /etc/profiles/per-user/you/bin/tma-hook ✓ (/nix/store/<hash>-tma-<version>/bin/tma-hook)
```

Running straight from the store with no profile install (`nix run`, a `nix build`
result) has no such stable path to find, so tma wires the store path and warns
that it will not survive collection.

Two settings change this if you want them. `--wrapper-path <PATH>` (env
`TMA_WRAPPER_PATH`) keeps the wrapper out of the store entirely by naming where
it is written; `tma install-hooks --check` and `tma doctor` resolve it the same
way install did, so export the variable rather than passing the flag once. And
`[install] wrapper_ref = "bare"` writes the name `tma-hook` with no path at all,
which is worth it when one agent config is shared between machines — though it
then depends on the `$PATH` each agent inherits, where an absolute reference does
not.

## With the Home Manager module

```nix
{
  imports = [ inputs.tma.homeModules.default ];

  programs.tma = {
    enable = true;
    settings.notify.on = [ "blocked" "done" ];
    keybindings.enable = true;
  };
}
```

The module's options:

| option | type | default | effect |
|---|---|---|---|
| `programs.tma.enable` | bool | `false` | Installs the package and writes the files the options below ask for. |
| `programs.tma.package` | package | `pkgs.tma` when the overlay is applied, otherwise built from the module's own source tree | The package to install. |
| `programs.tma.settings` | TOML attrset | `{}` | Written to `$XDG_CONFIG_HOME/tma/config.toml`. An empty attrset writes no file at all, leaving tma on its built-in defaults. Keys are the ones in [configuration](../reference/configuration.md), and an unknown one is a parse error at runtime, not at build time. |
| `programs.tma.agents` | attrset of TOML attrsets | `{}` | Each entry becomes `$XDG_CONFIG_HOME/tma/agents/<name>.toml`. A new stem adds an agent; a stem matching a bundled manifest (`claude`, `codex`, `cursor`, `gemini`, `opencode`, `pi`) replaces it wholesale, so that entry has to be a complete [manifest](../reference/manifest-schema.md). |
| `programs.tma.keybindings.enable` | bool | `false` | Appends tma's tmux bindings to `programs.tmux.extraConfig`. An assertion fails the build unless `programs.tmux.enable` is also on. |

`keybindings.enable` is the declarative alternative to running `tma install-keys`.
Use one or the other: both write the same keys, and running both defines each
binding twice. The module writes the whole prefix set and nothing else; the
opt-in mouse bindings still need `tma install-keys --mouse`. A test in the binary
reads the module and fails if its block drifts from the bindings `install-keys`
writes, so the two cannot silently disagree. See
[Keybindings](../reference/keybindings.md) for what each key does.

The module installs the binary and writes config. It does not touch your agents'
own config files, so wiring an agent is still
[`tma install-hooks`](install-agent-hooks.md), which needs nothing set.

## Next

- [Getting started](../tutorial/getting-started.md) walks the whole loop from an
  empty tmux.
- [Install agent hooks](install-agent-hooks.md) covers the per-agent wiring.
- [Show agents in your status line](show-agents-in-your-status-line.md) covers
  the ambient driver, and [Install the
  keybindings](install-the-keybindings.md) covers the key set.
