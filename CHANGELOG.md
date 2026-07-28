# Changelog

All notable changes to jterm1 are documented here. The project follows semantic
versioning for tagged releases while it remains experimental.

## Unreleased

### Added

- The session AI panel now streams replies: assistant text appears in the
  transcript as it generates instead of after the full response. On success
  the streamed text is replaced by the provider's complete reply, so the
  recorded conversation stays byte-identical to the non-streaming path (and
  picks up any trailing token-limit advisory); a mid-stream failure keeps the
  partial text visible and reports the error like any other request error,
  and cancellation still kills the transfer immediately. Controlled by
  `ai_stream` (`JTERM1_AI_STREAM`, settings toggle, default on); command
  generation, explain helpers, and agent mode always wait for the complete
  reply.
- Block mode now honors OSC 9 and OSC 777 desktop notifications: programs
  inside the PTY (including remote ones over SSH) can raise a `notify-send`
  notification through `jterm_core::notify::app_notification`. The parser
  control-strips and caps the text; jterm1 paces launches app-wide — at most
  one notification per output batch and one every two seconds, extras dropped
  silently — matching jterm3.
- One-command install and update for the companion shell rsh. The palette
  action "Install or update rsh" runs the installer in its own VTE tab, so the
  tab is the progress UI: it can be interrupted with Ctrl+C and waits for Enter
  before closing. When rsh is missing or outdated, a toast offers the same
  action. The installer itself comes from the rsh repository and is embedded in
  `jterm_core::rsh_install`, which keeps checksum verification, atomic
  replacement, the rollback copy, and the `/usr/bin/rsh` (BSD remote shell)
  shadowing warning in one place for the whole family. The check runs on a
  worker thread and never installs anything on its own; `rsh_update_check`
  (`startup` / `daily` (default) / `never`) governs how often it looks, and its
  cache is shared with every other jterm on the machine.
- Clear Blocks is now undoable: the cleared blocks are stashed and an explicit
  "Undo clear blocks" action rebuilds them above any blocks created since,
  with toast feedback on both clear and restore.
- Failed-block navigation: "Jump to previous/next failed block" actions step
  through non-zero-exit blocks with wrap-around, mirroring pinned-block
  navigation (`jump_to_prev_failed` / `jump_to_next_failed`).
- Whole-session export: "Export session as Markdown/JSON file" writes every
  completed block to a timestamped, owner-only file under the jterm1 data
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
  `jterm1-support-bundle` archive generator.
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
- New splits and every restored local split leaf consistently use the configured
  terminal backend; integrated remote restores stay on Block, and Block-only
  actions now explain when invoked from an incompatible VTE pane.
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
  jterm1's defaults to the family-wide `DEFAULT_CHORDS` table.

### Fixed

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
- OSC 7 cwd classification combines a per-pane authenticated authority with
  sticky namespace state and live foreground ancestry (including Flatpak host
  wrappers), preventing remote output from relabeling a remote path as local.
- Shared command history writes are serialized across processes and compacted
  through unique, atomically replaced temporary files; corrupt unterminated
  records are skipped with bounded memory.
