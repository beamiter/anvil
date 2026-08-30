# anvil

anvil is an experimental Linux terminal emulator built with Rust, GTK 4,
libadwaita, Relm4, and VTE. It can behave like a conventional VTE terminal or
turn completed commands into navigable blocks with their command, output, exit
status, duration, and working directory.

The project is currently at version `0.2.0`. Treat it as an early-stage daily
driver: keep backups of important configuration and do not rely on session
restoration as the only copy of work in progress.

## Highlights

- VTE and block-aware terminal modes
- Tabs, backend-inheriting nested split panes, pane zoom, directional focus,
  and session restore
- Drag a single-pane tab to any edge of a target pane to merge it as a split;
  drag a split-pane header back to the tab bar to promote it to a normal tab.
  A short tab hover previews the target page, while center/cancel drops leave
  every existing PTY untouched
- Sidebar tab list and a lazy file tree with byte-exact Linux path identity;
  files are safely shell-quoted before being inserted at the prompt, while a
  non-UTF-8 path is shown unambiguously and never rewritten into another name
- Command palette, command-history picker, parameterized TOML/YAML workflows, and
  fuzzy search
- Search within terminal output with match counts, previous/next controls and
  visible regex errors, plus block selection, output filtering, bookmarks,
  copy/rerun controls, and long-command notifications
- SSH host picker, connection status, multiplexing, and reconnect support
- Optional persistent multi-chat AI workspace, inline command generation,
  review-first correction, and a multi-turn Shell Agent with explicit approval
  before execution
- Optional local, no-LLM ASCII organism with durable bounded memory and
  desktop-aware motion modes
- Runnable Markdown notebooks (`.jtnb.md`) with isolated multi-shell cells
- Live appearance settings and a hot-reloaded TOML configuration

## Requirements

anvil targets a graphical Linux desktop running X11 or Wayland. The source
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
git clone https://github.com/beamiter/anvil.git
cd anvil
./scripts/install.sh
./scripts/install.sh --backend cargo
./scripts/install.sh --binary /path/to/anvil
./scripts/install.sh --prefix /opt/anvil --data-dir /opt/anvil/share
./scripts/install.sh --dry-run
```

The installer supports `DESTDIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
`CARGO_TARGET_DIR`; it never overwrites an existing configuration. By default
it builds a release binary and installs only user-local files:

- `~/.local/bin/anvil`
- `${XDG_CONFIG_HOME:-$HOME/.config}/anvil/config.toml`
- `${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.beamiter.anvil.desktop`
- icons under
  `${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/{scalable,128x128,256x256}/apps/`
  and AppStream metadata under `.../metainfo/`
- shell integration and examples under
  `${XDG_DATA_HOME:-$HOME/.local/share}/anvil/`
- sample workflows under
  `${XDG_DATA_HOME:-$HOME/.local/share}/anvil/workflows/`

`--binary` skips the Rust toolchain for release archives and distro staging.
Its input must be a readable, non-symlink regular file. Bash cannot make the
initial no-follow check and open atomic. Only after the open succeeds and GNU
`stat` verifies that the pathname and Linux `/proc/self/fd` descriptor identify
the same inode is a later pathname replacement unable to change what gets
copied. `mktemp` plus GNU `mv -T` then replaces the destination atomically on
the same filesystem. Errors and exits clean up the private temporary file and
retain the old executable until that rename commits the binary. The rename is
the commit point: a later asset/config failure does not roll the binary back,
and EXIT cleanup only removes a still-uncommitted temporary.
Support tooling, shell integrations, the frozen workflow manifest, notebooks,
AppStream metadata, and icons use the same same-directory temp/rename commit
discipline with explicit public modes. Every source is preflighted before the
build or first write. Initial config creation uses a same-directory temporary
plus a no-clobber hard link; a concurrent creator wins and is retained.
Zero-byte `--binary` inputs and explicit `--backend` plus `--binary` are
rejected. A non-root caller-controlled `DESTDIR` is first normalized by
collapsing repeated separators and lexical `.` components, then every existing
component from `/` through the staging root is checked for symlinks before an
install write or uninstall removal. Recursive purge roots are checked before
ordinary files; normal host prefixes are unchanged. This checks existing state
and does not promise safety against a concurrent path replacement after the
check. Runtime and staging paths must be absolute and may contain spaces or
Unicode, but control characters and lexical `..` components are rejected.
Run `bash scripts/test-install-paths.sh` for the private DESTDIR contract suite.

That desktop integration is what makes anvil appear in the GNOME/KDE
application list with its own icon, ready to pin. Two details matter for it to
show up at all, and the installer handles both:

- `Exec=`/`TryExec=` are rewritten to the binary's absolute path (system
  prefixes such as `/usr` keep the relocatable bare name). A desktop session
  fixes its `PATH` at login, so `TryExec=anvil` fails and hides the entry
  **completely** when `~/.local/bin` is not on that `PATH` — the usual reason an
  install produces no launcher icon. Spaces, backslashes, `$`, quotes and
  backticks are encoded according to the Desktop Entry layers; ambiguous `=`
  and `%` executable paths are rejected with a diagnostic.
- `update-desktop-database` and `gtk-update-icon-cache` are refreshed after
  install and uninstall (a stale icon cache shadows newly installed icons).
  `DESTDIR` builds skip the refresh and leave it to the package manager.

Verify with `desktop-file-validate <entry>` and `gtk-launch
io.github.beamiter.anvil`; use `--no-desktop` to install only the binary.

It never replaces an existing `config.toml`; installed examples live outside
the user-authored workflow directory. Make sure `~/.local/bin` is in `PATH`,
then run:

```bash
anvil
anvil --doctor
anvil --doctor --json            # machine-readable support diagnostics
anvil --check-config             # validate config without exposing its values
anvil --check-config ~/test.toml # validate one file without changing the active path
anvil -c ~/test.toml --doctor    # use one alternate config for this process
anvil --config-path              # print the active config path
anvil --safe-mode                # isolated VTE + sh recovery session
```

Useful headless commands:

```bash
anvil --help
anvil --version
anvil --init-config                 # create config without overwriting one
anvil --check-config --json         # machine-readable schema validation
anvil --config ~/test.toml --config-path # print an explicit effective path
anvil --restore-config-backup       # restore newest valid rotating backup
anvil --shell-integration bash      # print an integration script
anvil --mode vte --no-restore       # launch a fresh compatibility session
anvil -d /path/to/project           # launch in a directory
anvil -e bash -lc 'printf "hello\n"'
```

