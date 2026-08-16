# Bundled agent manifests

One TOML file per agent. These are embedded into the `tma` binary via
`include_str!` (see `tma-runtime/src/manifests.rs`) and are the zero-config detection corpus.
User overrides at `~/.config/tma/agents/<agent>.toml` shadow the bundled file by stem.
Every rule here is evidence-authored with a redacted fixture under `tma-core/fixtures/`.
