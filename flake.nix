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
        qubeherd = pkgs.rustPlatform.buildRustPackage {
          pname = "qubeherd";
          version = "0.2.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "Push herdr agent state, host clock and keyboard layout to an Ergohaven Qube dongle";
            mainProgram = "qubeherd";
          };
        };
        default = qubeherd;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
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
                # The layout source lives on the desktop session bus, so start
                # after it and stop with it rather than at plain login.
                After = [ "graphical-session.target" ];
                PartOf = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart =
                  "${lib.getExe cfg.package}"
                  + lib.optionalString (cfg.socket != null) " --socket ${cfg.socket}";
                # The daemon reconnects on its own; a restart only matters if
                # it dies outright (e.g. the socket path never appears) — or if
                # it stops going round its loop, which is what the watchdog is
                # for: a wedged process is otherwise indistinguishable from an
                # idle one, and Restart=on-failure would never hear about it.
                Type = "notify";
                NotifyAccess = "main";
                WatchdogSec = 90;
                Restart = "on-failure";
                RestartSec = 5;
              };
              Install.WantedBy = [ "graphical-session.target" ];
            };
          };
        };
    };
}