Source installs made by anvil versions that defaulted to
`~/.cargo/bin/anvil` are not removed automatically. After verifying the new
`~/.local/bin/anvil`, remove that legacy copy manually if it would otherwise
shadow the new binary on `PATH`.

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
make test-display # isolated GTK/VTE regressions under Xvfb
make check     # cargo check --all-targets
make build     # release build
make clippy    # repository lint policy
make security  # dependency audit + ShellCheck
make verify    # full tests, Xvfb regressions, docs, release, script contracts
make help      # all helpers
```

`make package` deliberately uses the host Cargo and system GTK/VTE development
libraries. The relocatable archive targets a compatible system-library ABI;
building its bare binary inside Nix would bake `/nix/store` paths into it.

## Diagnostics and recovery

`anvil --doctor` reports configuration, shell, display, integrations, remote
readiness, permissions, and session-state metadata. Add `--json` for automation
or support tooling; neither format includes configuration contents, terminal
history, command output, environment values, or credentials. An ordinary run
may include local paths and the bounded parser reason for the first rejected
workflow file (which can quote part of that file); support-bundle mode redacts
both while retaining the counts. Paths shown by Doctor are bounded and render
terminal controls, newlines, and bidirectional formatting as inert text.

When configuration, startup commands, session restore, or an integration causes
a bad launch, use:

```bash
anvil --safe-mode
```

Safe mode starts a local VTE pane with `sh`, skips session restore and persistence,
ignores configured startup commands and remote hosts, disables AI, notifications,
repository probes, history, remote clipboard writes, and jsh update/install
operations, and refuses to save or hot-reload settings for that process.

Create a privacy-preserving support archive with:

```bash
anvil-support-bundle ~/Desktop
```

Review the archive before sharing it. The bundle contains structured diagnostics,
system identity, linked-library information, and file metadata only.

Validate configuration keys, types, ranges, colors, shortcuts, and remote-host
records without starting GTK:

```bash
anvil --check-config
anvil --check-config --json
```

The report names keys and problems but never includes configuration values. If a
bad edit or interrupted recovery leaves the live file unusable, restore the newest
valid rotating backup with:

```bash
anvil --restore-config-backup
```

The command preserves the replaced live file as `config.toml.before-restore`.

## Terminal modes

Block mode is the default. It keeps a live VTE input cell at the bottom and
promotes each finished command into a separate block:

The total-history scrollbar adds short theme-red marks at the approximate
positions of failed completed commands. Block cards, marker/navigation actions,
and exact-exit filters all use `jterm_core::block_contract` after anvil has
resolved OSC metadata against its bounded screen capture. Background output —
even a legacy row carrying a nonzero raw status — and commands without a
reported status are therefore not presented as failures.

```toml
terminal_mode = "block"
```

A fresh Block pane with no completed or restored cards shows one accessible,
one-shot orientation card: completed commands become reusable cards; click a
card header to select it, right-click for more actions, and press
`Ctrl+Shift+G` to search. The card retires permanently after the first accepted
human input, completion, or restored history, even if clear, filtering, or
retention later leaves the pane empty. It is a non-targetable, non-measuring overlay, so it
neither consumes live PTY rows nor competes with shell-integration or AI
notices. It temporarily hides while an alternate-screen program owns the
surface and returns afterward, so it never covers the first TUI. Unified and
conventional VTE panes never show it.

Cross-block search combines `Aa` (case sensitive), `.*` (regular expression),
and `W` (Unicode whole word) controls. `Ctrl+I` / `Ctrl+R` / `Ctrl+W` toggle
them while the query keeps focus; the result scan and the VTE highlight used
after activation share the exact same options. `All / Cmd / Out` restricts the
scan to all text, commands, or output, with `Ctrl+O` cycling the scope before
the 500-hit cap is applied. `Failed`, `Slow`, `Bookmarked`, and `Background` compose before
that cap; with an empty text query, any filter turns the palette into a metadata
browser with one representative row for each eligible retained block that has
meaningful text on the selected surface. Queries above 8 KiB are rejected
before regex compilation, whose heap budget is capped independently. Result
status includes the current position; `↑/↓` wraps, `Home/End` selects either
edge, and `PageUp/PageDown` moves ten rows while keeping the row visible and
the query focused. `Enter` jumps and closes; `Shift+Enter` keeps the palette
open and advances only after a successful live-terminal jump. Snapshot-only
hits still open their snapshot, while unavailable hits stay selected with a
diagnostic instead of fake-stepping.
Reopening restores the last valid query, matching controls, scope, and all
four metadata filters for this pane's process lifetime only; nothing is written
to config or session snapshots. `Ctrl+U` clears only the query; **Reset** or
`Ctrl+Shift+U` restores the query, matching controls, scope, and all filters
to defaults. An invalid query above 8 KiB is never remembered, and activating
any control with the pointer returns focus to the query for uninterrupted typing.
While open, a 500 ms identity-and-bookmark-revision probe detects completed-block
additions, same-length retention rotation, and bookmark changes, then refreshes
through the existing debounce.
The exact selected hit remains selected when it survives; the probe never clones
command or output text.
Block Search 3.8 keeps the closest surviving old rank when retention removes
that exact hit, avoiding a jump to the first row. Query, matching, scope, or
metadata-filter edits remain new intent and deliberately restart at the top.
Block Search 3.9 adds a pointer-accessible refresh button to the dialog header.
The button and unmodified `F5` share one selection-preserving rebuild
that synchronizes the identity probe so the automatic timer cannot enqueue a
duplicate refresh; modified F5 chords pass through unchanged. The button exposes
its full action name and `F5` shortcut to accessibility clients. Key auto-repeat
is latched, so one physical unmodified F5 press performs at most one rebuild;
pressing F5 with a modifier and then releasing that modifier while F5 remains
held cannot accidentally refresh. Leaving the dialog focus domain clears the
latch, so a window-manager focus change cannot strand F5 after a lost release.
Block Search 4.0 keeps a held opening toggle from immediately closing the new
dialog: the window capture layer remembers the opener's physical keycode before
the asynchronous action dispatch, consumes its repeats even if modifiers are
released mid-hold, and clears the guard on physical release or window
deactivation. Fresh presses still toggle normally, including user-remapped
`cross_block_search` bindings. Refresh/Reset remain in the title bar while
scope/matching and metadata controls occupy two compact content rows. Each row
automatically scrolls horizontally when the active theme, font, or window width
cannot fit every control, preserving their keyboard order and reachability. A
manual refresh now exposes `Refreshing blocks…` as an accessible status for
one drawable frame before the synchronous bounded rebuild; a generation check
prevents that deferred edge from reviving stale query intent or a closed dialog.
The pending frame callback is explicitly replaced or removed on another search
intent, refresh, or close, so a closed widget without a frame clock cannot keep
the dialog graph alive. A dialog also retains its singleton slot through the
close animation; toggles during that transition keep closing the same instance,
and only its `closed` signal permits the next one to open.
Block Search 4.1 adds `Background` as a composable metadata condition backed by
the completed record's real commandless-output identity in both Block and
Unified modes. Background records have no command lifecycle, so they never
match Failed, exact-exit, Slow, or duration-bound predicates even if defensive
legacy input carries contradictory fields; result rows likewise suppress those
raw exit and duration values. Empty-query browsing produces only
rows backed by retained output: `All` and `Out` use the first meaningful output
line, while `Cmd` and records without retained output produce no synthetic hit.
Bookmarked search is pane-local and runtime-only in both Block and Unified modes.
Use the visible star on any result or `Ctrl+Shift+B` on the selected result;
`Bookmarked` composes with the other metadata filters before scope and the
500-hit cap. Unified bookmarks are removed only when their completed record is
retired, not when bounded output snapshots or visual chrome are discarded.

Unified mode keeps one continuous VTE scrollback while retaining authenticated
command zones, status chrome, bounded per-zone output snapshots, search/export,
and session replay. Kitty `a=T` images use nonce-scoped row probes and remain
aligned while scrolling and rewrapping; placements outside the live grid are
rejected rather than silently resized:

```toml
terminal_mode = "unified"
```

The supported Kitty subset is direct, static `a=T` display (`i`, `c/r/C`,
PNG/RGB/RGBA) plus `a=q`; transmit-only storage, `I`, crop/z/relative placement,
delete and replacement return `ENOTSUP`. Unified honors cell placement. Block
mode retains its established finished-card attachment profile, so it does not
promise in-grid `c/r` placement or replacement semantics.

If a shell omits OSC 133 `D`, a new prompt recovers the record only after the
shell is confirmed as PTY foreground owner. Such a record is explicitly
`boundary_inferred` / degraded and carries no fabricated exit status, end time,
or duration. Replayed and unknown records remain distinguishable in Unified
exports and chrome.

Use the conventional VTE backend when compatibility with terminal applications
matters more than command blocks:

```toml
terminal_mode = "vte"
```

Block boundaries are most reliable when the shell emits OSC 133 command marks
and OSC 7 working-directory updates. The installer places the integration files
under:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/anvil/shell-integration/
```

