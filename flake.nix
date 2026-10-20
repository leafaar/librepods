{
  description = "AirPods liberated from Apple's ecosystem";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-compat.url = "https://flakehub.com/f/edolstra/flake-compat/1.tar.gz";
    systems.url = "github:nix-systems/default";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      crane,
      flake-parts,
      systems,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import systems;
      imports = [
        inputs.treefmt-nix.flakeModule
      ];

      perSystem =
        {
          self',
          system,
          lib,
          ...
        }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          buildInputs =
            with pkgs;
            [
              dbus
              libpulseaudio
              # libavcodec/libavutil for the hi-res microphone's AAC-ELD decoder
              ffmpeg-headless
              bluez
              gtk4
              libadwaita
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
              pkgs.libiconv
            ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            # libclang for ffmpeg-sys-next's bindgen step
            rustPlatform.bindgenHook
            # Points the installed binary at GTK's schemas, icons and modules.
            wrapGAppsHook4
          ];

          # Build with the same toolchain the repo pins for everyone else.
          craneLib = (crane.mkLib pkgs).overrideToolchain (
            p: p.rust-bin.fromRustupToolchainFile ./linux-rust/rust-toolchain.toml
          );
          unfilteredRoot = ./linux-rust/.;
          src = lib.fileset.toSource {
            root = unfilteredRoot;
            fileset = lib.fileset.unions [
              # Default files from crane (Rust and cargo files)
              (craneLib.fileset.commonCargoSources unfilteredRoot)
              # The desktop entry and the app icon, installed under share/.
              ./linux-rust/assets
              ./linux-rust/clippy.toml
              ./linux-rust/deny.toml
            ];
          };

          commonArgs = {
            inherit buildInputs nativeBuildInputs src;
            strictDeps = true;

            # RUST_BACKTRACE = "1";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          librepods = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              doCheck = false;

              # The window and the desktop entry name the icon by the app id, and
              # StartupWMClass matches the program name main sets.
              postInstall = ''
                install -Dm644 assets/me.kavishdevar.librepods.desktop \
                  $out/share/applications/me.kavishdevar.librepods.desktop
                install -Dm644 assets/icon.png \
                  $out/share/icons/hicolor/256x256/apps/me.kavishdevar.librepods.png
              '';
              # The symbolic icons the UI uses (bluetooth, headphones, batteries),
              # for sessions without an icon theme of their own. Appended, so the
              # session's theme still wins.
              preFixup = ''
                gappsWrapperArgs+=(--suffix XDG_DATA_DIRS : "${pkgs.adwaita-icon-theme}/share")
              '';

              meta = {
                description = "AirPods liberated from Apple's ecosystem";
                homepage = "https://github.com/kavishdevar/librepods";
                license = pkgs.lib.licenses.gpl3Only;
                maintainers = [ "kavishdevar" ];
                platforms = pkgs.lib.platforms.unix;
                mainProgram = "librepods";
              };
            }
          );
        in
        {
          checks = {
            inherit librepods;

            librepods-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- -D warnings";
              }
            );

            librepods-test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });

            # Advisories need the network, which the sandbox does not allow; CI
            # runs that part of cargo deny separately.
            librepods-deny = craneLib.cargoDeny {
              inherit src;
              cargoDenyChecks = "bans licenses sources";
            };
          };

          packages.default = librepods;
          apps.default = {
            type = "app";
            program = lib.getExe librepods;
            meta.description = librepods.meta.description;
          };

          devShells.default = craneLib.devShell {
            name = "librepods-dev";
            checks = self'.checks;

            # NOTE: cargo and rustc are provided by default.
            buildInputs =
              with pkgs;
              [
                rust-analyzer
              ]
              ++ buildInputs;

          };

          treefmt = {
            programs.nixfmt.enable = pkgs.lib.meta.availableOn pkgs.stdenv.buildPlatform pkgs.nixfmt.compiler;
          };
        };
    };
}
