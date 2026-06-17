{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
        f { pkgs = import nixpkgs { inherit system; }; });
    in {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.dictate-desktop;
          dictate-desktop = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in {
          options.services.dictate-desktop = {
            enable = lib.mkEnableOption "Dictate desktop voice-to-text daemon";

            user = lib.mkOption {
              type = lib.types.str;
              description = "User to run the daemon as";
            };

            environmentFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "Path to environment file with API keys for STT providers";
            };

            proxy = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              example = "socks5h://127.0.0.1:1080";
              description = "SOCKS/HTTP proxy for API requests (DICTATE_PROXY)";
            };
          };

          config = lib.mkIf cfg.enable {
            users.users.${cfg.user}.packages = [ dictate-desktop ];

            systemd.user.services.dictate-desktop = {
              description = "Dictate desktop voice-to-text daemon";
              after = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              wantedBy = [ "graphical-session.target" ];

              serviceConfig = {
                Environment = [
                  "PATH=${lib.makeBinPath [ pkgs.pipewire pkgs.fontconfig ]}"
                  "XDG_RUNTIME_DIR=/run/user/%U"
                  "FONTCONFIG_FILE=${pkgs.makeFontsConf { fontDirectories = [ pkgs.inter ]; }}"
                ] ++ lib.optional (cfg.proxy != null) "DICTATE_PROXY=${cfg.proxy}";
                Type = "simple";
                ExecStart = "${dictate-desktop}/bin/dictate-desktop daemon";
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

      devShells = forAllSystems ({ pkgs }: {
        default = pkgs.mkShell {
          nativeBuildInputs = [ pkgs.pkg-config pkgs.mold ];
          buildInputs = [ pkgs.alsa-lib pkgs.openssl pkgs.libxkbcommon pkgs.wayland pkgs.libglvnd ];
          LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath [ pkgs.libglvnd pkgs.wayland ]}";
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
        };
      });

      packages = forAllSystems ({ pkgs }: let
        craneLib = crane.mkLib pkgs;
      in {
        default = let
          unwrapped = craneLib.buildPackage {
            src = craneLib.cleanCargoSource ./.;
            nativeBuildInputs = [ pkgs.pkg-config pkgs.installShellFiles ];
            buildInputs = [ pkgs.alsa-lib pkgs.openssl pkgs.libxkbcommon pkgs.wayland ];
            postInstall = ''
              installShellCompletion --cmd dictate-desktop \
                --zsh <($out/bin/dictate-desktop completions zsh) \
                --bash <($out/bin/dictate-desktop completions bash) \
                --fish <($out/bin/dictate-desktop completions fish)
            '';
            meta = {
              description = "Voice-to-text dictation daemon";
              mainProgram = "dictate-desktop";
            };
          };
        in pkgs.symlinkJoin {
          name = "dictate-desktop-wrapped";
          paths = [ unwrapped ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postBuild = ''
            wrapProgram $out/bin/dictate-desktop \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath [ pkgs.libglvnd pkgs.wayland ]}" \
              --prefix PATH : "${pkgs.lib.makeBinPath [ pkgs.flac ]}"
          '';
          inherit (unwrapped) meta;
        };
      });
    };
}