Source the file for the current shell. Sourcing unconditionally works in both
anvil backends; the scripts protect against being loaded twice, and terminals
that do not understand the OSC sequences ignore them.

After installing anvil, Bash and Zsh can also load the script embedded in the
binary:

```bash
source <(anvil --shell-integration bash)
```

```bash
# ~/.bashrc
source "${XDG_DATA_HOME:-$HOME/.local/share}/anvil/shell-integration/anvil.bash"
```

```zsh
# ~/.zshrc
source "${XDG_DATA_HOME:-$HOME/.local/share}/anvil/shell-integration/anvil.zsh"
```

```fish
# ~/.config/fish/config.fish
set -l anvil_data_home "$HOME/.local/share"
if set -q XDG_DATA_HOME
    set anvil_data_home "$XDG_DATA_HOME"
end
source "$anvil_data_home/anvil/shell-integration/anvil.fish"
```

PowerShell users can dot-source `anvil.ps1`; its Enter hook requires
PSReadLine. More detail is in
[`scripts/shell-integration/README.md`](scripts/shell-integration/README.md).

CLI completions are embedded for the same four shells. For example, load Bash
completion for the current session with:

```bash
source <(anvil --generate-completion bash)
```

`--completion` is an alias for `--generate-completion`; use `zsh`, `fish`, or
`pwsh` to print those formats for installation in the shell's normal completion
directory.

Shell selection follows this order: `ANVIL_SHELL`, the `shell` config key,
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
| `Ctrl+Alt+Arrow` or `Ctrl+Alt+H/J/K/L` | Focus the pane in that direction |
| `Ctrl+Shift+Alt+Arrow` or `Ctrl+Alt+Shift+H/J/K/L` | Resize the active split |
| `Ctrl+Shift+Z` | Toggle pane zoom |
| `Ctrl+Shift+!` | Move the focused pane into a new tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+PageDown/PageUp` | Next / previous tab |
| `Ctrl+1` ... `Ctrl+8` | First ... eighth tab |
| `Ctrl+9` | Last tab |
| `Ctrl+Shift+P` | Unified command palette (actions, history, workflows, AI) |
| `Ctrl+Shift+H` | History palette; `Ctrl+R` and `Ctrl+P` remain available to the shell |
| `Ctrl+Shift+F` | Search terminal output with result count and previous/next controls (`/pattern/` enables regex) |
| `Ctrl+Shift+G` | In block mode, search command and output lines across all finished blocks |
| `Ctrl+Shift+O` | Settings |
| `Ctrl+Shift+R` | Reload configuration |
| `Ctrl+\` | Toggle sidebar |
| `Ctrl+Alt+B` | Move tabs between sidebar and top bar |
| `Ctrl+Shift+L` | Focus the tab filter |
| `Ctrl+Shift+S` | Remote host picker |
| `Ctrl+Shift+X` | Step to the next failed block, wrapping at the end |
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

- `Ctrl+Home` / `Ctrl+End` and `PageUp` / `PageDown` navigate completed output
  while no command or full-screen application owns the viewport. Bare `Home` /
  `End` belong to the shell, so they still move the cursor within the command
  being typed.
- With one or more blocks selected, `Up` / `Down` moves the active edge,
  `Shift+Up/Down` extends the range, `Home` / `End` moves the selection to the
  oldest / newest block, `Enter` recalls every selected command in terminal
  order without running it, and `Escape` clears the selection. These keys keep
  working after a finished command/output surface or card header takes focus;
  ordinary typing still returns safely to the live prompt through its IME.
- The selection hint reports how many cards are selected and distinguishes
  `Enter recall` from `Enter recall all`; it no longer makes a static
  prompt-readiness claim. A refused plain Enter is consumed, rings, and briefly
  shows the busy, dirty, unsupported-paste, or unsafe-command reason instead of
  reaching a dirty prompt or running program. Multiline/multi-card recall also
  requires bracketed-paste
  support; without it the whole action is refused rather than silently keeping
  only the first command.
- A visible text highlight always wins over whole-card copy, even if a card
  selection still exists. `Ctrl+Shift+C` aggregates selected command/output/live
  VTE text in document order; an oversized aggregation fails atomically instead
  of falling back to unrelated card content.
- All history insertion surfaces require lossless encoding. A multiline card is
  therefore left untouched when bracketed paste is unavailable instead of being
  reported as inserted after only its first line was written.
- `Ctrl+Enter` directly re-runs only one selected foreground card whose command
  is complete and single-line, and only at a verified empty, foreground-owned
  prompt with no pending Agent/review submission. A refused re-run consumes the
  chord, rings, and briefly shows the refusal reason instead of allowing VTE to
  submit unrelated prompt contents. An
  admitted re-run first inserts without Enter, waits for the exact stable VTE
  rendering, and sends CR only in the second phase. The
  active card's hint omits the action whenever the selection itself is not
  eligible.

Every history-recall entry point, including card actions and contextual insert,
uses the same verified empty-prompt guard, so it cannot erase or splice into an
existing edit. Alternate-screen ownership suppresses Block selection navigation
entirely, preventing a hidden selection from appearing after a TUI exits. Card
action strips that fade out are also insensitive and non-targetable, so Tab or
stale keyboard focus cannot activate an invisible action on an old card.
- A focused card-header button keeps ordinary GTK Return/Space activation;
  only the explicit Ctrl+Enter chord is delegated to Block re-run.
- `Ctrl+Shift+B` bookmarks the selected block, or the newest block when nothing
  is selected.
- `Ctrl+,` / `Ctrl+.` jumps to the previous / next bookmarked block. Until a
  block is bookmarked both chords reach the program in the terminal.
- `Alt+Shift+F` toggles the selected or most recent block's output filter.
- `Alt+Shift+O` folds or unfolds that block's output.

Jump-to-oldest failed/slow/pinned, previous-failed, and pinned-navigation
actions remain available in the command palette and can be assigned in
`[keybindings]`, but have no default shortcuts.

## Installing and updating jsh

anvil prefers its companion shell [`jsh`](https://github.com/beamiter/jsh) and
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
${XDG_CONFIG_HOME:-$HOME/.config}/anvil/config.toml
```

