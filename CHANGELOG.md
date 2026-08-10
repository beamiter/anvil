# Changelog

All notable changes to anvil are documented here. The project follows semantic
versioning for tagged releases while it remains experimental.

## Unreleased

### Added

- A persistent Relm4-native **AI Chats** side panel with a searchable retained
  chat library, per-chat drafts and selected-Block context, automatic titles,
  rename/archive/delete, concurrent owner-bound streaming requests, Stop/Retry,
  dragged-width persistence, and bounded versioned session recovery.
- The same local, no-LLM ASCII organism available in Forge, rebuilt behind a
  Relm4 adapter: shared reducer/memory semantics, automatic/full/calm/static
  motion, focused-pane ownership, live/sticky/inline Block surfaces, Agent and
  command lifecycle reactions, and durable bounded shutdown flushing.
- Built-in Bash, Zsh, Fish, and PowerShell CLI completion generation through
  `--generate-completion` (with `--completion` as an alias), plus the matching
  shipped completion assets.
- Configurable `connect_remote_1` through `connect_remote_9` actions, including
  command-palette entries and safe-mode/out-of-range feedback.
- The Remote Hosts settings group edits saved hosts, not just adds and removes
  them. The pencil loads a host into the form below, which retitles itself and
  offers Save Changes / Cancel Edit; the entry is replaced in place so it keeps
  its position in the picker. Fields the form has no widget for — `ssh_args`,
  `session`, `remote_shell`, `login_shell`, `multiplex`, `deploy_artifact` — are
  carried through untouched, and any `ssh_args` are shown in the host row, since
  losing a `-p 2222` while correcting a typo in a name is exactly the kind of
  edit nobody would think to check afterwards. Renaming a host no longer trips
  the duplicate-name check against itself.
- A config file with no `remote_hosts` key now starts with two worked entries —
  one ssh destination, one container — rather than an empty list. The two
  mistakes the grammar cannot forgive are invisible in an empty list: the port
  belongs in `ssh_args`, never as `host = "box:22"`, and the login belongs in
  `user`, never as `host = "root@box"`. An explicit list still wins,
  `remote_hosts = []` included, so deleting them in the dialog (which writes the
  key back) makes them stay gone.
- `[[remote_hosts]]` gained `deploy_artifact`: a jsh built on this machine for
  `deploy` to push, instead of the published release it would otherwise fetch.
  It is the only way to deploy where there is no release — a build from a
  branch, or a machine with no network — and without it such a host spends a
  few seconds failing to reach the release host and then falls back to shell
  integration, which keeps blocks but none of jsh. An artifact that is not an
  absolute path rejects the host rather than being ignored: a relative path
  would resolve against whatever directory the tab started in, and a leading
  `-` would be read as an option by the launcher.
- `[[remote_hosts]]` gained `docker = true`: `host` becomes the name of a
  running container, the tab connects with `docker exec` instead of ssh, and
  `user` becomes the user inside it (`-u`, or `--docker-user` when deploying).
  `deploy` behaves as it does over ssh. `jterm_core::jsh_remote` and
  `jsh-remote.sh` have supported `--docker` all along; anvil hardcoded it to
  `false`, so a container target could not be expressed in the config at all.
  `ssh_args`, `multiplex`, and `login_shell` are ignored for a container.
- Project licensing under `MIT OR Apache-2.0`: canonical `LICENSE-MIT` and
  `LICENSE-APACHE` texts, a `license` field in `Cargo.toml`, and the matching
  AppStream `project_license`, which until now claimed `LicenseRef-proprietary`
  because the repository granted no license at all.
- The session AI panel now streams replies: assistant text appears in the
  transcript as it generates instead of after the full response. On success
  the streamed text is replaced by the provider's complete reply, so the
  recorded conversation stays byte-identical to the non-streaming path (and
  picks up any trailing token-limit advisory); a mid-stream failure keeps the
  partial text visible and reports the error like any other request error,
  and cancellation still kills the transfer immediately. Controlled by
  `ai_stream` (`ANVIL_AI_STREAM`, settings toggle, default on); command
  generation, explain helpers, and agent mode always wait for the complete
  reply.
