{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
        f { pkgs = import nixpkgs { inherit system; }; });
    in {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.dictate;
          dictate = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in {
          options.services.dictate = {
            enable = lib.mkEnableOption "Dictate voice-to-text daemon";

            user = lib.mkOption {
              type = lib.types.str;
              description = "User to run the daemon as";
            };

            environmentFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "Path to environment file (DEEPGRAM_API_KEY, optionally DICTATE_KEY)";
            };
          };

          config = lib.mkIf cfg.enable {
            users.users.${cfg.user}.packages = [ dictate ];

            systemd.user.services.dictate = {
              description = "Dictate voice-to-text daemon";
              after = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              wantedBy = [ "graphical-session.target" ];

              serviceConfig = {
                Environment = [
                  "PATH=${lib.makeBinPath [ pkgs.wtype pkgs.wl-clipboard pkgs.pipewire pkgs.sox ]}"
                  "XDG_RUNTIME_DIR=/run/user/%U"
                ];
                Type = "simple";
                ExecStart = "${dictate}/bin/dictate daemon";
                Restart = "on-failure";
                RestartSec = 3;
                EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
                PassEnvironment = [ "WAYLAND_DISPLAY" ];

                NoNewPrivileges = true;
                ProtectControlGroups = true;
                ProtectKernelTunables = true;
                RestrictSUIDSGID = true;
              };
            };
          };
        };

      packages = forAllSystems ({ pkgs }: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "dictate";
          version = "0.2.0";
          src = nixpkgs.lib.cleanSource ./.;
          useFetchCargoVendor = true;
          cargoHash = "sha256-AN267ZgjpgXtfwjoYlj6d+xuIZL0+KA8EqQQQz/pSes=";
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.alsa-lib pkgs.openssl ];
          meta = {
            description = "Voice-to-text dictation daemon";
            mainProgram = "dictate";
          };
        };
      });
    };
}