Create the file safely with `anvil --init-config`, or start from
[`config.toml.example`](config.toml.example). The command refuses to overwrite
an existing file. The application watches the file: appearance, scrollback, key
bindings, and defaults for newly created panes are reloaded while it is running.
Some advanced options are captured when a pane is constructed, so restart
anvil after changing them for predictable results. Changing `terminal_mode`
affects new tabs and restored leaves; it does not replace an existing terminal
backend in place. Splitting always inherits the focused pane's backend, so a
later setting change cannot turn one side of an existing Block/VTE layout into
a different backend. Managed remote sessions stay on Block so their shell
integration and reconnect metadata remain available.

`-c PATH` / `--config PATH` selects an alternate file for the current process
and can be combined with a normal launch, `--doctor`, `--check-config`,
`--config-path`, `--init-config`, or config-backup recovery. `ANVIL_CONFIG`
provides the same process-local override. Separately,
`anvil --check-config PATH [--json]` validates exactly that file without
changing `ANVIL_CONFIG` or the active path.

Built-in theme names are `default`, `light`, `solarized-dark`,
`solarized-light`, `gruvbox-dark`, `gruvbox-light`, `dracula`, and `nord`.
The Settings dialog exposes appearance, scrollback, terminal backend, block
density, command history, AI/Agent, notifications, and the OSC 52 clipboard
policy. Advanced rendering, remote, and keybinding options remain TOML-only.

The following environment variables override selected TOML values:

```text
ANVIL_MODE                 ANVIL_SHELL
ANVIL_CONFIG
ANVIL_THEME                ANVIL_FONT
ANVIL_FONT_SCALE           ANVIL_OPACITY
ANVIL_SCROLLBACK           ANVIL_HISTORY_PATH
ANVIL_COMMAND_HISTORY_PATH
ANVIL_TAB_PLACEMENT        ANVIL_BLOCK_COMPACT
ANVIL_AGENT_AUTO_APPROVE_READONLY
ANVIL_COMMAND_CORRECTION_ENABLED
ANVIL_FG / BG / CURSOR / CURSOR_FG
```

Advanced block-rendering and history tuning keys are documented in
`config.toml.example`.

### Configuration integrity

Every window records the exact bytes of the configuration it loaded. In-app
settings saves acquire an advisory process lock and compare that revision with the
current file before writing. If another anvil window or editor changed the file,
the stale writer is rejected instead of overwriting newer work; the file watcher
then reloads the newer version and the user can reapply the setting.

Successful saves use a unique sibling temporary file, `fsync`, atomic rename, and
a directory sync. Two known-good states rotate through `config.toml.bak` and
`config.toml.bak.1`. Invalid TOML or schema errors are never overwritten by the UI.
Configuration files may be owner- or system-managed read-only, but are rejected
when group or other users can write them; a writable config can select a shell
and startup commands, so anvil will not load or hot-reload it.
The lock anchor is `config.toml.lock`; it contains no configuration data and may
remain on disk while unlocked.

### Workflows