- Block mode now honors OSC 9 and OSC 777 desktop notifications: programs
  inside the PTY (including remote ones over SSH) can raise a `notify-send`
  notification through `jterm_core::notify::app_notification`. The parser
  control-strips and caps the text; anvil paces launches app-wide — at most
  one notification per output batch and one every two seconds, extras dropped
  silently — matching frost.
- One-command install and update for the companion shell jsh. The palette
  action "Install or update jsh" runs the installer in its own VTE tab, so the
  tab is the progress UI: it can be interrupted with Ctrl+C and waits for Enter
  before closing. When jsh is missing or outdated, a toast offers the same
  action. The installer itself comes from the jsh repository and is embedded in
  `jterm_core::jsh_install`, which keeps checksum verification, atomic
  replacement, the rollback copy, and the "`PATH` resolves `jsh` to something
  else" warning in one place for the whole family. The check runs on a
  worker thread and never installs anything on its own; `jsh_update_check`
  (`startup` / `daily` (default) / `never`) governs how often it looks, and its
  cache is shared with every other jterm on the machine.
- Clear Blocks is now undoable: the cleared blocks are stashed and an explicit
  "Undo clear blocks" action rebuilds them above any blocks created since,
  with toast feedback on both clear and restore.
- Failed-block navigation: "Jump to previous/next failed block" actions step
  through non-zero-exit blocks with wrap-around, mirroring pinned-block
  navigation (`jump_to_prev_failed` / `jump_to_next_failed`).
- The total-history scrollbar now paints theme-aware failure markers at the
  approximate positions of failed completed commands, using the same outcome
  classification as block-card status chrome.
- Block search now includes output already produced by the command that is
  still running. Its live VTE joins the same highlighted result sequence after
  the finished blocks, so next/previous search navigation reaches long-running
  builds and streaming commands before they finish; closing search clears the
  live highlight as well.
- Whole-session export: "Export session as Markdown/JSON file" writes every
  completed block to a timestamped, owner-only file under the anvil data
  directory and reports the path in a toast.
- The block right-click menu gained multi-selection-aware "Copy Blocks as
  Markdown", including prompt, command, output, exit code, and duration.

- Cross-block command/output search and a shared review-only input boundary for
  workflow, history, file-tree, notebook, remote, and AI insertions.
- TOML/YAML workflows with ordered directory precedence, metadata retention,
  Unicode-safe `{name}`/`{{name}}` substitution, and six installed examples.
- Multi-shell executable notebooks with Run All/Stop All, separate bounded
  stdout/stderr, and process-group cancellation that reaps descendants.
- Configurable Anthropic, OpenAI-compatible, and Ollama clients plus a strict
  proposal-ID Agent state machine with explicit per-command approval.
- Stable Flatpak host execution for terminal, notebook, Git, notification, and
  dependency probes; reverse-DNS desktop/AppStream identity and raster icons.
- Cargo-or-Nix source installation with prefix/data/bin overrides, `DESTDIR`,
  dry-run, safe legacy-launcher cleanup, and XDG-aware uninstall.
- `--check-config [--json]`, `--config-path`, and explicit rotating-backup
  recovery with preservation of the replaced live file.
- Process-safe configuration locking, exact revision conflict detection, two
  known-good backups, and durable atomic replacement.
- Isolated `--safe-mode` recovery sessions with VTE + `sh`, no restore or
  persistence, and network/state-producing integrations disabled.
- Machine-readable `--doctor --json` diagnostics and a privacy-preserving
  `anvil-support-bundle` archive generator.
- A scheduled dependency vulnerability audit, ShellCheck gate, shared
  `make security` command, and repository Rust toolchain contract.
