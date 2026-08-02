# jterm1

jterm1 is an experimental Linux terminal emulator built with Rust, GTK 4,
libadwaita, Relm4, and VTE. It can behave like a conventional VTE terminal or
turn completed commands into navigable blocks with their command, output, exit
status, duration, and working directory.

The project is currently at version `0.2.0`. Treat it as an early-stage daily
driver: keep backups of important configuration and do not rely on session
restoration as the only copy of work in progress.

## Highlights

- VTE and block-aware terminal modes
- Tabs, nested split panes, pane zoom, directional focus, and session restore
- Sidebar tab list and a lazy file tree; files are safely shell-quoted before
  being inserted at the prompt
- Command palette, command-history picker, parameterized TOML/YAML workflows, and
  fuzzy search
- Search within terminal output, block selection, output filtering, bookmarks,
  copy/rerun controls, and long-command notifications
- SSH host picker, connection status, multiplexing, and reconnect support
- Optional AI command generation, error explanation, session Q&A, and a
  multi-turn command agent with explicit approval before execution
- Runnable Markdown notebooks (`.jtnb.md`) with isolated multi-shell cells
- Live appearance settings and a hot-reloaded TOML configuration

## Requirements

jterm1 targets a graphical Linux desktop running X11 or Wayland. The source
installer prefers [Nix with flakes enabled](https://nixos.org/download/) and
falls back to Cargo when the GTK 4, libadwaita, VTE, and native build
dependencies are already available through the system toolchain.

The GTK 4 stack must be recent (glib >= 2.80, pango >= 1.52, gtk4 >= 4.14,
libadwaita >= 1.5, and the GTK 4 build of VTE, `vte-2.91-gtk4` >= 0.76). Stable
distributions such as Ubuntu 22.04 ship these too old or omit `vte-2.91-gtk4`
entirely, so `cargo install --path .` fails in the `*-sys` build scripts. Run
`./scripts/bootstrap_deps.sh` to provision them:

```bash
./scripts/bootstrap_deps.sh            # set up the recommended toolchain (Nix)
./scripts/bootstrap_deps.sh --check    # report what is missing, install nothing
./scripts/bootstrap_deps.sh --backend system --install   # use distro packages
```

It defaults to a Nix-based toolchain (which pins matching library versions
without touching system packages) and can install the distro `-dev` packages
instead with `--backend system`.

Runtime integrations are optional:

- `notify-send` for long-command desktop notifications
- `jq` for `scripts/show-state.sh`
- `cargo-watch` for `make watch`
- `valgrind` or `strace` for the matching debug helpers

## Install

```bash
git clone https://github.com/beamiter/jterm1.git
cd jterm1
./scripts/install.sh
./scripts/install.sh --backend cargo
./scripts/install.sh --prefix /opt/jterm1 --data-dir /opt/jterm1/share
./scripts/install.sh --dry-run
```

The installer supports `DESTDIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
`CARGO_TARGET_DIR`; it never overwrites an existing configuration. By default
it builds a release binary and installs only user-local files:

- `~/.local/bin/jterm1`
- `${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/config.toml`
- `${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.beamiter.jterm1.desktop`
- icons under
  `${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/{scalable,128x128,256x256}/apps/`
  and AppStream metadata under `.../metainfo/`
- shell integration and examples under
  `${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/`
- sample workflows under
  `${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/workflows/`

That desktop integration is what makes jterm1 appear in the GNOME/KDE
application list with its own icon, ready to pin. Two details matter for it to
show up at all, and the installer handles both:

- `Exec=`/`TryExec=` are rewritten to the binary's absolute path (system
  prefixes such as `/usr` keep the relocatable bare name). A desktop session
  fixes its `PATH` at login, so `TryExec=jterm1` fails and hides the entry
  **completely** when `~/.local/bin` is not on that `PATH` — the usual reason an
  install produces no launcher icon.
- `update-desktop-database` and `gtk-update-icon-cache` are refreshed after
  install and uninstall (a stale icon cache shadows newly installed icons).
  `DESTDIR` builds skip the refresh and leave it to the package manager.

Verify with `desktop-file-validate <entry>` and `gtk-launch
io.github.beamiter.jterm1`; use `--no-desktop` to install only the binary.

It never replaces an existing `config.toml`; installed examples live outside
the user-authored workflow directory. Make sure `~/.local/bin` is in `PATH`,
then run:

```bash
jterm1
jterm1 --doctor
jterm1 --doctor --json            # machine-readable support diagnostics
jterm1 --check-config             # validate config without exposing its values
jterm1 --check-config ~/test.toml # validate one file without changing the active path
jterm1 -c ~/test.toml --doctor    # use one alternate config for this process
jterm1 --config-path              # print the active config path
jterm1 --safe-mode                # isolated VTE + sh recovery session
```

Useful headless commands:

```bash
jterm1 --help
jterm1 --version
jterm1 --init-config                 # create config without overwriting one
jterm1 --check-config --json         # machine-readable schema validation
jterm1 --config ~/test.toml --config-path # print an explicit effective path
jterm1 --restore-config-backup       # restore newest valid rotating backup
jterm1 --shell-integration bash      # print an integration script
jterm1 --mode vte --no-restore       # launch a fresh compatibility session
jterm1 -d /path/to/project           # launch in a directory
jterm1 -e bash -lc 'printf "hello\n"'
```

Remove installed binaries and assets while preserving configuration and state:

```bash
./scripts/uninstall.sh
```

Use `./scripts/uninstall.sh --purge-config` only when configuration and runtime
state should also be deleted. Flatpak build, install, and host-command bridge
instructions are in
[`packaging/flatpak/README.md`](packaging/flatpak/README.md).

For development:

```bash
make run       # debug build and launch
make test      # cargo test --all-targets
make check     # cargo check --all-targets
make build     # release build
make clippy    # repository lint policy
make security  # dependency audit + ShellCheck
make help      # all helpers
```

## Diagnostics and recovery

`jterm1 --doctor` reports configuration, shell, display, integrations, remote
readiness, permissions, and session-state metadata. Add `--json` for automation
or support tooling; neither format includes configuration contents, terminal
history, command output, environment values, or credentials.

When configuration, startup commands, session restore, or an integration causes
a bad launch, use:

```bash
jterm1 --safe-mode
```

Safe mode starts a local VTE pane with `sh`, skips session restore and persistence,
ignores configured startup commands and remote hosts, disables AI, notifications,
repository probes, history, remote clipboard writes, and jsh update/install
operations, and refuses to save or hot-reload settings for that process.

Create a privacy-preserving support archive with:

```bash
jterm1-support-bundle ~/Desktop
```

Review the archive before sharing it. The bundle contains structured diagnostics,
system identity, linked-library information, and file metadata only.

Validate configuration keys, types, ranges, colors, shortcuts, and remote-host
records without starting GTK:

```bash
jterm1 --check-config
jterm1 --check-config --json
```

The report names keys and problems but never includes configuration values. If a
bad edit or interrupted recovery leaves the live file unusable, restore the newest
valid rotating backup with:

```bash
jterm1 --restore-config-backup
```

The command preserves the replaced live file as `config.toml.before-restore`.

## Terminal modes

Block mode is the default. It keeps a live VTE input cell at the bottom and
promotes each finished command into a separate block:

```toml
terminal_mode = "block"
```

Use the conventional VTE backend when compatibility with terminal applications
matters more than command blocks:

```toml
terminal_mode = "vte"
```

Block boundaries are most reliable when the shell emits OSC 133 command marks
and OSC 7 working-directory updates. The installer places the integration files
under:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/shell-integration/
```

Source the file for the current shell. Sourcing unconditionally works in both
jterm1 backends; the scripts protect against being loaded twice, and terminals
that do not understand the OSC sequences ignore them.

After installing jterm1, Bash and Zsh can also load the script embedded in the
binary:

```bash
source <(jterm1 --shell-integration bash)
```

```bash
# ~/.bashrc
source "${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/shell-integration/jterm1.bash"
```

```zsh
# ~/.zshrc
source "${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/shell-integration/jterm1.zsh"
```

```fish
# ~/.config/fish/config.fish
set -l jterm1_data_home "$HOME/.local/share"
if set -q XDG_DATA_HOME
    set jterm1_data_home "$XDG_DATA_HOME"
end
source "$jterm1_data_home/jterm1/shell-integration/jterm1.fish"
```

PowerShell users can dot-source `jterm1.ps1`; its Enter hook requires
PSReadLine. More detail is in
[`scripts/shell-integration/README.md`](scripts/shell-integration/README.md).

Shell selection follows this order: `JTERM1_SHELL`, the `shell` config key,
`jsh` when it is executable on `PATH`, `bash -l`, then `sh`.

## Default shortcuts

Shortcuts are captured at the window level unless noted otherwise. They can be
overridden in `[keybindings]`; the command palette displays the bindings that
are currently active.

| Shortcut | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close the focused pane, or the tab when it has one pane |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| `Ctrl+Shift+Alt+C` | In block mode, copy the selected block's output only |
| `Ctrl+Shift+E` / `Ctrl+Shift+D` | Split left/right / top/bottom |
| `Ctrl+Alt+Arrow` | Focus the pane in that direction |
| `Ctrl+Shift+Alt+Arrow` | Resize the active split |
| `Ctrl+Shift+Z` | Toggle pane zoom |
| `Ctrl+Shift+!` | Move the focused pane into a new tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+PageDown/PageUp` | Next / previous tab |
| `Ctrl+1` ... `Ctrl+8` | First ... eighth tab |
| `Ctrl+9` | Last tab |
| `Ctrl+Shift+P` | Command palette |
| `Ctrl+Shift+H` | History palette; `Ctrl+R` and `Ctrl+P` remain available to the shell |
| `Ctrl+Shift+F` | Search terminal output (`/pattern/` enables regex) |
| `Ctrl+Shift+G` | In block mode, search command and output lines across all finished blocks |
| `Ctrl+Shift+O` | Settings |
| `Ctrl+Shift+R` | Reload configuration |
| `Ctrl+\` | Toggle sidebar |
| `Ctrl+Alt+B` | Move tabs between sidebar and top bar |
| `Ctrl+Shift+L` | Focus the tab filter |
| `Ctrl+Shift+S` | Remote host picker |
| `Ctrl+Shift+X` | Jump to the first failed block |
| `Ctrl+Shift+N` | Jump to the oldest block |
| `Ctrl+Shift+A` | Select all finished blocks |
| `Ctrl+Shift+I` | Reinput selected block commands without running them |
| `Ctrl+Shift+K` | Clear all finished blocks in the pane |
| `Ctrl+Shift+Alt+A` | Session AI panel |
| `Ctrl+Shift+Q` | Ask AI about the selected finished block |
| `Ctrl+Shift+M` | Workflows |
| `Ctrl+Alt+G` | AI agent |
| `F12` | Debug dashboard |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Increase / decrease / reset font scale |
| `Ctrl+Alt+-` / `Ctrl+Alt+=` | Decrease / increase window opacity |
| `Ctrl+Up/Down` | Scroll terminal output |

Block mode also has context-sensitive navigation:

- `Home` / `End` and `PageUp` / `PageDown` navigate completed output while no
  command or full-screen application owns the viewport.
- With one or more blocks selected, `Up` / `Down` moves the active edge,
  `Shift+Up/Down` extends the range, `Enter` recalls every selected command in
  terminal order without running it, and `Escape` clears the selection.
- `Ctrl+Shift+B` bookmarks the selected block.
- `Ctrl+,` / `Ctrl+.` jumps to the previous / next bookmarked block.
- `Alt+Shift+F` toggles the selected or most recent block's output filter.

Slow-block, pinned-block, and non-contextual pinned-navigation actions remain
available in the command palette and can be assigned in `[keybindings]`, but
have no default shortcuts.

## Installing and updating jsh

jterm1 prefers its companion shell [`jsh`](https://github.com/beamiter/jsh) and
falls back to bash only when it cannot find one. The palette action
**Install or update jsh** runs the installer in a dedicated VTE tab: the tab is
the progress UI, so it can be interrupted with Ctrl+C and it waits for Enter
before closing, instead of a failure flashing past.

The installer is embedded in the binary, so a machine that has never had jsh can
still bootstrap one. It verifies the download's checksum, swaps the binary in
with `rename(2)` — **shells that are already running keep the version they
started with; new tabs pick up the new one** — keeps the previous binary for
rollback, and reports when `PATH` resolves `jsh` to some other binary of the
same name rather than this shell.

When jsh is missing or a newer one is published, a toast offers the same action.
The check runs on a worker thread, never installs anything by itself, and stays
silent when it cannot reach the network:

```toml
jsh_update_check = "daily"    # "startup" every launch, "daily" cached, "never" off
```

`daily` reuses the installer's own cache (`~/.cache/jsh/update-check.json`), so
several jterms open at once still cost one request a day.

## Configuration

The configuration file is:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/config.toml
```

Create the file safely with `jterm1 --init-config`, or start from
[`config.toml.example`](config.toml.example). The command refuses to overwrite
an existing file. The application watches the file: appearance, scrollback, key
bindings, and defaults for newly created panes are reloaded while it is running.
Some advanced options are captured when a pane is constructed, so restart
jterm1 after changing them for predictable results. Changing `terminal_mode`
affects new or restored local panes; it does not replace an existing terminal
backend in place. Managed remote sessions stay on Block so their shell
integration and reconnect metadata remain available.

`-c PATH` / `--config PATH` selects an alternate file for the current process
and can be combined with a normal launch, `--doctor`, `--check-config`,
`--config-path`, `--init-config`, or config-backup recovery. `JTERM1_CONFIG`
provides the same process-local override. Separately,
`jterm1 --check-config PATH [--json]` validates exactly that file without
changing `JTERM1_CONFIG` or the active path.

Built-in theme names are `default`, `light`, `solarized-dark`,
`solarized-light`, `gruvbox-dark`, `gruvbox-light`, `dracula`, and `nord`.
The Settings dialog exposes appearance, scrollback, terminal backend, block
density, command history, AI/Agent, notifications, and the OSC 52 clipboard
policy. Advanced rendering, remote, and keybinding options remain TOML-only.

The following environment variables override selected TOML values:

```text
JTERM1_MODE                 JTERM1_SHELL
JTERM1_CONFIG
JTERM1_THEME                JTERM1_FONT
JTERM1_FONT_SCALE           JTERM1_OPACITY
JTERM1_SCROLLBACK           JTERM1_HISTORY_PATH
JTERM1_COMMAND_HISTORY_PATH
JTERM1_TAB_PLACEMENT        JTERM1_BLOCK_COMPACT
JTERM1_FG / BG / CURSOR / CURSOR_FG
```

Advanced block-rendering and history tuning keys are documented in
`config.toml.example`.

### Configuration integrity

Every window records the exact bytes of the configuration it loaded. In-app
settings saves acquire an advisory process lock and compare that revision with the
current file before writing. If another jterm1 window or editor changed the file,
the stale writer is rejected instead of overwriting newer work; the file watcher
then reloads the newer version and the user can reapply the setting.

Successful saves use a unique sibling temporary file, `fsync`, atomic rename, and
a directory sync. Two known-good states rotate through `config.toml.bak` and
`config.toml.bak.1`. Invalid TOML or schema errors are never overwritten by the UI.
The lock anchor is `config.toml.lock`; it contains no configuration data and may
remain on disk while unlocked.

### Workflows

Workflow files live in:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/workflows/*.{toml,yaml,yml}
```

Installed examples are read from
`${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/workflows/`; user workflows with
the same name take precedence. `JTERM1_WORKFLOW_DIR` adds higher-priority search
paths after the user config directory and before user/system data examples.
Workflow names are deduplicated in that directory-precedence order. Tags,
optional `shell`, and source-file metadata are retained. Workflows are reloaded
whenever the palette opens. A minimal shared YAML workflow is:

```yaml
name: "Search text"
description: "Search recursively with ripgrep"
command: "rg {{pattern}} {{path}}"
tags: [search]
args:
  - name: pattern
    description: "Pattern to find"
    default: "TODO"
  - name: path
    description: "Directory to search"
    default: "."
```

Press `Ctrl+Shift+M` or type `:` in the palette. Rendering a workflow inserts
the command at the prompt; it does not press Enter. Both `{name}` and
`{{name}}` placeholders are accepted, including Unicode names. Double braces
without a matching argument produce literal braces, such as `{{a,b}}` →
`{a,b}`. Rendered commands containing line breaks or terminal control
characters are rejected by the shared review-only input boundary.
Each workflow file is limited to 256 KiB, directory/file/argument/tag counts
are capped, and rendered commands are limited to 64 KiB before insertion.
Special files are rejected without blocking. Display metadata and command
values containing control, invisible, or bidirectional formatting characters
are rejected so the palette cannot present a visually reordered command.

### Remote hosts

The example config starts with an explicit empty remote list. To add a host,
remove `remote_hosts = []` and add one or more `[[remote_hosts]]` tables:

```toml
[[remote_hosts]]
name = "staging"
host = "server.example.com"
user = "deploy"
remote_shell = "bash -l"
ssh_args = ["-p", "22"]
login_shell = true
multiplex = true
```

jterm1 runs `ssh -t`, passes `ssh_args` before the target, and optionally uses
OpenSSH ControlMaster sockets. The custom `jsh` remote shell additionally
supports stable session IDs and block-aware reconnection; a regular remote
shell works as a normal interactive SSH tab.

#### Hosts that do not have jsh

Blocks, cwd tracking, exit codes, and the Commands timeline all come from jsh,
so a host with only `bash` on it opens as a plain terminal tab. `deploy` closes
that without anyone installing anything on the far side:

```toml
[[remote_hosts]]
name = "build-box"
host = "build.example.com"
deploy = "persist"      # "off" (default), "persist", or "incognito"
```

The tab then runs `jsh-remote.sh`, which places a verified static jsh on the
destination for the life of the session and removes it afterwards. Nothing edits
the destination's `.bashrc`, `.profile`, or login shell, and nothing needs root.

`persist` lets jsh keep its own dot-files and a cached binary in that account's
`$HOME`, so history survives and later tabs skip the transfer. `incognito`
sandboxes `HOME` for the session and deletes it on exit, which is what a shared
account needs. `remote_shell` is ignored when `deploy` is on — choosing the
shell is the launcher's job. A spelling neither this build nor the launcher
understands is a config error rather than a silent fall back to `off`, because
the difference between the modes is whether the destination gets written to.

#### Containers

`docker = true` makes `host` the name of a **running** container and the tab
connects with `docker exec` instead of ssh. `user` becomes the user inside the
container, and `deploy` works exactly as it does over ssh:

```toml
[[remote_hosts]]
name = "service"
host = "my-service"
docker = true
user = "devuser"
deploy = "persist"
```

`ssh_args`, `multiplex`, and `login_shell` mean nothing to a container and are
ignored. Containers run as root unless told otherwise, and a jsh older than the
"root shell trusts the system helpers it could write" fix refuses `/usr/bin/git`
and `/usr/bin/bash` as untrusted helpers when euid is 0 — Git completion, the
Git prompt, and the `.bashrc` import all disappear inside the container while
working locally. Pair container tabs with a jsh that carries that fix.

### AI

AI surfaces are optional and can be hidden with `ai_enabled = false`. Provider
selection is `JTERM1_AI_PROVIDER`, then the `ai_provider` setting, then the
Anthropic default. Accepted values are `anthropic`, `openai-compatible` (or
`openai`), and `ollama`. `JTERM1_AI_MODEL` and `JTERM1_AI_BASE_URL` override the
matching TOML settings and provider defaults. `JTERM1_AI_API_KEY` overrides the
provider-specific `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `OLLAMA_API_KEY`.
Keep API keys out of the TOML file: either in the environment or in a private
key file. The Settings dialog's **API Key** row stores a pasted key as a
0600 single-line file — `ai_api_key_file` if configured, otherwise
`~/.config/jterm1/ai.key` — and `JTERM1_AI_API_KEY_FILE` overrides the
configured path without ever being written back to config.toml.

AI network calls happen only after an explicit AI action. The session panel
keeps a multi-turn role history until Clear is pressed and can optionally attach
recent shell history. `Ctrl+Shift+Q`, or **Ask AI About Block** in a finished
block's context menu, starts a conversation with a bounded command/output
snapshot. `?` command generation inserts a command for review without executing
it. The agent proposes one command per turn and only runs it after the user
presses Approve; approval submits the command immediately. Approval is bound to
the current clean prompt, the exact reviewed command, a one-shot local
generation, and the shell integration's matching OSC 133 start/completion ID.
If any of those change or the terminal write fails, the proposal is cancelled
instead of attributing a different command's result to the Agent.

### Notebooks

Activating a `.jtnb.md` file in the sidebar opens the notebook viewer. Markdown
is intentionally minimal. Unlabelled, `shell`, `bash`, `sh`, `zsh`, `fish`,
`pwsh`, and `powershell` code fences get Run, Stop, and Copy controls. Unlabelled
and `shell` cells use jterm1's configured shell (with the same safe fallback as
new terminal panes); explicit fences use the named interpreter. Run All executes
runnable cells sequentially, while Stop All
terminates the current run and clears its queue. Every run gets a separate Unix
process group, so Stop, Stop All, and closing the viewer terminate descendants
as well as the interpreter. stdout and stderr remain separate, combined output
is bounded to 256 KiB, and cells run in the notebook's directory, isolated from
the active terminal but not sandboxed from the system.
Notebook files are regular UTF-8 files capped at 1 MiB; segment and executable
cell counts are capped independently. A cell larger than 256 KiB or containing
hidden/bidirectional or unsafe control characters remains visible (with unsafe
characters marked) and copyable, but its Run action is disabled.

The installer provides a walkthrough at:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/notebooks/welcome.jtnb.md
```

## State and diagnostics

jterm1 writes one session snapshot and companion lifetime owner lock per
process:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/tabs.<uuid>.state
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/tabs.<uuid>.lock
```

It records tab titles, pane layout, working directories, and restorable
foreground commands. On startup, jterm1 leaves live windows untouched and
atomically claims the newest valid snapshot whose owner lock is no longer held.
Snapshot publication and cleanup share a directory protocol lock so a
partially published owner cannot be mistaken for an exited process. The legacy
`tabs.state` and `tabs.<pid>.state` names are still accepted; PID snapshots are
retained conservatively when their owner cannot be proven dead. Corrupt
snapshots are retained for inspection inside the 32-snapshot recovery window
instead of being deleted on parse failure. A versioned replacement records the
snapshot it supersedes, so a crash between publishing the new checkpoint and
cleaning the old claim cannot revive stale state; closing the final tab writes
a durable empty tombstone for the same reason. Restore and save both enforce a
4 MiB payload budget, at most 32 tabs, 16 panes per tab and 64 panes total, plus
bounded argument counts and lengths for recognized replayable commands. The
state directory is owner-only (`0700`), and snapshots and locks are `0600`.
Inspect the newest snapshot with:

```bash
./scripts/show-state.sh
./scripts/debug.sh info
```

Logging accepts plain levels and target-specific directives. `JTERM1_LOG`
takes precedence over the standard `RUST_LOG` variable:

```bash
JTERM1_LOG=debug jterm1
RUST_LOG='warn,jterm1=debug,jterm1::session=trace' jterm1
```

Configuration recovery files live beside `config.toml`: two rotating backups
(`config.toml.bak` and `config.toml.bak.1`) plus `config.toml.before-restore`
when the recovery command replaces a live file. They are private user files and
can contain the same paths or remote profiles as the main configuration.

Command-only history is enabled by default at
`${XDG_STATE_HOME:-$HOME/.local/state}/jterm1/history.jsonl`. It stores the
command, working directory, exit status, and completion time, but not terminal
output. Set `command_history_enabled = false` to disable it or configure an
absolute `command_history_path`. Reads, appends, compaction, and shutdown flush
are size-bounded and serialized across processes; malformed or unterminated
records are skipped without allowing unbounded allocation.

Each launch is an independent application instance with its own snapshot, so
concurrent windows no longer overwrite one another. Session restoration is
still recovery state rather than durable project storage.

## Security and privacy

- Shell commands have the same permissions as jterm1. Review commands inserted
  by workflows, history, notebooks, remote profiles, or AI before running them.
- `startup_commands` executes automatically in every new tab. Keep it limited
  to commands you trust.
- Session restoration can replay recognized foreground commands such as SSH or
  container sessions. Remove saved snapshots with
  `scripts/debug.sh clean-state` if that is not desired.
- OSC 52 clipboard writes are disabled by default. Enable
  `allow_remote_clipboard_write` only for trusted local and remote programs.
- Long-command notifications can expose the first line of a command on the
  desktop notification surface; set `notify_long_blocks = false` on shared or
  locked-down desktops.
- Configuration backups, session state, command history, and block history may
  contain remote profiles, commands, output, and local paths. Protect or delete
  those files before sharing diagnostics. `--check-config` and the support bundle
  report only schema issues and metadata, never configuration values.
- Shell integration modifies prompt/pre-exec hooks. Review the script and test
  it with heavily customized shell frameworks.
- AI context leaves the machine when a cloud provider is selected. Use Ollama
  or disable AI when terminal content must remain local.

## Repository layout

```text
src/                         application and terminal backends
scripts/shell-integration/   OSC 133 / OSC 7 shell hooks
scripts/workflows/           example workflow definitions
scripts/notebooks/           example runnable notebook
packaging/                    desktop integration
```

Use `cargo fmt`, `cargo clippy --all-targets`, and `cargo test --all-targets`
before submitting changes.

## License

jterm1 is dual-licensed under **MIT OR Apache-2.0**; pick either at your option.
Full texts are in [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE). Contributions are accepted under the same
dual terms.