Workflow files live in:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/anvil/workflows/*.{toml,yaml,yml}
```

Installed examples are read from
`${XDG_DATA_HOME:-$HOME/.local/share}/anvil/workflows/`; user workflows with
the same name take precedence. `ANVIL_WORKFLOW_DIR` adds higher-priority search
paths after the user config directory and before user/system data examples.
Workflow names are deduplicated in that directory-precedence order. Tags,
optional `shell`, and source-file metadata are retained. Workflows are reloaded
whenever the palette opens.

The format is not anvil's. Discovery, both parsers, validation and the template
engine are `jterm_core::workflows`, shared with the sibling jterm terminals,
which read the same library out of the same directories — so a file that loads
here loads there, and means the same thing. A minimal shared YAML workflow is:

```yaml
name: "Search text"
description: "Search recursively with ripgrep"
command: "rg {{pattern}} {{path}}"
tags: [search]
args:
  - name: pattern
    description: "Pattern to find"
  - name: path
    description: "Directory to search"
    default: "."
```

Press `Ctrl+Shift+M` or type `:` in the palette. Rendering a workflow inserts
the command at the prompt; it does not press Enter.

#### Arguments with no default are required

**An argument the file gives no `default` for must be filled in.** Leaving its
field blank and pressing Insert reports `missing values: <names>` in the dialog
and inserts nothing — it does not substitute the empty string. In the example
above, `pattern` is required and `path` is not.

That distinction is what `default` is for, so it cuts both ways:

- No `default` key at all — required. Blank, or only whitespace, is *unfilled*.
- `default: ""` — an explicit empty value. The field starts empty, renders
  empty, and is never reported as missing.
- Any other `default` — the field starts at that value. Clearing it by hand is
  a deliberate empty value and renders as one; it does not spring back to the
  default. The dialog has no revert control, so reopen the workflow to get the
  declared default back.

Before this, every field was pre-filled with `""` and blank meant blank:
`kill -9 {pid}` with an untouched Pid field was inserted at the prompt as
`kill -9 `. If you have workflow files that relied on that, add
`default: ""` to the arguments you want to keep optional.

#### Placeholders

Both `{name}` and `{{name}}` placeholders are accepted, including Unicode
names. Names are trimmed, so `{{ service }}` binds the argument named
`service`; for the same reason a declared `name` may not have leading or
trailing whitespace, and a file that declares one is rejected rather than
loaded with a row that can never bind. Double braces with no matching argument
produce literal braces, such as `{{a,b}}` → `{a,b}`; an unmatched `{{` is left
exactly as authored, so `awk '{{print $1}' {{log}}` keeps its awk program and
substitutes only `log`.

#### What is refused, and how you find out

Symlinked workflow *files* are not loaded: anvil opens candidates with
`O_NOFOLLOW`. Symlinking the containing *directory* still works, and is the
supported way to keep a library in a dotfiles checkout.

```sh
ln -s ~/dotfiles/workflows ~/.config/anvil/workflows            # scanned
ln -s ~/dotfiles/deploy.toml ~/.config/anvil/workflows/deploy.toml  # refused
```

A symlinked file used to be followed, parsed, and turned into a palette entry
that gets typed at a prompt; the sibling terminals refused it. If this removes
a workflow you were using, move the file into the directory or link the
directory instead.

A file that is refused for any reason — a symlink, a parse error, a validation
error, a special file, an oversized file — is skipped without disabling the
rest of the library, and anvil raises a toast naming it the first time that
happens ("Workflow file skipped — *path*: *reason*", or "N workflow files
skipped, including …"). The toast repeats only when the set of refused files
changes, so a broken file you have not fixed yet does not nag on every palette
open. `anvil --doctor` reports the same count alongside how many search
locations were readable, and an ordinary (unredacted) run names the first
rejected file and its bounded reason. Support-bundle diagnostics retain the
counts but redact that local path and parser detail.

Rendered commands containing line breaks or terminal control characters are
rejected by the shared review-only input boundary.
Each workflow file is limited to 256 KiB, directory/file/argument/tag counts
are capped, and rendered commands are limited to 64 KiB before insertion.
Special files are rejected without blocking. Display metadata and command
values containing control, invisible, or bidirectional formatting characters
are rejected so the palette cannot present a visually reordered command — as
are file names and parse errors on their way to a log line or a toast, because
whoever can write to a scanned directory chooses both.

### Remote hosts

A config file with no `remote_hosts` key gets two worked entries to copy from —
one ssh destination and one container — because the two mistakes the grammar
cannot forgive are invisible in an empty list: the port belongs in `ssh_args`,
never as `host = "box:22"`, and the login belongs in `user`, never as
`host = "root@box"`. An explicit list wins, `remote_hosts = []` included, so
hosts deleted in the settings dialog stay deleted.

The dialog (Ctrl+Shift+O → Remote Hosts) adds, edits and removes entries; the
pencil loads a host into the form below, and the fields it has no widget for
(`ssh_args`, `session`, `remote_shell`, `login_shell`, `multiplex`,
`deploy_artifact`) are carried through an edit untouched. To write them, edit
one or more `[[remote_hosts]]` tables by hand:

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

anvil runs `ssh -t`, passes `ssh_args` before the target, and optionally uses
OpenSSH ControlMaster sockets. The custom `jsh` remote shell additionally
supports stable session IDs and block-aware reconnection; a regular remote
shell works as a normal interactive SSH tab.

The application repeats one host gate at the final argv boundary for fresh
connections, reconnects, workspace restore, and every remote-filesystem probe.
It enforces character and byte budgets, rejects visual-formatting spoofing,
checks the first-128 index boundary, and treats `ssh_args` as structured
OpenSSH options (`-p 22` and `-o Name=value` remain supported; a second bare
destination or premature `--` does not). Rejection is diagnostic and occurs
before a pane is replaced or a process is spawned.

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
ignored.

`deploy_artifact` names a jsh built on this machine to push, instead of the
published release `deploy` would otherwise fetch:

```toml
[[remote_hosts]]
name = "service"
host = "my-service"
docker = true
deploy = "incognito"
deploy_artifact = "/home/you/jsh/target/x86_64-unknown-linux-musl/release/jsh"
```

Usually unnecessary: when the local jsh is a static build — which a Linux
install now is — the launcher lends it automatically, with no release lookup
and no network, and the destination runs exactly the version that sent it.
`deploy_artifact` remains for pushing a build *other* than the one you run,
such as an artifact from a branch. The path must be absolute, and the binary
must be one the destination can run; the launcher verifies the version banner
after it lands but cannot know which libc it was built against. Works for ssh
destinations too. Containers run as root unless told otherwise, and a jsh older than the
"root shell trusts the system helpers it could write" fix refuses `/usr/bin/git`
and `/usr/bin/bash` as untrusted helpers when euid is 0 — Git completion, the
Git prompt, and the `.bashrc` import all disappear inside the container while
working locally. Pair container tabs with a jsh that carries that fix.

#### Browsing remote filesystems

The sidebar file tree browses any configured host natively — no sshfs, nothing
to install on the far side. The location selector in the tree's header lists
`Local` plus every `[[remote_hosts]]` entry as `ssh: name` or `docker: name`;
switching re-roots the tree at that account's home directory. Listing and file
operations spawn the system `ssh` (BatchMode, 10 s connect timeout, `ssh_args`
honored) or `docker exec` and run a small POSIX sh probe on the far side whose
arguments are single-quote-escaped or passed as raw argv, so paths with spaces
or shell metacharacters survive the trip. A remote listing admits a name only
when its UTF-8 bytes can be reused exactly for the actionable path; malformed,
duplicate, and conflicting same-name records fail closed. The probe checks
symlinks before directories, so a link to a directory is shown as a file-like
leaf and cannot create an expansion cycle. It receives a hard 4097-record
budget, stops enumerating on the far side at that boundary, retains at most
4096 unique rows, and carries an explicit truncated bit back to the scan
consumer instead of silently treating the prefix as complete. Right-clicking a row offers New
File, New Folder, Rename, Delete (with confirmation), Copy, Cut, Paste, and
Refresh; the same menu works locally, and a successful operation refreshes
only the directories it touched, so unrelated expanded rows never collapse.
Refreshes are latest-wins per directory: an older remote reply cannot overwrite
a newer snapshot. Superseding a same-path request now retires it while it is
queued for a scan slot and cooperatively stops an already-running remote list
probe, including its process group. A non-root row removed while its request is
in flight is discarded instead of having its children merged into the root.
Surviving rows keep their loaded children and expansion; a same-name file ↔
directory type flip is replaced exactly, and surviving selection/cursor
identities are restored after reconciliation. Scans and mutations share 16
fixed workers behind a hard 128-job pending limit. Each immutable remote
authority is additionally capped at four running and 32 pending jobs.
Authorities round-robin inside the Interactive, listing, and bulk-transfer
lanes while those lanes retain weighted 4:2:1 admission, so one slow host cannot
occupy the queue or starve another; at most four background transfers run
globally. Cancellation physically removes queued work. The Files status
therefore distinguishes **Queued**, **Loading**, and in-place
**Refreshing**. An initial expansion failure leaves its lazy row retryable,
while a refresh failure leaves the last-good contents visible. Both expose a
focusable, accessibly labelled **Retry** button. Completed failures are bounded
to the newest eight and user-visible errors are allow-listed by category, so
SSH stderr, credentials, hostile control characters, and endpoint details are
kept in bounded logs rather than labels or toasts. Queue wait and running time
are observable without displaying authority details. Typed failures enter an
authority/path exponential cooldown; automatic expansion and TTL work respect
it, while an explicit Retry bypasses it once.

Successful listings record their completion time; loaded directory snapshots
become stale after 30 seconds or immediately when an exact file-operation
affected directory is invalidated. Re-expansion refreshes only that stale
directory. While Files is visible and the window active, a five-second tick
boundedly revalidates the root plus at most seven loaded expanded directories;
activation and reopening Files trigger the same check. Successful root
listings also populate an authority-bound eight-entry LRU used only as a safe
reconciliation seed, and exact operation directories invalidate it even if the
user navigated away before completion. Plain **F5** refreshes the root plus up
to 63 materialized expanded directories, and a row context-menu Refresh targets
that exact directory (or a
file's parent). F5 is accepted only while focus or the pointer is inside the
visible Files header, status, or tree; terminal-region F5 and modified F5
keystrokes continue to the terminal. Reconciliation clears only a vanished
selection/cursor and the drag-hover target, and advances a presentation
revision when rows actually change. Menus or confirmation dialogs captured
before that change are rejected, while a file operation already dispatched
against the validated backend can still settle and report its result.
Dot-prefixed entries are hidden by default; the focusable eye button reveals
or hides them instantly over the loaded model without rescanning or losing
loaded expansion state. The name filter composes with that preference.

The header has Back/Forward, Parent, filesystem Home, active-terminal-directory,
and clickable breadcrumb actions. Location changes, Parent/Home, directory
activation, history, and typed paths are transactions: Anvil freezes the
authority, lists the absolute target, and commits the location/root/history
only after that latest request succeeds. A failure, stale token, or changed
authority leaves the old rows, expansion, and selection untouched. History is
success-only, bounded to 50 entries per authority, and Forward is truncated
only by a successful divergent navigation. **Ctrl+L** opens the absolute-path
entry; it rejects relative, dot-segment, oversized, control, and bidi/spoofing
text before any probe. Non-UTF-8 local paths are never copied lossily into the
actionable entry. **Alt+Left/Right**, **Alt+Up**, and **Alt+Home** invoke
history, Parent, and Home only while keyboard focus is within the mapped Files
region. Merely hovering Files never captures a terminal's Ctrl+L or Alt chord;
F5 retains its separate focus-or-hover refresh policy.

Files also follows an ordinary interactive SSH login launched in the focused
pane. Anvil uses the shared dedicated process observer to read the real
foreground argv from `/proc`; terminal text, OSC command strings, and generic
session-restore argv are never connection authority. It recognizes both a
direct interactive `ssh` and jsh's provenance-checked `jsh-remote.sh` launcher,
while refusing remote commands and options such as `ProxyCommand`,
`LocalCommand`, `-F`, and provider loading. A field-for-field compatible,
unique configured transport is preferred; otherwise the target becomes a
clearly labeled `(temporary)` location that is never written to `config.toml`.
When jsh proves its ControlPath, Files adds that socket only to an immutable
execution snapshot: stable profile matching and UI identity continue to use
the base target. An explicit `ssh -S …` or `-o ControlPath=…` is separated in
the same way; an observed live socket wins over a matching saved profile's old
socket, while that saved socket remains the fallback when the observed command
has none. The home probe, scans, file operations, clipboard, and transfers then
reuse the chosen authenticated connection. Saved and temporary authorities for
the same stable SSH namespace paste directly rather than relaying locally, and
Copy/Cut prefers the live endpoint from either side. The existing tree remains usable
while the remote-home probe runs. Only a successful probe from the
still-focused pane, with the same foreground base target, exact execution
overlay, and unchanged tree navigation and file-action revisions, switches the
root and reveals Files. A
late result after a pane change, SSH exit, user file action, or Local →
other-host → Local navigation is discarded. If Files is already on the same
target, a changed execution socket first passes the same staged probe; only
then is it refreshed and revealed without losing rows or the current directory.
Exiting SSH does not yank a tree the user is still browsing back to Local.
Password-only SSH cannot be borrowed from the terminal safely: when the
BatchMode sidecar cannot authenticate, Anvil preserves the old tree and offers
**Retry** alongside key/agent or saved-profile guidance. Automatic following
is disabled in safe mode.
Cloud-generated endpoint names are middle-ellipsized in the selector (keeping
the recognizable login prefix and provider-domain suffix), while the selected
entry's tooltip retains the complete safely rendered endpoint.

The terminal button in that header is in the normal Tab order and has an
explicit accessible action name. On `Local` it opens a normal local tab with
the current tree root as its initial working directory. On an `ssh:` or
`docker:` location it revalidates and connects the selected profile. A
temporary observed target opens a plain interactive SSH login through the live
socket when available, with no implicit remote jsh command. It deliberately
does **not** promise to start at the path
currently shown by the tree, because the remote shell/profile owns its startup
directory. Its
tooltip states that distinction before activation, and Enter or Space invokes
the focused button. If `remote_hosts` is reloaded or edited, a browsed remote
location follows only one field-for-field identical complete profile through a
reorder. An edited, removed, invalid, or ambiguously duplicated profile drops
the tree to `Local` and clears the old remote rows instead of reusing its index
for a different destination. Remote-home answers carry both the initiating
scan generation and complete profile, so leaving and later reusing the same
numeric slot cannot let an old answer replace the new host's root. Every
context-menu filesystem action is likewise bound to the generation, location,
and complete remote profile that opened the menu; New, Rename, and Delete keep
that same authority through their confirmation dialog. The header terminal
button and OS file drop capture the same click/drop-time authority before their
Relm messages are queued. A stale action is cancelled with a notice instead of
applying its old root or path to another backend.
The file clipboard follows the same exact-profile rule across a safe reorder
and gives every Copy/Cut action its own identity. Paste resolves the menu's
frozen token through the live reconciled clipboard, so an old menu cannot use a
later Copy/Cut. Slow rename, delete, or cut completion can consume only the
intent it started with; batch delete retires clipboard sources only for paths
actually removed. Batch cut removes only successfully moved sources from that
intent, including the committed prefix of a cancelled batch, and retains
colliding, unfinished, or source-delete-failed items for retry. Cut source
cleanup also keeps the original validated host snapshot for its entire
transfer. Background mutation and transfer callbacks settle that exact token
first, then publish progress, completion/error notices, and row refreshes only
while both the frozen tree authority and the transfer's monotonic identity are
still current; a newer transfer or an A-to-B tree switch suppresses every late
UI effect. Following an active remote tab's cwd uses the connection's immutable
complete configured profile (kept separate from learned/restored runtime
session state) and requires one valid exact match, so reorder remains safe but
a same-name replacement, edit, removal, or duplicate cannot redirect the tree.

Paste also works across locations. Copying or cutting on a host and pasting
locally downloads (labeled "Paste (download)"); the reverse uploads; pasting
between two different hosts relays through a staging file under the system
temp dir, inside an exclusively-created mode-0700 wrapper that retries occupied
names and verifies its held directory inode before cleanup. Files stream
through the probe (`cat`/`put` — the upload lands in a private same-parent
directory and is hard-linked into place without replacement), directories
stream as tar archives, and payloads are capped at 512 MiB with a 15-minute
overall timeout. While a transfer runs, a held toast reports throttled
progress ("Downloading name… 12.4 MiB", or "X / Y MiB" for single-file
uploads) and offers a Cancel action that kills the stream, removes the
partial temp file, and reports a neutral cancelled status rather than an
error. Destination preflight classifies links before directories and treats a
FIFO, socket, or device as occupied without opening it for a size read. A
destination that already holds the name is refused before
any bytes move, including when that directory entry is a dangling symbolic
link — for directory uploads the v3 probe checks the collision itself before
extracting, so the refusal is atomic on the far side — a cut
across locations deletes the source only after the copy
landed, and partial transfers clean up after themselves. Names are validated
before any dialog is accepted, `/` can never be a delete target, and a stale
host removed from the config drops the tree back to `Local`. The context menu
also has Copy Path, which puts the row's full path — for remote rows the
plain remote path, ready to paste into the remote shell — on the clipboard.

Files and folders can also be dropped straight from the OS file manager onto
the tree: the row under the pointer (a directory, a file's parent, or the
tree root over empty space) is highlighted while hovering, and the drop
imports into it — copied recursively when the tree shows Local, uploaded
through the transfer machinery otherwise, with the same progress toast and
Cancel action. A drop is refused wholesale past 256 items or the 512 MiB
total (estimated with a bounded, symlink-free size walk), and per-item
failures — an existing name, an unreadable file — are summarized in the
completion toast without aborting the rest.

Local Rename and downloaded-file publication use Linux
`renameat2(RENAME_NOREPLACE)`: a destination created by another process after
the friendly existence check wins intact instead of being overwritten by the
commit. A kernel or filesystem without that atomic primitive fails closed
rather than falling back to a racy rename.
Transfer staging names are reserved owner-only with exclusive create before
their producer starts, so partial content is not published by a permissive
umask, and a planted hidden symlink is refused without touching its target or
leaving a child process to reap. The downloaded regular file retains that
owner-only mode when its staging inode is published. These private names have
a fixed-size basename independent of the transferred name, so a valid
filesystem-limit name remains transferable; occupied candidates are retried
without unlinking them or starting the producer, and cleanup verifies the
reserved inode before unlinking so a replaced candidate survives.
Remote regular-file uploads likewise receive bytes inside a private 0700
same-parent directory and publish the mode-0600 payload with an atomic
no-replace hard link. A destination created after preflight wins intact, and
cancel cleanup enumerates only the 32 candidates bound to that upload's unique
transfer token.
Downloaded directories are extracted into a private 0700 same-parent directory,
validated for one matching directory root, and only then published with the
same no-replace rename. A concurrently-created destination is never merged
with tar output or removed during cleanup. The staging directory keeps a
no-follow descriptor open, and recursive cleanup runs only while its path still
resolves to that held inode.

The tree multi-selects: Ctrl+click toggles rows, Shift+click extends a range.
A right-click inside the selection aims the menu at all of it (Delete reads
"Delete N items" and the confirmation names up to five); a right-click
outside collapses the selection to that row first. Copy, Cut, and Delete run
as one background batch that continues past per-item failures and reports a
summary, and a batch cut-paste deletes each source only after that item
landed. Rename, New File, and New Folder stay single-row operations and are
insensitive while multiple rows are selected; Copy Path with a selection
copies one full path per line. The magnifier in the header opens an inline
filter over the already-loaded rows — case-insensitive name matching that
keeps matches and their ancestors (auto-expanded), restores the previous
expansion when it clears, works identically over local and remote listings,
and never triggers a scan.

### AI

AI surfaces are optional and can be hidden with `ai_enabled = false`. Provider
selection is `ANVIL_AI_PROVIDER`, then the `ai_provider` setting, then the
Anthropic default. Accepted values are `anthropic`, `openai-compatible` (or
`openai`), and `ollama`. `ANVIL_AI_MODEL` and `ANVIL_AI_BASE_URL` override the
matching TOML settings and provider defaults. `ANVIL_AI_API_KEY` overrides the
provider-specific `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `OLLAMA_API_KEY`.
Keep API keys out of the TOML file: either in the environment or in a private
key file. The Settings dialog's **API Key** row stores a pasted key as a
0600 single-line file — `ai_api_key_file` if configured, otherwise
`~/.config/anvil/ai.key` — and `ANVIL_AI_API_KEY_FILE` overrides the
configured path without ever being written back to config.toml.

AI network calls happen only after an explicit AI action, except the optional
review-first correction fallback after a narrowly classified failed command.
The persistent right-side **AI Chats** panel keeps a searchable library of up
to 50 conversations. **New chat** retains older conversations; each chat owns
its title, draft, selected-Block context, archive state, in-flight request,
Stop/Retry state, and streamed response. Chats can be renamed, archived,
unarchived, or deleted after confirmation. Switching chats never redirects a
background response, and a late response cannot recreate a deleted chat.

The panel divider, visibility, selected chat, drafts, retryable questions, and
bounded conversation snapshot survive a normal session restart. Press Enter or
Ctrl+Enter to send and Shift+Enter for a newline; IME candidate confirmation is
given first refusal. `Ctrl+Shift+Q`, or **Ask AI About Block** in a finished
block's context menu, attaches a visible, clearable bounded command/output
snapshot without replacing text already being edited. Optional recent shell
history remains an explicit per-request context switch.

Typing `?` in the command palette opens an inline, pane-bound suggestion card.
The request can be stopped or retried; a result can be copied, regenerated,
edited, or inserted at the prompt. Insert never presses Enter. The shared review
card shows a live risk label and rejects multiline, oversized, control, and
visually ambiguous edits.

The Shell Agent presents status and settings in a responsive inline dashboard,
while conversation events remain as separate Block-style cards after the
dashboard closes or **New task** resets model context. It proposes one command
per turn and only runs it after **Approve & Run**. **Insert only** leaves the
edited command at the prompt. Approval is bound to the current clean prompt,
the exact reviewed command, a one-shot local generation, and bash/zsh shell
integration authenticated by a private inherited-FD token. Anvil inserts the
approved text without Enter, reads the rendered editor back exactly (including
an empty suffix), and only then submits Enter separately. OSC 7771 proves the
integration consumed the token; token-bound OSC 133 start/completion IDs and
PTY foreground ownership correlate the lifecycle. If any proof changes, times
out, or is unavailable (including remote/Flatpak bridges), execution fails
closed instead of attributing a different command's result to the Agent.

`command_correction_enabled = true` (or
`ANVIL_COMMAND_CORRECTION_ENABLED=true`) enables suggestions for unknown
executables/packages, subcommands, and options. Target output is preferred,
then bounded local APT/PATH evidence, with AI as a strict-JSON fallback. An
unchanged, non-dangerous host-verified candidate offers **Run verified
command**; any edit, new risk, target-output hint, remote candidate, or AI
candidate immediately uses **Insert for review** instead. Nothing runs or is
inserted automatically. The option defaults to true and is available in
Settings and the Agent dashboard; `--safe-mode` suppresses it.

Only the local stages run on consent you have already given by enabling the
feature. The AI fallback sends the failed command, the working directory and a
bounded terminal-output sample off the machine, so it additionally requires
`ai_share_command_context = true` — the same switch Codex tasks and the AI
palette honour. With it off (the default), a correction is offered from local
evidence or not at all.

This is a change in behaviour, and the largest one on this surface: correction
defaults to on, so with the shipped default of consent off, failures that only
the model could answer previously produced a card and now produce nothing. If
you relied on AI corrections, set `ai_share_command_context = true`.

Two further limits changed with it. An automatic helper is resolved through the
family's one trust predicate, which requires every path component to be
system-owned — or owned by you and not self-writable — and writable by neither
group nor others. A binary owned by a *third* user found earlier on your `PATH`
is therefore never executed; on a machine whose only usable helper was such a
binary, the `PATH` and APT evidence it produced disappears and only
target-output corrections remain. Conversely, anvil running as root (a
container, or `sudo anvil`) produces APT- and `PATH`-verified corrections again.
And a card is raised only for a command whose exit status the shell itself
reported, so an interrupted or force-closed block — where the classifier could
be reading the previous command's scrollback — no longer raises one, and
dismisses any card still standing for an older command in that pane.

The legacy `agent_auto_approve_readonly` /
`ANVIL_AGENT_AUTO_APPROVE_READONLY` setting is accepted only for migration and
is always normalized to false. Every Agent proposal requires explicit approval.

See [`docs/SMART_COMMAND_CORRECTION.md`](docs/SMART_COMMAND_CORRECTION.md) for
the correction boundary and
[`docs/AI_AGENT_ACCEPTANCE.md`](docs/AI_AGENT_ACCEPTANCE.md) for manual
acceptance checks. The shared behavior boundary between this Relm4 frontend and
Forge's GTK4 frontend is recorded in
[`docs/FRONTEND_PARITY.md`](docs/FRONTEND_PARITY.md).

### ASCII organism

Set `ascii_organism_enabled = true` to attach the optional local organism to
new Block panes. It reacts only to content-free command lifecycle facts,
focused-pane presence, elapsed time, exit status/duration, and Agent state; it
does not run commands, use an LLM, or persist command/output text. Its bounded
memory lives under `${XDG_STATE_HOME:-~/.local/state}/anvil/` and is flushed on
normal shutdown through the same durable persistence worker as other local
state.

`ascii_organism_motion = "full" | "calm" | "static"` selects animation and
spatial behavior. Omitting it follows the desktop animation preference.
Settings exposes the same four choices as Forge: Automatic, Full, Calm, and
Static. Changes apply to newly created Block panes; existing pane-local life
continues without being replaced mid-command.

Lifetime growth is now visible as well as named in the badge: juvenile bodies
use rounder ears, larger eyes, and quicker micro-motion; adult keeps the
canonical silhouette; seasoned gains a notched ear and a slower cadence. These
layers preserve every pose family's exact bounding box and never change the
semantic strength of a failure, recovery, or push reaction.

While a command runs, only output-activity timestamps—not bytes, lines, ANSI,
or command text—shape its watch rhythm. The most recent three pulses inside
roughly 1.2 seconds read as busy; roughly three quiet seconds, including a
command with no output yet, read as waiting; returning output gets a brief
0.9-second acknowledgement. These volatile rhythms never infer a result or
enter memory. Full motion also connects selected semantic pose boundaries with
four fixed-envelope frames; Calm snaps to the target and Static remains
card-only. Typing, alternate-screen entry, and fail-closed sizing always
preempt the bridge.

A familiar canonical repository acquires a process-local preferred nest side
and walking-route offset. An unfamiliar checkout may receive one short
post-settle exploration when no higher-priority vigil conflicts; otherwise it
is dropped rather than replayed. No path is displayed, sent to the reducer, or
persisted. A window/session attention arbiter similarly admits optional
failure/vigil expression before closure/recovery/push, then command-rhythm
changes, then greetings and remembered insights. Admitted expressions own a
shared focus window and cue-local cooldown; suppressed ones are dropped without
a pending queue, while durable `[!]`, `[!!]`, `[ok]`, and `[?]` facts remain.

### Notebooks

Activating a `.jtnb.md` file in the sidebar opens the notebook viewer. Markdown
is intentionally minimal. Unlabelled, `shell`, `bash`, `sh`, `zsh`, `fish`,
`pwsh`, and `powershell` code fences get Run, Stop, and Copy controls. Unlabelled
and `shell` cells use anvil's configured shell (with the same safe fallback as
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
${XDG_DATA_HOME:-$HOME/.local/share}/anvil/notebooks/welcome.jtnb.md
```

## State and diagnostics

anvil writes one session snapshot and companion lifetime owner lock per
process:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/anvil/tabs.<uuid>.state
${XDG_CONFIG_HOME:-$HOME/.config}/anvil/tabs.<uuid>.lock
```

It records tab titles, pane layout, working directories, and restorable
foreground commands. On startup, anvil leaves live windows untouched and
atomically claims the newest valid snapshot whose owner lock is no longer held.
Live GTK state is captured quickly on the UI thread; JSON encoding, atomic
replacement, file and directory sync, and cleanup run on a bounded coalescing
session worker, with the newest pending snapshot winning for each window. Its
dedicated lane prevents a slow history or organism write from delaying the
final workspace checkpoint during shutdown.
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
bounded argument counts and lengths for recognized replayable commands. These
structural limits are enforced while the workspace grows, before an
unrestorable layout can be created. If individually valid optional AI or command
state would push the aggregate snapshot over 4 MiB, anvil keeps the tab/pane
workspace and retries without that optional payload. The
state directory is owner-only (`0700`), and snapshots and locks are `0600`.
Inspect the newest snapshot with:

```bash
./scripts/show-state.sh
./scripts/debug.sh info
```

Logging accepts plain levels and target-specific directives. `ANVIL_LOG`
takes precedence over the standard `RUST_LOG` variable:

```bash
ANVIL_LOG=debug anvil
RUST_LOG='warn,anvil=debug,anvil::session=trace' anvil
```

Configuration recovery files live beside `config.toml`: two rotating backups
(`config.toml.bak` and `config.toml.bak.1`) plus `config.toml.before-restore`
when the recovery command replaces a live file. They are private user files and
can contain the same paths or remote profiles as the main configuration.

Command-only history is enabled by default at
`${XDG_STATE_HOME:-$HOME/.local/state}/anvil/history.jsonl`. It stores the
command, working directory, exit status, and completion time, but not terminal
output. Set `command_history_enabled = false` to disable it or configure an
absolute `command_history_path`. Reads, appends, compaction, and shutdown flush
are size-bounded and serialized across processes; malformed or unterminated
records are skipped without allowing unbounded allocation.

Each launch is an independent application instance with its own snapshot, so
concurrent windows no longer overwrite one another. Session restoration is
still recovery state rather than durable project storage.

## Security and privacy

- Shell commands have the same permissions as anvil. Review commands inserted
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
data/                        desktop entry, AppStream metadata, icons
packaging/                   Flatpak manifest and release installer
```

Use `cargo fmt`, `cargo clippy --all-targets`, and `cargo test --all-targets`
before submitting changes.

## License

anvil is dual-licensed under **MIT OR Apache-2.0**; pick either at your option.
Full texts are in [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE). Contributions are accepted under the same
dual terms.