- Build provenance metadata and the exact Cargo lockfile in relocatable bundles.
- Commands completed in inactive Block panes now surface activity on success
  and attention on failure.

- First-class Nix package, `nix run` app, flake check, and enriched development
  shell for the two supported Linux architectures.
- Deterministic relocatable Linux release archives with SHA-256 checksums and a
  user-local installer that preserves existing configuration.
- Tag-driven GitHub release automation and CI artifacts for every successful
  pull request build.
- Grouped weekly Dependabot updates for Cargo crates and GitHub Actions.
- Contribution, architecture, and private vulnerability reporting guidance.

### Changed

- The Kitty graphics protocol now parses through
  `jterm_core::kitty_graphics`: control data, chunk assembly, base64, raw
  `f=24`/`f=32` validation with RGB→RGBA expansion, the PNG IHDR sniff, and
  the caps (`Caps::BLOCK`, this repository's historical 16 MiB / 16384 px
  budgets) are shared with the rest of the family, while the GDK texture
  build, the `a=q` probe answer, the `i=`/`I=`/`p=` PTY responder, and the
  per-block image budget stay here. Where the four terminals disagreed the
  protocol wins, so five behaviors change: a command without `f=` now means
  raw RGBA instead of PNG (PNG payloads need an explicit `f=100`); a raw
  payload must match `s=`×`v=`×channels exactly instead of merely reaching it;
  `t=f`/`t=t`/`t=s` are reported unsupported instead of being decoded as image
  data; `f=` accepts only `100`/`32`/`24`; and a continuation chunk may carry
  only `m=` and an optional `q=`, so a repeated first chunk aborts the upload
  rather than splicing into it. `i=` together with `I=`, `o=z` compression,
  and Unicode-placeholder controls are now rejected while parsing; because
  their identifier cannot be trusted, such commands get no reply at all
  instead of a guessed one.
