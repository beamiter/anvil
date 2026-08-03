{
  description = "jterm1 — a block-aware Linux terminal workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
      ]
      (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);

          package = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = self;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # Git dependencies are not covered by Cargo.lock checksums, so
              # Nix needs an explicit hash per revision. Update these whenever
              # the jagent / jterm_core pins in Cargo.lock change.
              outputHashes = {
                "jagent-0.5.0" = "sha256-N1k8LbpYwkbPVQjHNCjZ+k002m/zAV0eqwFs3vapLbc=";
                "jterm_core-0.1.0" = "sha256-+oC2reyitkg/JdSsDRFZFTJLudkYX+YZ0Zj9JNrmWc4=";
              };
            };
            strictDeps = true;

            nativeBuildInputs = with pkgs; [
              pkg-config
              wrapGAppsHook4
            ];

            buildInputs = with pkgs; [
              gtk4
              libadwaita
              vte-gtk4
              pcre2
              fcitx5-gtk
            ];

            # Embedded at compile time and used only when the caller has not
            # already selected an input-method setup.
            FCITX5_GTK_PATH = "${pkgs.fcitx5-gtk}/lib/gtk-4.0";

            # GTK tests are exercised under Xvfb in GitHub Actions. Running them
            # inside the sandbox without a display would produce false failures.
            doCheck = false;

            postInstall = ''
              install -Dm644 packaging/app.jterm1.desktop \
                "$out/share/applications/io.github.beamiter.jterm1.desktop"
              install -Dm644 packaging/app.jterm1.metainfo.xml \
                "$out/share/metainfo/io.github.beamiter.jterm1.metainfo.xml"
              install -Dm644 packaging/app.jterm1.svg \
                "$out/share/icons/hicolor/scalable/apps/io.github.beamiter.jterm1.svg"
              install -Dm644 packaging/app.jterm1-128.png \
                "$out/share/icons/hicolor/128x128/apps/io.github.beamiter.jterm1.png"
              install -Dm644 packaging/app.jterm1-256.png \
                "$out/share/icons/hicolor/256x256/apps/io.github.beamiter.jterm1.png"
              install -Dm644 config.toml.example \
                "$out/share/doc/jterm1/config.toml.example"
              install -Dm644 README.md \
                "$out/share/doc/jterm1/README.md"
              install -Dm644 Cargo.lock \
                "$out/share/doc/jterm1/Cargo.lock"
              install -Dm755 scripts/support-bundle.sh \
                "$out/bin/jterm1-support-bundle"

              install -d "$out/share/jterm1/shell-integration"
              install -m644 scripts/shell-integration/README.md \
                "$out/share/jterm1/shell-integration/"
              install -m644 scripts/shell-integration/jterm1.* \
                "$out/share/jterm1/shell-integration/"

              install -d "$out/share/jterm1/workflows"
              install -m644 scripts/workflows/*.yaml \
                "$out/share/jterm1/workflows/"

              install -Dm644 scripts/notebooks/welcome.jtnb.md \
                "$out/share/jterm1/notebooks/welcome.jtnb.md"
            '';

            preFixup = ''
              gappsWrapperArgs+=(
                --set-default JTERM1_WORKFLOW_DIR "$out/share/jterm1/workflows"
                --set-default JTERM1_ASSET_DIR "$out/share/jterm1"
              )
            '';

            meta = with pkgs.lib; {
              description = manifest.package.description;
              homepage = manifest.package.repository;
              mainProgram = "jterm1";
              platforms = platforms.linux;
            };
          };
        in
        {
          packages.default = package;
          apps.default = flake-utils.lib.mkApp { drv = package; };
          checks.package = package;
          formatter = pkgs.nixfmt-rfc-style;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ package ];
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              cargo-audit
              cargo-watch
              shellcheck

              gtk4
              glib
              pkg-config
              libadwaita
              vte
              vte-gtk4
              pcre2
              fcitx5-gtk

              glade
              cambalache
              xdotool
              jq
              valgrind
              strace
              patchelf
              fuse
              fakeroot
              openssl
            ];

            shellHook = ''
              export GSETTINGS_SCHEMA_DIR="${pkgs.gtk4}/share/gsettings-schemas/:${pkgs.glib}/share/gsettings-schemas/"
              export RUST_BACKTRACE=1
              export GTK_IM_MODULE="''${GTK_IM_MODULE:-fcitx}"
              export XMODIFIERS="''${XMODIFIERS:-@im=fcitx}"
              export QT_IM_MODULE="''${QT_IM_MODULE:-fcitx}"
              export GTK_PATH="${pkgs.fcitx5-gtk}/lib/gtk-4.0''${GTK_PATH:+:$GTK_PATH}"
              export FCITX5_GTK_PATH="${pkgs.fcitx5-gtk}/lib/gtk-4.0"
              echo "jterm1 development environment ready. Run 'make verify'."
            '';
          };
        }
      );
}
