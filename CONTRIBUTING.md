# Contributing

## Toolchain

Rust edition 2021, MSRV **1.88** (pinned in `[workspace.package]` and checked by
CI on that exact toolchain, so a dependency bump that raises the real floor fails
there rather than in someone's install). `tmux` must be on `PATH` for the
integration tests.

```
cargo build
```

## The three commands

The tasks live in `mise.toml`, which is the source of truth; these are the ones
CI runs.

| command | what it does |
|---|---|
| `mise run test` | `cargo test --workspace --all-features --no-fail-fast`. `--all-features` matters: the manifest fixture suites sit behind `tma-core`'s default-off `fixtures` feature and never run without it. |
| `mise run lint` | clippy with warnings denied, `cargo fmt --check`, and a `cargo doc` build so a dead rustdoc link fails here. |
| `mise run fmt` | `cargo fmt --all`. |

One trap if you narrow that to a single package. The integration suites outside the
`tma` package spawn the built `tma` binary rather than linking it, and `cargo test -p
tma-daemon` rebuilds the test binary but not `tma`: run it after editing `tma-runtime`
and you are testing the previous build. The harness compares the binary's mtime against
every source under `crates/` and panics on a stale one, so this reports itself instead of
passing green. Run `cargo build --workspace` first, or just use the whole-workspace
command above. `TMA_TEST_BIN_NO_STALE_CHECK` skips the scan.

## Where things live

Eight crates under `crates/`, stacked so the dependency graph enforces the
boundaries below.

| crate | one line |
|---|---|
| `tma-core` | The pure detection library: snapshot and evidence types, manifest schema, identity, the verdict fold. |
| `tma-tmux` | The only crate that spawns `tmux`: read path, control-mode pool, guarded option writes. |
| `tma-runtime` | Tier 2: config, manifest loading, the poll cycle, capture, `tma event`, the wire protocol, and the `ui` helper surface. |
| `tma-daemon` | Tier 3 only: the serve loop and notification dispatch. |
| `tma-ui-core` | the pure Elm-style folds behind the picker and `tma watch`. No terminal, no tmux. |
| `tma-ui` | The display layer: the shell loop, drawing, jump, and the `ls`/`status` surfaces. |
| `tma` | The binary: clap dispatch, hook installation, doctor. |
| `tma-test-support` | The shared integration-test harness (scratch tmux socket, daemon lock gate). Dev-dependency only. |

Agent manifests are `crates/tma-core/manifests/<agent>.toml`, one file per agent,
compiled in and shadowed at runtime by `~/.config/tma/agents/<agent>.toml`. Action
manifests are `crates/tma-core/actions/`. Captured-screen fixtures are
`crates/tma-core/fixtures/`, with the format documented in the README beside them.

Adding an agent is a manifest and a fixture, not code. Every screen rule must be
authored from a real captured screen (`tma debug capture`), redacted (`tma debug
redact`), and committed as the fixture that proves it fires. Never match
incidental pane text.

## Invariants a change has to hold

The architecture decisions are in
[`docs/internal/ARCHITECTURE.md`](docs/internal/ARCHITECTURE.md); the four rules a
review will check are these.

- **`tma-core` stays pure.** Snapshot and evidence in, verdict out. No I/O, no
  process spawning, and no clock read inside the fold. Every timestamp is
  injected by a caller.
- **tmux only through `tma-tmux`.** Nothing above it constructs a tmux command
  line. This matters most for writes: concurrent producers stamp the same
  options and tmux has no transactions, so the conditional-write shape lives in
  exactly one adapter.
- **Tier 3 is never required.** Only the `tma daemon` subcommand dispatch may
  reach `tma-daemon`. The wire protocol and the notify primitive live in
  `tma-runtime` so a daemonless `tma event` can use them. A source-guard test
  (`crates/tma/tests/tier_boundary.rs`) catches the one legitimate edge drifting,
  and `crates/tma-ui/tests/ui_boundary.rs` does the same for display code calling
  tmux directly instead of through `tma_runtime::ui`.
- **JSON schemas grow, they do not change.** A new key keeps `"schema": 1`;
  renaming or removing one bumps the version. Each serialization site has a test
  pinning its exact key set, so drift cannot ship silently. The same applies to
  the pane options, which are a published contract read by other people's status
  lines.

## Docs

The book is mdBook over `docs/` with `create-missing = false`, so `docs/SUMMARY.md`
has to track every page you add, remove, or rename, and `mdbook build` has to stay
clean. The tree is [Diátaxis](https://diataxis.fr/): a task-shaped recipe is a
how-to, a contract is reference, an argument is explanation. `docs/internal/` is
design history and is not part of the site.

## Pull requests

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/).
Run `mise run lint` and `mise run test` before pushing. Contributions are dual licensed
under Apache-2.0 and MIT, matching the project.

## Releasing

`CHANGELOG.md` is the source of the release notes: the release workflow reads the section for the
tag and refuses to build a tag that has none, so nothing ships with an empty release page.

1. `mise run changelog` prints a draft of everything since the last tag, grouped from the commit
   subjects. It is a starting point — write the entry from it under `## [Unreleased]`, in terms of
   what a user of tma sees. Machinery commits (`ci`, `chore`, `test`, `refactor`) are dropped from
   the draft on purpose, and a breaking change earns its own entry saying what to re-run.
2. `mise run release <version>` bumps the workspace version, stamps `[Unreleased]` into
   `## [<version>] - <date>`, opens a fresh `[Unreleased]`, runs lint and test, then commits and
   tags. It refuses to start when `[Unreleased]` is empty. Nothing is pushed.
3. `git push --follow-tags` builds the tarballs and publishes the release.

While the major version is 0, a breaking change bumps the minor.
