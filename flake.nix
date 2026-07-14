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

            cargoLock.lockFile = ./Cargo.lock;
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
                "$out/share/applications/app.jterm1.desktop"
              install -Dm644 config.toml.example \
                "$out/share/doc/jterm1/config.toml.example"
              install -Dm644 README.md \
                "$out/share/doc/jterm1/README.md"

              install -d "$out/share/jterm1/shell-integration"
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
              cargo-watch

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
