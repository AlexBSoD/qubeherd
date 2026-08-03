{
  description = "Bridge herdr agent state to the Ergohaven Qube dongle screen";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        qubeherd = pkgs.stdenvNoCC.mkDerivation {
          pname = "qubeherd";
          version = "0.1.0";
          src = ./.;
          installPhase = ''
            install -Dm755 qubeherd.py $out/bin/qubeherd
            substituteInPlace $out/bin/qubeherd \
              --replace-fail '#!/usr/bin/env python3' '#!${pkgs.python3}/bin/python3'
          '';
          meta = {
            description = "Push herdr agent-state counts to an Ergohaven Qube dongle";
            mainProgram = "qubeherd";
          };
        };
        default = qubeherd;
      });

      homeModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.qubeherd;
        in
        {
          options.services.qubeherd = {
            enable = lib.mkEnableOption "the herdr -> Qube dongle agent status bridge";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.qubeherd;
              description = "qubeherd package to run.";
            };
            socket = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              example = "%h/.config/herdr/herdr.sock";
              description = "herdr API socket; defaults to the one qubeherd discovers itself.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.user.services.qubeherd = {
              Unit = {
                Description = "herdr agent status on the Qube dongle";
                After = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart =
                  "${lib.getExe cfg.package}"
                  + lib.optionalString (cfg.socket != null) " --socket ${cfg.socket}";
                # The daemon reconnects on its own; a restart only matters if
                # it dies outright (e.g. the socket path never appears).
                Restart = "on-failure";
                RestartSec = 5;
              };
              Install.WantedBy = [ "default.target" ];
            };
          };
        };
    };
}
