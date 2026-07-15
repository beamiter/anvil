# jterm1 portable Linux bundle

This archive contains a prebuilt `jterm1` binary plus its desktop entry, shell
integrations, example workflows, documented configuration, and welcome notebook.
It installs only into the current user's XDG directories; root access is not
required, and an existing `config.toml` is never overwritten.

## Runtime requirements

A graphical Linux desktop with GTK 4, libadwaita, and GTK4 VTE libraries is
required. Optional integrations include `notify-send`, OpenSSH, and an AI
provider configured through environment variables.

## Verify, extract, and install

From the directory containing the downloaded archive and checksum:

```bash
sha256sum --check jterm1-*.tar.gz.sha256
tar -xzf jterm1-*.tar.gz
cd jterm1-*/
./install.sh
```

After installation:

```bash
jterm1 --doctor
jterm1 --doctor --json
jterm1 --check-config
jterm1 --safe-mode
jterm1
```

For support, `jterm1-support-bundle [OUTPUT_DIRECTORY]` creates a privacy-preserving
archive that excludes configuration contents, terminal history/output, and secret
values. In-app settings use revision-checked atomic writes and two rotating backups;
`jterm1 --restore-config-backup` restores the newest valid one while preserving the
replaced file.

The installer writes the binary to `~/.local/bin/jterm1` and assets beneath
`${XDG_DATA_HOME:-$HOME/.local/share}`. Add `~/.local/bin` to `PATH` when needed.