- Shell quoting, restorable-command classification, and the `/proc` foreground
  probes now come from `jterm_core::process` (which was seeded from this
  repository's copy), and executable lookup uses `jterm_core::host`; the local
  duplicates were removed. Login-shell wrapping and file-tree path insertion
  converge on the shared quoting style: embedded single quotes use the
  `'"'"'` form instead of `'\''` (both POSIX-valid), and obviously safe file
  paths are inserted unquoted for readability.
- Pane moves and remote reconnect callbacks now retain stable pane/session
  identity; reconnect cancellation affects only the dead leaf it belongs to.
- Safe mode ignores user config and behavior overrides, disables restore and
  persistence, and starts an isolated built-in VTE recovery pane.
- Support bundles now use redacted diagnostics and exclude host names, local
  paths, environment values, configuration/history/session contents, and keys.
- Default shortcuts now share the jterm ergonomic layout: directional Pane
  focus/resize layers, browser-style tab digits, symmetric zoom/opacity keys,
  and shell-owned `Ctrl+R` / `Ctrl+P` passthrough.
- Shortcut references and the welcome notebook now match the reload and
  cross-block-search bindings plus multi-shell, process-group notebook controls.
- In-app settings now refuse stale multi-window writes and invalid schemas;
  diagnostics and support bundles expose validation/lock state without values.
- Local and CI linting now share one canonical Clippy policy.
- Development commands use the committed Cargo lockfile and expose complete
  `make verify` and `make package` workflows.
- CI now validates the headless CLI, desktop entry, release bundle, checksum,
  and Nix package in addition to Rust formatting, tests, lints, docs, and the
  optimized build.
- New tabs and restored local leaves use the configured terminal backend; a new
  split inherits the focused pane's backend, matching Forge for both Block and
  VTE layouts. Integrated remote restores stay on Block, and Block-only actions
  explain when invoked from an incompatible VTE pane.
- Long-running Block output and asynchronous prompt output now use bounded ring
  buffers, avoiding repeated multi-megabyte front shifts.
- Command-history writes and compaction now run through a bounded worker and
  flush on normal shutdown; palette searches use one bounded in-memory snapshot
  per opening instead of rescanning JSONL on every keystroke.
- Missing restored working directories fall back safely, while PTY launch
  failures render an actionable, focus-aware pane error instead of crashing the
  application. Flatpak host-directory probes are cached and time-bounded.
- Managed remote panes restore by resolving their saved profile name against
  the current validated configuration. Profile names are now unique; removing
  or renaming one suppresses stale connection replay and surfaces a safe local
  fallback instead.
- Remote/container working directories remain externally namespaced and can no
  longer seed local tabs, splits, file trees, duplicate-pane launches, or local
  Git-status probes.
- Initial Block commands retain structured argument boundaries through the
  shell wrapper and use a bounded fallback when shell integration markers are
  unavailable, including direct SSH and Mosh sessions.
- Keyboard chords now use the shared `jterm_core::keybindings` core (parsing,
  display, and map keys); only the GTK keysym/modifier translation remains
  app-side. The config grammar widens deliberately: `control`/`option`/
  `cmd`-style modifier aliases, case-insensitive named keys, `esc`/`del`/
  `ins`/arrow aliases, and F13–F24 now parse, and `"unbind"` joins the
  existing unbind tokens (empty, `none`, `disabled`, `false`). The validator
  now accepts unbind tokens and `false` instead of flagging them. Displayed
  shortcuts keep the `Ctrl+Shift+Alt` order and "Enter" spelling; the
  sidebar chord now displays as `Ctrl+\` instead of `Ctrl+backslash`, and
  docs follow the displayed modifier order. A new contract test pins
  anvil's defaults to the family-wide `DEFAULT_CHORDS` table.
- Block-mode child-process termination now uses the shared
  `jterm_core::process` lifecycle (also seeded from this repository's copy):
  `ChildLifecycle`, `ReapOwner`, and the `EscalationPolicy` ladder replace the
  local `ChildLifecycle`/`terminate_terminal_process` pair, so `src/process.rs`
  is re-exports only. The SIGHUP → SIGTERM → SIGKILL timings and the
  quiet-scan PTY session drain are unchanged. Three things get stricter:
  constructing a lifecycle is now fallible and opens a pidfd, so every signal
  during shutdown is bound to the process the PTY forked instead of a bare pid
  number; the PTY child itself is signaled through that pidfd rather than
  `kill(pid, …)`; and a lifecycle that is dropped without ever being
  terminated now reaps its child instead of leaking a zombie. A pane whose
  PTY cannot be referenced at all (an exhausted descriptor table) fails to
  open with that error rather than starting unmanaged.

### Fixed

- OSC 8 hyperlinks are now bounded and validated before becoming clickable:
  only the terminal's documented URI schemes are accepted, control/whitespace
  and oversized targets are rejected, and GTK tag names use opaque hashes so
  untrusted URI text can no longer become widget object names.
- Session owner, protocol, and short-lived inspection lock descriptors are now
  close-on-exec and explicitly unlocked when their logical guard ends. A shell
  forked concurrently can no longer inherit a lock that makes an exited window
  look live or prevents another instance from recovering its snapshot. Lock
  paths now accept only current-user, singly linked regular files, and the
  session directory is never followed through a symlink. The protocol also
  holds the directory inode, so renaming or replacing the visible protocol-lock
  entry cannot let a second cooperating instance enter the protected namespace.
- Updated `spin` within `flume`'s compatible `0.9` range from the yanked 0.9.8
  release to 0.9.9.
- The desktop integration now actually produces a launcher icon after
  `./scripts/install.sh`. Three separate causes:
  - The entry shipped `Exec=anvil` / `TryExec=anvil`, which depend on `PATH`.
    A desktop session fixes its `PATH` at login and the default target
    `~/.local/bin` is frequently absent from it, so `TryExec` failed and the
    entry vanished from the application list entirely. Both installers now
    rewrite those lines to the binary's absolute path; system bin directories
    such as `/usr/bin` keep the relocatable bare name.
  - Neither installer refreshed the desktop caches, so new entries and icons
    waited for the next login, and a stale `icon-theme.cache` could shadow a
    freshly installed icon indefinitely. Install and uninstall now validate the
    entry and refresh `update-desktop-database` and `gtk-update-icon-cache`,
    skipping the refresh for `DESTDIR` staging and generating those caches under
    a relaxed umask so a `sudo --prefix /usr` install cannot leave `0600` caches
    that no other user can read.
  - `StartupWMClass` carried the application ID, but GTK4 derives the X11
    `WM_CLASS` from the program name (measured: `anvil`). X11 sessions could
    not associate a running window with the entry, so the dock showed a second,
    icon-less item. It is now `anvil`; Wayland still matches on app_id.
- The installer now reports `PATH` problems it cannot fix: a target bin
  directory outside `PATH`, and any other `anvil` earlier in `PATH` (such as an
  old `cargo install` copy) that shadows the binary just installed.

- Inline images render again in block mode. The Kitty graphics assembler
  (`terminal/kitty_graphics.rs`) had no caller: block mode re-wrapped every
  APC G payload as `ESC _ … ESC \` and fed it to the live VTE, and since
  libvte implements no APC graphics handler, every `kitten icat` (or other
  Kitty-protocol) image was silently dropped — no image, no error, no
  diagnostic. The reader now feeds those payloads to the assembler instead;
  decoded textures accumulate against the running command and are mounted as
  GTK Pictures under the finished block's output, folding with the collapse
  chevron (image-only blocks keep a working chevron). Support probes are
  answered too: `a=q` validates the sample image, and commands carrying an
  `i=`/`I=` identifier receive an `OK`/`EINVAL`/`ENOTSUP` reply on the PTY
  (with `p=` echoed and `q=1`/`q=2` honored), so clients that block waiting
  for the terminal to answer no longer hang or fall back to text. The
  per-image (16 MiB encoded), per-block (16 MiB decoded), and dimension
  (16384 px) caps plus the pre-decode overflow guards are unchanged, and a
  half-uploaded image is discarded whenever the active block resets. Images
  are display-only: block history stays text-only, so restored sessions show
  the text without them. Ports forge's implementation.
- OSC color queries now answer with dynamic colors. When a program sets the
  foreground, background, or cursor color (OSC 10/11/12 with a value), the
  pane records the override — the raw bytes still pass through, so the live
  VTE recolors natively — and a later OSC 10/11/12 query reports the recorded
  color instead of the stale static theme; OSC 110/111/112 drops the override
  so queries fall back to the theme again. Finished-block widgets created
  after a dynamic change render with the overridden colors too, matching the
  recolored live view. Specs are parsed as `rgb:R/G/B` (XParseColor, 1–4 hex
  digits per channel), hex, or color names; unparseable specs leave the
  tracked state unchanged.
- "Undo clear blocks" now rebuilds blocks with the pane's active dynamic
  colors. The restore path used the plain theme, so undoing a clear while a
  program held an OSC 10/11/12 override produced theme-colored blocks sitting
  next to correctly recolored ones; the tracked overrides are now shared with
  the view and overlaid exactly as the reader does for new blocks.
- An explicit theme change now clears the pane's dynamic color overrides.
  Applying a theme repaints the live VTE and every finished-block snapshot from
  the theme, so keeping the app's OSC 10/11/12 values left color queries
  reporting a superseded color that nothing on screen used. A program that
  cares can set its colors again after the switch.
- The block id counter is now seeded past every restored history id, so a new
  block can no longer alias a restored one and corrupt id-keyed state
  (selection, bookmarks, undo-clear, context-menu copy).

### Security

- AI command-palette responses containing newlines or terminal control
  characters are rejected before any bytes reach the live PTY.
- Potentially destructive AI-generated commands are highlighted for review.
- Detached palette requests no longer permanently leak their cancellation
  token.
- Session recovery preserves command argument boundaries and does not execute
  legacy joined command strings; replay uses shell-specific POSIX or PowerShell
  quoting and skips unknown grammars. SSH session identifiers are bounded,
  control-free, and shell-quoted, and option parsing ends before the destination.
- PTY launch data and `exec` pointer arrays are validated and prepared before
  `fork`; the child path avoids Rust allocation and environment mutation.
- PTY reaping and shutdown signals share one lifecycle lock so a released PID
  can never be targeted by a delayed cleanup worker. Linux descendant signaling
  uses pidfds plus a bounded repeated session drain, while closing a Block pane
  also releases its reader source.
- Session snapshots use per-process unique identities, lifetime-held owner
  locks, and a serialized publication protocol instead of bare PIDs, preventing
  PID reuse, namespace changes, or lock-creation races from hiding, consuming,
  or overwriting another window's recovery state.
- Session restore now reads only current-user, singly linked regular files
  through nonblocking, no-follow descriptors, enforces a 4 MiB byte budget,
  and rejects more than 32 tabs, 16 panes per tab, or 64 panes total before any
  child process can be spawned. Replayable argv is independently capped at 256
  arguments, 64 KiB per argument and 256 KiB total, and is reclassified before
  spawn; arbitrary legacy shell command strings are never replayed.
- Session checkpoints now use a versioned envelope with a durable empty
  tombstone and a predecessor link. A crash after publishing a replacement but
  before deleting its claimed predecessor can no longer resurrect the old
  workspace, while a crash before the replacement is durable leaves the claim
  recoverable by the next process.
- Configuration reads and write locks apply the same owner, hard-link,
  symlink, FIFO, close-on-exec, and bounded-input checks; permission tightening
  is performed on validated descriptors so a substituted path cannot redirect
  it to an unrelated file or directory. Existing config parents must be owned
  by the current user and not group/world writable, and the parent directory is
  locked across revision checks, backup rotation and atomic publication.
- OSC 7 cwd classification combines a per-pane authenticated authority with
  sticky namespace state and live foreground ancestry (including Flatpak host
  wrappers), preventing remote output from relabeling a remote path as local.
- Shared command history writes are serialized across processes and compacted
  through unique, atomically replaced temporary files; corrupt unterminated
  records are skipped with bounded memory. Lock acquisition is time-bounded,
  the directory inode prevents lock-path replacement bypass, and retired pane
  identities cannot inherit another pane's saved revision.
- Agent snapshots are bounded, owner-checked, no-follow files and are consumed
  exactly once under a directory lock after successful validation. Approved
  commands are armed only at a clean prompt after a private inherited-FD shell
  capability handshake. The reviewed text is inserted without Enter, read back
  exactly from VTE with an empty suffix, and only then submitted separately;
  prompt generation, foreground ownership, token-bound OSC IDs, timeout, and
  completion must all match. Unsupported shells, remote/Flatpak bridges, stale
  prompts, write failures, and lost correlation fail closed.
- Notebook and workflow inputs now reject special files without blocking and
  enforce byte, file-count, field-count, segment, cell, and rendered-command
  budgets. Runnable notebook/workflow commands containing invisible,
  bidirectional, or unsafe control characters are disabled instead of relying
  on a visually misleading review surface; Git context labels make those
  characters visible and bound `.git` pointer/HEAD reads.
- Both PTY bridge writers and the VTE-response/AI-delta cross-thread queues are
  bounded. Kernel backpressure, terminal-query storms, or a very fast local
  model can no longer grow an unbounded userspace queue or monopolize one GTK
  dispatch; overload drops an entire non-authoritative message and final AI
  text remains the source of truth.
