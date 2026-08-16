{
  lib,
  rustPlatform,
  tmux,
  unixtools,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tma";
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  # Only the files the build needs: book/, target/ and friends stay out of the
  # store path, so unrelated edits don't force a rebuild. docs/ and README.md
  # are included because workspace tests include_str! them.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../docs
      ../README.md
      # The keybinding drift test reads hm-module.nix, so module edits must rebuild.
      ../nix
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # The workspace has one bin crate; build just its dependency closure.
  cargoBuildFlags = [
    "-p"
    "tma"
  ];

  # Tests spawn a scratch tmux server and `tma doctor` shells out to `ps`. No locale on
  # purpose: the scrubbed sandbox env regression-tests tmux's utf8_sanitize vs `tmux -u`.
  nativeCheckInputs = [
    tmux
    unixtools.ps
  ];

  meta = {
    description = "tmux-agents CLI: agent state monitor, picker, jump, and stamping for tmux";
    homepage = "https://github.com/pperanich/tmux-agents";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "tma";
    # tma-tmux is a unix-only crate (unix process/permission APIs).
    platforms = lib.platforms.unix;
  };
})
