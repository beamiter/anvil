# anvil relocatable Linux bundle

This archive contains a prebuilt `anvil` binary plus its desktop entry, shell
integrations, example workflows, documented configuration, and welcome notebook.
It installs only into the current user's XDG directories; root access is not
required, and an existing `config.toml` is never overwritten.

The archive is relocatable before installation, but it is not statically linked
or self-contained; the compatible GTK/libadwaita/VTE runtime libraries below
must already exist on the target system.

## Runtime requirements

A graphical Linux desktop with GTK 4, libadwaita, and GTK4 VTE libraries is
required. Optional integrations include `notify-send`, OpenSSH, and an AI
provider configured through environment variables.

## Verify, extract, and install

From the directory containing the downloaded archive and checksum:

```bash
sha256sum --check anvil-*.tar.gz.sha256
tar -xzf anvil-*.tar.gz
cd anvil-*/
./install.sh
```

The extracted `./uninstall.sh` removes the binary and installed assets while
preserving configuration and state by default. Add `--purge-config` only when
those user files should also be removed.

After installation:

```bash
anvil --doctor
anvil --doctor --json
anvil --check-config
anvil --safe-mode
anvil
```

For support, `anvil-support-bundle [OUTPUT_DIRECTORY]` creates a privacy-preserving
archive that excludes configuration contents, terminal history/output, and secret
values. In-app settings use revision-checked atomic writes and two rotating backups;
`anvil --restore-config-backup` restores the newest valid one while preserving the
replaced file.

The installer writes the binary to `~/.local/bin/anvil` and assets beneath
`${XDG_DATA_HOME:-$HOME/.local/share}`. Add `~/.local/bin` to `PATH` when needed.
