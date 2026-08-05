# Flatpak packaging and host integration

The `io.github.beamiter.anvil.yml` manifest targets the GNOME 50 runtime with application ID
`io.github.beamiter.anvil`. The committed
`cargo-sources.json` is generated from `Cargo.lock`, so the Rust build runs with
Cargo networking disabled.

## Host-shell boundary

A terminal must operate on the user's real development environment. In a
Flatpak launch, anvil therefore routes Block and VTE shells, Notebook cells,
Git probes, notifications, and dependency probes through
`flatpak-spawn --host --watch-bus`. Native launches execute the same commands
directly. Cwd and `TERM_PROGRAM=anvil` are forwarded explicitly.

This means the Flatpak is not a sandbox for commands typed into the terminal or
run from a trusted Notebook. Those commands intentionally have the current
host user's authority. Review `.jtnb.md` content before Run or Run All.

The manifest grants Wayland/fallback X11, IPC, DRI, host files, network, SSH
agent, and access to `org.freedesktop.Flatpak` for that bridge. OSC 52 clipboard
writes and AI remain governed by anvil's own opt-in settings.

## Build

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean \
  --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
  packaging/flatpak/io.github.beamiter.anvil.yml
flatpak build-bundle flatpak-repo io.github.beamiter.anvil.flatpak \
  io.github.beamiter.anvil
sha256sum io.github.beamiter.anvil.flatpak
```

Install and diagnose with:

```bash
flatpak --user install ./io.github.beamiter.anvil.flatpak
flatpak run io.github.beamiter.anvil --doctor
flatpak run --command=anvil-support-bundle io.github.beamiter.anvil "$PWD"
flatpak run io.github.beamiter.anvil
```

Host rc files cannot reliably read `/app/share`. Load the embedded integration
through the application instead, for example in `~/.bashrc`:

```bash
if [[ $TERM_PROGRAM == anvil ]]; then
    source <(flatpak run io.github.beamiter.anvil --shell-integration bash)
fi
```

Use the equivalent `zsh` or fish command for those shells. API keys are not
stored in `config.toml`; provide them through a trusted launcher or an explicit
Flatpak override.

To regenerate `cargo-sources.json`, use the pinned Flatpak Cargo generator
against the repository's current `Cargo.lock`, then verify that the JSON has
exactly two entries per registry package plus the final Cargo source config.
