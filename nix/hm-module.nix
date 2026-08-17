{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.tma;
  tomlFormat = pkgs.formats.toml { };
in
{
  options.programs.tma = {
    enable = lib.mkEnableOption "tma, the tmux agent state monitor";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.tma or (pkgs.callPackage ./package.nix { });
      defaultText = lib.literalExpression "pkgs.tma";
      description = ''
        The tma package to install. Defaults to `pkgs.tma` when the flake's
        overlay is applied, otherwise builds from this module's source tree.
      '';
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          fold.dwell_secs = 5;
          notify.on = [ "blocked" "done" ];
        }
      '';
      description = ''
        Contents of {file}`$XDG_CONFIG_HOME/tma/config.toml`. Every section is
        optional; an empty attrset writes no file and tma falls back to its
        built-in defaults.
      '';
    };

    agents = lib.mkOption {
      type = lib.types.attrsOf tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          myagent = {
            min_engine_version = "0.1";
            identity.process_names = [ "myagent" ];
            capture.visible = [ "blocked" ];
          };
        }
      '';
      description = ''
        Agent manifests, written to
        {file}`$XDG_CONFIG_HOME/tma/agents/<name>.toml`. A new stem adds an
        agent; a stem matching a bundled manifest (claude, codex, cursor,
        gemini, opencode, pi) replaces it wholesale, so such an entry must be a
        complete manifest.
      '';
    };

    keybindings.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Add tma's default tmux keybindings (the same prefix set `tma
        install-keys` writes) to {option}`programs.tmux.extraConfig`: prefix+a
        picker popup, prefix+G watch window, prefix+A
        action menu, prefix+j/g/b/h jumps. The opt-in mouse bindings are not
        written; run `tma install-keys --mouse` for those. Declarative
        alternative to running `tma install-keys`; do not use both, or the
        bindings are defined twice.
      '';
    };

    daemon.autostart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Start the event-hub daemon for every tmux server that loads this config
        (what `tma install-keys --daemon` writes). `#{socket_path}` pins the
        server doing the loading, so a `tmux -L work` server gets its own
        daemon rather than the default one, and `--ensure` is idempotent. The
        daemon exits on its own when its tmux server does.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.keybindings.enable -> config.programs.tmux.enable;
        message = "programs.tma.keybindings.enable requires programs.tmux.enable";
      }
      {
        assertion = cfg.daemon.autostart -> config.programs.tmux.enable;
        message = "programs.tma.daemon.autostart requires programs.tmux.enable";
      }
    ];

    home.packages = [ cfg.package ];

    xdg.configFile = lib.mkMerge [
      (lib.mkIf (cfg.settings != { }) {
        "tma/config.toml".source = tomlFormat.generate "tma-config.toml" cfg.settings;
      })
      (lib.mapAttrs' (
        name: manifest:
        lib.nameValuePair "tma/agents/${name}.toml" {
          source = tomlFormat.generate "tma-agent-${name}.toml" manifest;
        }
      ) cfg.agents)
    ];

    # Kept verbatim in sync with the BINDINGS table in crates/tma/src/install_keys.rs; a test there
    # reads this file and fails if the two sets diverge. tmux's `#{...}` formats need no escaping
    # here: Nix antiquotation is `${`, not `#{`.
    programs.tmux.extraConfig = lib.mkMerge [
      (lib.mkIf cfg.keybindings.enable ''
        # tma keybindings (programs.tma.keybindings.enable)
        bind-key a display-popup -E -w 80% -h 60% 'tma'
        bind-key G new-window 'tma watch --table'
        bind-key j run-shell 'tma jump --attention --client "#{client_name}"'
        bind-key g run-shell 'tma jump --blocked --client "#{client_name}"'
        bind-key b run-shell 'tma jump --back --client "#{client_name}"'
        bind-key h run-shell 'tma jump --home --client "#{client_name}"'
        bind-key A run-shell 'tma act --menu --pane "#{pane_id}"'
      '')
      # Byte-identical to DAEMON_LINE in crates/tma/src/install_keys.rs; the same test pins it.
      (lib.mkIf cfg.daemon.autostart ''
        # tma daemon (programs.tma.daemon.autostart)
        run-shell -b 'tma --socket-path "#{socket_path}" daemon --ensure >/dev/null 2>&1'
      '')
    ];
  };
}
