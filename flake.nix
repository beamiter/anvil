{
  description = "anvil — a block-aware Linux terminal workspace";

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
              # Nix needs an explicit hash per revision. A jterm_core repin can
              # also move its transitive jagent pin: compare both source lines
              # in Cargo.lock, set each affected value to pkgs.lib.fakeHash,
              # then run `nix flake check --no-write-lock-file` and copy `got:`.
              # Full revisions beside the hashes make a stale repin conspicuous.
              outputHashes = {
                # e94469b3b0b706100cfda91ea80281dbbfe6fe6b
                "jagent-0.5.0" = "sha256-UuHxaZTR9gQB27E2d2iyjCaICBADmecC6E/GNyvCJIE=";
                # 468b1b5f83c14c53fded02715ccb4bb2a721855d
                "jterm_core-0.1.0" = "sha256-zMCTbpuMSOXN3b3yNzBEV2FxIYkFmoDIFVzL7AJNuLw=";
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
              install -Dm644 data/io.github.beamiter.anvil.desktop \
                "$out/share/applications/io.github.beamiter.anvil.desktop"
              install -Dm644 data/io.github.beamiter.anvil.metainfo.xml \
                "$out/share/metainfo/io.github.beamiter.anvil.metainfo.xml"
              install -Dm644 data/io.github.beamiter.anvil.svg \
                "$out/share/icons/hicolor/scalable/apps/io.github.beamiter.anvil.svg"
              install -Dm644 data/io.github.beamiter.anvil-128.png \
                "$out/share/icons/hicolor/128x128/apps/io.github.beamiter.anvil.png"
              install -Dm644 data/io.github.beamiter.anvil-256.png \
                "$out/share/icons/hicolor/256x256/apps/io.github.beamiter.anvil.png"
              install -Dm644 config.toml.example \
                "$out/share/doc/anvil/config.toml.example"
              install -Dm644 README.md \
                "$out/share/doc/anvil/README.md"
              install -Dm644 Cargo.lock \
                "$out/share/doc/anvil/Cargo.lock"
              install -Dm755 scripts/support-bundle.sh \
                "$out/bin/anvil-support-bundle"

              install -d "$out/share/anvil/shell-integration"
              install -m644 scripts/shell-integration/README.md \
                "$out/share/anvil/shell-integration/"
              install -m644 scripts/shell-integration/anvil.* \
                "$out/share/anvil/shell-integration/"

              install -d "$out/share/anvil/workflows"
              install -m644 scripts/workflows/*.yaml \
                "$out/share/anvil/workflows/"

              install -Dm644 scripts/notebooks/welcome.jtnb.md \
                "$out/share/anvil/notebooks/welcome.jtnb.md"
            '';

            preFixup = ''
              gappsWrapperArgs+=(
                --set-default ANVIL_WORKFLOW_DIR "$out/share/anvil/workflows"
                --set-default ANVIL_ASSET_DIR "$out/share/anvil"
              )
            '';

            meta = with pkgs.lib; {
              description = manifest.package.description;
              homepage = manifest.package.repository;
              mainProgram = "anvil";
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
              echo "anvil development environment ready. Run 'make verify'."
            '';
          };
        }
      );
}
