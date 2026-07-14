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
- Command palette, command-history picker, parameterized YAML workflows, and
  fuzzy search
- Search within terminal output, block selection, output filtering, bookmarks,
  copy/rerun controls, and long-command notifications
- SSH host picker, connection status, multiplexing, and reconnect support
- Optional AI command generation, error explanation, session Q&A, and a
  multi-turn command agent with explicit approval before execution
- Runnable Markdown notebooks (`.jtnb.md`) with isolated Bash cells
- Live appearance settings and a hot-reloaded TOML configuration

## Requirements

jterm1 targets a graphical Linux desktop running X11 or Wayland. Building it
requires [Nix with flakes enabled](https://nixos.org/download/); the repository
flake supplies Rust, GTK 4, libadwaita, VTE, and the native build dependencies.

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
```

The installer builds a release binary and installs only user-local files:

- `~/.local/bin/jterm1`
- `${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/config.toml`
- `${XDG_DATA_HOME:-$HOME/.local/share}/applications/app.jterm1.desktop`
- shell integration and examples under
  `${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/`
- sample workflows under
  `${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/workflows/`

It never replaces an existing `config.toml`; installed examples live outside
the user-authored workflow directory. Make sure `~/.local/bin` is in `PATH`,
then run:

```bash
jterm1
jterm1 --doctor
```

Useful headless commands:

```bash
jterm1 --help
jterm1 --version
jterm1 --init-config                 # create config without overwriting one
jterm1 --shell-integration bash      # print an integration script
jterm1 --mode vte --no-restore       # launch a fresh compatibility session
jterm1 -d /path/to/project           # launch in a directory
jterm1 -e bash -lc 'printf "hello\n"'
```

For development:

```bash
make run       # debug build and launch
make test      # cargo test --all-targets
make check     # cargo check --all-targets
make build     # release build
make help      # all helpers
```

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
`rsh` when it is executable on `PATH`, `bash -l`, then `sh`.

## Default shortcuts

Shortcuts are captured at the window level unless noted otherwise. They can be
overridden in `[keybindings]`; the command palette displays the bindings that
are currently active.

| Shortcut | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close the focused pane, or the tab when it has one pane |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| `Ctrl+Alt+Shift+C` | In block mode, copy the selected block's output only |
| `Ctrl+Shift+E` / `Ctrl+Shift+D` | Split horizontally / vertically |
| `Ctrl+.` / `Ctrl+,` | Cycle pane focus forward / backward |
| `Ctrl+Shift+Left/Right` | Focus pane left / right |
| `Ctrl+Alt+Shift+Up/Down` | Focus pane up / down |
| `Ctrl+Alt+Arrow` | Resize the active split |
| `Ctrl+Shift+Z` | Toggle pane zoom |
| `Ctrl+Shift+!` | Move the focused pane into a new tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+PageDown/PageUp` | Next / previous tab |
| `Ctrl+0` | First tab |
| `Ctrl+1` ... `Ctrl+8` | Second ... ninth tab |
| `Ctrl+9` | Last tab |
| `Ctrl+Shift+P` | Command palette |
| `Ctrl+R` | History palette; this intentionally replaces shell reverse search |
| `Ctrl+Shift+F` | Search terminal output (`/pattern/` enables regex) |
| `Ctrl+Shift+O` | Settings |
| `Ctrl+\` | Toggle sidebar |
| `Ctrl+Alt+B` | Move tabs between sidebar and top bar |
| `Ctrl+Shift+L` | Focus the tab filter |
| `Ctrl+Shift+R` | Remote host picker |
| `Ctrl+Shift+X/S/M` | Jump to the first failed / slow / pinned block |
| `Alt+Up/Down` | Previous / next pinned block |
| `Ctrl+Shift+N` | Jump to the oldest block |
| `Ctrl+Shift+A` | Select all finished blocks |
| `Ctrl+Shift+I` | Reinput selected block commands without running them |
| `Ctrl+Shift+K` | Clear all finished blocks in the pane |
| `Ctrl+Alt+Shift+A` | Session AI panel |
| `Ctrl+Shift+Y` | Workflows |
| `Ctrl+Shift+G` | AI agent |
| `F12` | Debug dashboard |
| `Ctrl+Shift++` | Increase font scale |
| `Ctrl+-` | Decrease font scale |
| `Ctrl+Shift+J` / `Ctrl+Alt+Shift+K` | Decrease / increase window opacity |
| `Ctrl+Up/Down` | Scroll terminal output |

Block mode also has context-sensitive navigation:

- `Home` / `End` and `PageUp` / `PageDown` navigate completed output while no
  command or full-screen application owns the viewport.
- With one or more blocks selected, `Up` / `Down` moves the active edge,
  `Shift+Up/Down` extends the range, `Enter` recalls every selected command in
  terminal order without running it, and `Escape` clears the selection.
- `Ctrl+Shift+B` bookmarks the selected block.
- `Alt+Shift+F` toggles the selected or most recent block's output filter.
- `Ctrl+P` opens block mode's in-memory command-history picker.

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
affects new or restored panes; it does not replace an existing terminal backend
in place.

Built-in theme names are `default`, `light`, `solarized-dark`,
`solarized-light`, `gruvbox-dark`, `gruvbox-light`, `dracula`, and `nord`.
The Settings dialog exposes appearance, scrollback, terminal backend, block
density, command history, AI/Agent, notifications, and the OSC 52 clipboard
policy. Advanced rendering, remote, and keybinding options remain TOML-only.

The following environment variables override selected TOML values:

```text
JTERM1_MODE                 JTERM1_SHELL
JTERM1_THEME                JTERM1_FONT
JTERM1_FONT_SCALE           JTERM1_OPACITY
JTERM1_SCROLLBACK           JTERM1_HISTORY_PATH
JTERM1_COMMAND_HISTORY_PATH
JTERM1_TAB_PLACEMENT        JTERM1_BLOCK_COMPACT
JTERM1_FG / BG / CURSOR / CURSOR_FG
```

Advanced block-rendering and history tuning keys are documented in
`config.toml.example`.

### Workflows

Workflow files live in:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/workflows/*.yaml
```

Installed examples are read from
`${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/workflows/`; user workflows with
the same name take precedence. `JTERM1_WORKFLOW_DIR` can add more search paths.
Workflows are reloaded whenever the palette opens. A minimal workflow is:

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

Press `Ctrl+Shift+Y` or type `:` in the palette. Rendering a workflow inserts
the command at the prompt; it does not press Enter.

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
OpenSSH ControlMaster sockets. The custom `rsh` remote shell additionally
supports stable session IDs and block-aware reconnection; a regular remote
shell works as a normal interactive SSH tab.

### AI

AI surfaces are optional and can be hidden with `ai_enabled = false`. Provider
selection is:

1. `JTERM1_AI_PROVIDER=anthropic|openai|ollama`
2. `ANTHROPIC_API_KEY`
3. `OPENAI_API_KEY`
4. local Ollama at `http://localhost:11434`

`JTERM1_AI_MODEL` and `JTERM1_AI_BASE_URL` override the provider defaults.
Keep API keys in the environment rather than the TOML file.

AI network calls happen only after an explicit AI action. Depending on the
surface, the request can include the current directory, command text, and a
bounded sample of terminal output. `?` command generation inserts a command for
review without executing it. The agent proposes one command per turn and only
runs it after the user presses Approve; approval submits the command immediately.

### Notebooks

Activating a `.jtnb.md` file in the sidebar opens the notebook viewer. Markdown
is intentionally minimal. `bash`, `sh`, and `shell` code fences get Run, Stop,
and Copy controls; Run executes the cell with `bash -c` in the notebook's
directory, isolated from the active terminal but not sandboxed from the system.

The installer provides a walkthrough at:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/jterm1/notebooks/welcome.jtnb.md
```

## State and diagnostics

jterm1 writes one session snapshot per process:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/jterm1/tabs.<pid>.state
```

It records tab titles, pane layout, working directories, and restorable
foreground commands. On startup, jterm1 leaves live windows untouched and
atomically claims the newest valid snapshot owned by an exited process. The
legacy `tabs.state` name is still accepted. Corrupt snapshots are retained for
inspection instead of being deleted. Inspect the newest snapshot with:

```bash
./scripts/show-state.sh
./scripts/debug.sh info
```

Command-only history is enabled by default at
`${XDG_STATE_HOME:-$HOME/.local/state}/jterm1/history.jsonl`. It stores the
command, working directory, exit status, and completion time, but not terminal
output. Set `command_history_enabled = false` to disable it or configure an
absolute `command_history_path`.

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
- Session state, command history, and block history may contain commands,
  output, and local paths. Protect or delete those files before sharing
  diagnostics.
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
