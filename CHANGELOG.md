# Changelog

All notable changes to anvil are documented here. The project follows semantic
versioning for tagged releases while it remains experimental.

## Unreleased

### Added

- Session restore now rejects duplicate JSON object members recursively before
  interpreting envelope, saved-session, or pane-layout data, including escaped
  spellings of the same key and duplicate members inside future extension
  objects. Restore inputs remain byte-budgeted and malformed snapshots fail
  closed, and serde_json's private RawValue sentinel is reserved so feature
  unification cannot reopen unchecked JSON. The shared boundary is pinned to
  `jterm_core` `21437ba` (and its transitive `jagent` `a462ec8`).
- A fresh, empty Block pane now shows one accessible orientation card explaining
  reusable completed cards, header selection, right-click actions, and
  `Ctrl+Shift+G` search. The first accepted human input, completion, or restored
  history retires it permanently, so it cannot linger over a long first command.
  The card is a non-targetable, non-measuring overlay: it consumes no live PTY
  rows, does not compete for shell-integration/AI notice ownership, and never
  appears in Unified or conventional VTE panes. It suspends during alternate-
  screen ownership and returns afterward, so it cannot cover the first TUI.
- Finished Block cards now keep their selection keyboard contract even after a
  snapshot VTE or header control takes focus. The active edge shows a dynamic
  hint in the header's existing spacer slack: multi-card selections advertise
  recall only, background/empty cards advertise only cancel, and a lone safe
  command advertises `Ctrl+Enter` re-run. Direct re-run inserts the command
  without Enter, waits for an exact stable VTE render, and only then sends CR;
  it fails closed unless the prompt is settled,
  visibly empty, untouched, foreground-owned, and free of pending Agent or
  reviewed submissions; every refusal consumes the chord, rings, and briefly
  displays its visible reason so it cannot fall through to an accidental VTE
  Enter. Plain Enter is likewise selection-owned: busy/dirty/unsafe recalls show
  their reason and ring instead of submitting unrelated prompt text,
  and selected multiline text is refused when missing bracketed paste would
  silently keep only its first line. Hints no longer claim static prompt
  readiness: they show the selected-card count and distinguish recall from
  recall all.
  The hint's natural-width cap follows its real longest row, so `Esc cancel`
  remains visible when the header still has spacer room.
  Focused header controls retain GTK Return/Space activation; Ctrl+Enter remains
  the deliberate Block chord. Alternate-screen ownership suppresses selection
  navigation as well as clearing the existing selection, so no hidden selection
  can surface after a TUI exits. Every history-recall surface, including header
  and context actions, shares the verified empty-prompt guard and cannot replace
  a dirty edit. Faded card action strips are also insensitive and non-targetable,
  preventing Tab or stale focus from activating an invisible control. Delete is
  intentionally not advertised until a grouped
  removal plus undo transaction exists.
- Visible text selection now outranks whole-card selection when copying: a
  highlighted range in one command/output/live VTE or across several VTEs is
  aggregated in document order before Block-card fallback. Oversized text is
  rejected atomically with visible feedback instead of silently copying a card
  or partial range. Repeated selection refusals also refresh one generation-owned
  status lifetime, so older timers cannot hide or resurrect stale messages.
  Every header, menu, and selection recall also refuses multiline history when
  missing bracketed paste would reduce it to a misleading first-line prefix.
- The cross-block search palette (`Ctrl+Shift+G`) gained **Failed** and **Slow**
  toggles and an outcome column. The two predicates already existed with no
  surface that could reach them, so "which failing build took over a second" was
  unanswerable with the data sitting right there; and every row looked alike, so
  telling the failing `cargo build` from the passing ones meant visiting each.
  Rows now carry `exit:1 · 2.4s · …/anvil`, coloured by outcome. Holding Down
  also keeps the selection on screen: GTK scrolls to follow focus, and focus
  belongs to the search entry here, so the list is scrolled directly instead —
  the selection used to walk off the bottom and Enter jumped somewhere the user
  could not see. The three surfaces that mean "slow" now share one threshold
  constant rather than three literals, two of which disagreed.
- A card's output folds and unfolds from the keyboard (`Alt+Shift+O`, selected
  or most recent block) and from its right-click menu, not only from the
  chevron. A 400-line `cargo build` was mouse-only to collapse.
- The block right-click menu can copy a block's working directory, or insert a
  correctly quoted `cd` for it at the prompt (inserted, never run, under the
  same clean-prompt gate as the other recalls). The cwd chip's tooltip carries
  the full path the chip shortens, and both Markdown exports carry a
  `**Directory:**` line — a pasted block that does not say where it ran is not
  reproducible.
- Block mode tells you when the shell is not reporting command boundaries.
  Without OSC 133 there are no blocks at all — no cards, no exit codes, no
  durations — while the terminal otherwise works perfectly, which is the worst
  way for a feature to be missing: nothing looks broken, so nobody goes looking
  for the cause. A pane that reaches the raw fallback with a bash, zsh, fish or
  pwsh shell now docks a dismissible card naming that shell's rc file and the
  one line that fixes it, with a Copy button. It retires itself the moment marks
  appear, including from a `source` typed minutes later. Nothing is said for a
  shell there is no honest advice for: `jsh` carries the marks itself, an `ssh`
  or `docker` pane is someone else's shell, a `-c` pane runs one command and
  exits, and behind a wrapper the rc file cannot be named.
- The card right-click menu offers **Copy Selection** when text is selected, and
  opening it no longer leaves two selection models painted at once. The menu
  reads the text selection before it activates the card selection that repaints
  over it, then clears the text selection — so the Ctrl+Shift+C that follows
  copies what the screen now shows is selected instead of silently disagreeing
  with it.
- Right-click on the block-mode canvas opens a menu with Copy, Paste and Select
  All Blocks. Block mode was the one backend where that button did nothing: the
  live cell is a display-only VTE with no child PTY, so VTE's own menu never
  applied to it, and the per-card menu covers only the cards. The card menu
  still wins inside a card, and a full-screen program keeps the button — the
  menu does not open over `htop`. Clear Blocks is deliberately absent: the
  toast that carries its Undo is emitted around the message, not by the view,
  so a menu entry would be a silent destructive action.
- A running-command pill at the live edge: after two seconds, a chip appears at
  the top right of the pane with a ticking elapsed time, the command in its
  tooltip, and a stop button. The sticky running header only exists once the
  user has scrolled away from the prompt, so at the live edge — where most
  commands are actually watched — a command that printed nothing looked exactly
  like a hung shell. Ctrl+C always worked; what was missing was the readout and
  something to aim at. The two are never on screen together and share one
  elapsed formatter, so the same command cannot be shown two different ages
  depending on where the viewport is. Short commands never flash one.
- The clear-blocks toast carries an **Undo** button. Recovery existed but was
  only reachable by name, through the palette, which is not where anyone looks
  in the second after an accidental `Ctrl+Shift+K`. The button is bound to the
  pane that raised it, not to whichever pane has focus when it is clicked: the
  toast outlives a tab switch, and undoing into the wrong pane would be a second
  accident on top of the first. If that pane has closed, the toast says so.
- Unified mode now renders Kitty `a=T` images on a probe-addressed GTK layer
  below the organism surface. Chunked uploads retain first-chunk geometry and
  final-chunk cursor identity; images stay aligned across scrolling and rewrap, while
  ED3, RIS, alternate-screen transitions, row retention and memory limits all
  fail closed. Block image accounting now also includes PNG backing and GTK
  object overhead, with a 64-image cap.
- Block and Unified command records now keep completion provenance separately
  from exit status. A missing OSC 133 `D` can recover only when the shell owns
  the PTY foreground; inferred records are visibly degraded and never receive
  fabricated end times or durations. Legacy/restored weak records do not gain
  trust, and RIS clears active Agent/command correlation before the next prompt.
  Every accepted `C` now also closes its observer lifecycle exactly once: a
  trusted prompt recovery emits an unknown/degraded finish before Block or
  Unified finalization, while a later `A`, background output, and RIS cannot
  replay or manufacture that finish. This keeps the organism and shared
  activity counters from remaining permanently busy after a lost `D`.
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
- Find-in-terminal now reports `No results`, the current/total match position,
  counts that cannot be proven exact with a `+`, and invalid-regex errors in the
  search bar. Both Block and VTE backends feed the same status model, rebind the
  current query when the active pane changes, and expose accessible
  previous/next/close buttons while Enter, Shift+Enter, and Escape retain their
  keyboard behavior.
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

- The AI chat panel is a thin binding over `jterm_core::ai::chat_store` instead
  of a private copy of the same state machine, so the aggregate live-history
  budget, compaction-before-persistence, typed archive/delete outcomes and the
  reported draft merge are shared with the siblings rather than re-derived.
- `[[remote_hosts]]` validation adopts the caps forge enforces — 64 `ssh_args`,
  4 KiB per field, 256 KiB of total argv. The schema is documented as shared,
  but anvil accepted profiles (a long jump-host chain, a large `remote_shell`)
  that forge rejected with an error pointing at the file rather than at the
  sibling's stricter limit.
- `scripts/clippy.sh` runs bare `-D warnings`. Its seven-lint `-A` allowlist
  included the two lints the handoff called blocking, so the enforced gate and
  the documented gate disagreed; both lints are fixed rather than silenced.
- The AppStream metadata no longer declares a 0.2.0 release. No tag exists for
  it in any sibling, and frost/ember already omit the node for that reason.
- The default AI-panel binding is spelled `Ctrl+Shift+Alt+A`, the modifier
  order `jterm_core::keybindings::Chord::display` renders and the README and
  `config.toml.example` already used. The chord itself is unchanged.
- Changing Block density now updates every existing finished, virtualized,
  correction/review, suggestion, Agent, and notice card in place. The
  virtualization height model changes with the chrome, so long histories do
  not accumulate stale offsets while switching between roomy and compact.
- Git branch chips now share a 64-entry HEAD-locator cache without caching the
  branch value: every card observes branch switches immediately, while missing
  repositories use only a short 200 ms negative cache.
- The complete local verification gate now includes the explicitly named
  GTK/VTE regressions in isolated D-Bus/Xvfb sessions. Portable release
  packaging uses a separate system-linked target and refuses a Nix shell,
  preventing a bare archive from embedding `/nix/store` interpreter paths.

- Typing at a prompt that has been scrolled away brings the viewport back to
  it, clears the unread badge, and retires the jump-to-latest button. Every
  other terminal applies scroll-on-keystroke; block mode did not, so the
  keystrokes went to a prompt the user could not see. Unified is excluded —
  there the scroll lock belongs to its VTE's own adjustment — and a full-screen
  program still owns the viewport it took.
- **Compact Block Layout** applies to the pane you are looking at. The switch
  reached only panes created after it, so it toasted success and left a
  half-dense workspace behind; card margins are GTK properties rather than CSS,
  so reinstalling the stylesheet could never have moved them. Existing cards and
  the live input cell now switch in place, through one layout pass and one
  winsize sync for the whole pane rather than one per card. Its subtitle no
  longer says "in new Block panes".
- The card's git-branch chip is memoized for a few seconds per working
  directory. It is built once per card, and restoring a 200-block session builds
  every card in one pass, so a single repository paid for the same walk-to-`.git`
  200 times — synchronously, on the GTK thread, before the first frame. Misses
  are cached too: "not inside a repository" is the answer that walks all the way
  to `/`.
- Block mode stops claiming keys the shell needs more than it does. Bare
  `Home` / `End` at a prompt jumped the viewport to the ends of history, which
  meant the command line being typed could never reach its own start — the
  single most-used editing key in a terminal, spent on a jump the FAB,
  `PageUp`, and `Ctrl+Shift+N` already offer. They now reach the shell; with
  blocks selected they move the selection to the oldest / newest card, and the
  viewport edges moved to `Ctrl+Home` / `Ctrl+End`. `Ctrl+,` / `Ctrl+.` are no
  longer swallowed in a pane that has never bookmarked a block, or while a
  command or full-screen program owns the PTY: until there is somewhere to
  jump, the near-universal preferences chord reaches the program in the
  terminal.
- `Ctrl+Shift+X` now steps to the *next* failed block and wraps, instead of
  re-landing on the oldest one however many times it is pressed — a dead key
  in exactly the session that has more than one failure worth reading. It is
  bound to `jump_to_next_failed`; `filter_failed_blocks` keeps its
  jump-to-oldest meaning in the palette and in `[keybindings]`.
- `Ctrl+Shift+B` with no selection bookmarks the newest block instead of doing
  nothing, matching how `Alt+Shift+F` already picks its target. Bookmarking
  the command you just watched finish no longer needs a selection first.
- Card quick actions have three distinct icons: copy-command and copy-output
  shared one glyph and were separable only by hovering for a tooltip, which is
  the thing a quick-action row exists to avoid. The duration badge shares
  Unified's formatter — a 90-second command reads `1m30s`, not `2m`, and an
  hour reads `1h`, not `60m` — and its tooltip carries the unrounded
  milliseconds.
- The finished-block right-click menu is part of the render backend, so cards
  rebuilt by session restore and by "Undo clear blocks" get it too. A restored
  card whose right-click did nothing was a card missing half its actions.
- Command Palette disk discovery no longer blocks GTK. Workflow directories
  are prewarmed through a single-flight background refresh and each opening
  reads its bounded command-history tail asynchronously with generation checks;
  actions and the last workflow cache remain usable while fresh rows load.
- Source install and uninstall now agree on `${PREFIX}/bin` (normally
  `~/.local/bin`). A legacy `~/.cargo/bin/anvil` is reported for manual
  migration but never removed without an explicit `--bin-dir`.
- Top-bar, file-tree navigation, remote-host, AI-panel, and Agent icon buttons
  expose explicit AT-SPI names. Dynamic maximize/restore and Shell Agent
  controls update their accessible action name together with their icon and
  tooltip.
- Session snapshots now leave the GTK thread after capturing owned Rust state.
  JSON validation/encoding, atomic replacement, file and directory sync,
  predecessor cleanup, and pruning run through the bounded coalescing
  session lane; repeated changes keep only the newest pending snapshot and
  normal shutdown flushes it before the ordinary history/organism lane, so an
  unrelated slow target cannot hold the final workspace behind it.
- The out-of-box terminal font is the portable Pango `Monospace 14` alias.
  Block status and action affordances use GTK symbolic icons or explicit text,
  so a clean Linux install no longer needs a separately installed Nerd Font.
  An explicitly configured custom font remains selectable even when Pango does
  not include it in its enumerated family list.

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

- A saved AI chat library that is too large or unreadable no longer invalidates
  the whole session envelope. The chats are dropped on their own — while
  decoding and again in the post-decode audit — so the tabs, panes, cwds and
  restorable commands still restore. The `ai_conversation` format is shared,
  and siblings write bigger libraries than anvil ever emits, so importing one
  used to cost the user their workspace layout as well as their chats.
- Streaming an AI reply no longer rebuilds the entire transcript buffer and
  queues a scroll for every fragment. Only the new bytes are spliced in, the
  panel scrolls only when the reader is already at the bottom, and at most one
  idle scroll is pending, so a long reply into a long chat stops saturating the
  UI thread under the software renderer.
- Block/Unified 搜索现在把每个完成卡片的 render stamp 纳入游标身份。Resize、折叠/展开或
  输出过滤重新灌入 VTE 后，Next/Previous 会用保留的查询重建计数；即使搜索只有一个命中、
  逻辑游标停在原位，也会先验证 stamp，不再永久保留失效高亮。跨块结果按 surface 内实际
  命中数记录 occurrence（同一行多个命中分别计步），激活会从顶部精确定位；超过 4096 步
  或中途耗尽时 fail closed，而不是高亮较早的错误命中。
- 共享核心升级到 `jterm_core` `852d33d`（transitively `jagent` `2570e5e`）。
  Core 在暴露恢复会话前持久退役公开快照名，anvil 因此删除重复的恢复后
  目录同步失败门控；新 jagent 也在所有 JSON 边界拒绝重复对象成员。Cargo 与
  Nix 的精确 source hash 同步更新。

- A block whose completion nobody vouched for says so on the card. The caveat
  used to live in a card-level tooltip — the one place a doubt about an exit
  code cannot be seen, because the header's own chips and buttons shadow it and
  a tooltip has to be hunted for. It is now a header chip reading `inferred`,
  `recovered` or `incomplete`, from the same vocabulary Unified's status line
  uses, with the full explanation as the chip's own unshadowable tooltip.
  Background output, which never ran a command, keeps none of it.
- Card output stops being wedged into the bottom-left corner: cards gained
  bottom padding, and the prompt chevron moved from the command row into the
  header so the command and its output share one left edge. Neither costs a
  column — horizontal padding would have narrowed the output terminal, and its
  column count comes from that widget's pixel width, so `ls` would wrap
  differently inside a card than it did in the live pane.
- A filtered block no longer copies more than it shows, or zeroes find for the
  whole session. Copy-output took the full transcript out of a card the user had
  just filtered down — filtering is *how* you decide what to copy — with nothing
  to say the clipboard held more than the screen. And find counted hits in lines
  the filter had hidden: the VTE could not step to them, and that failure is
  read as "no matches", which cleared the query for every other block too. Both
  now read what the card is displaying.
- "Delete Block" deletes the blocks that are selected. Every sibling item in
  that menu already acted on the whole selection; this one removed one of five
  and left the rest highlighted. The label counts them, and removal walks the
  selection from the back so bookmarks, the unread badge and virtualization
  stay consistent.
- The card's metadata cluster stops jumping sideways on hover. The quick-action
  buttons were hidden rather than faded, so revealing them took ~150px out of a
  hexpand header and slid every timestamp, duration and exit badge left, then
  snapped them back on leave — once per card while dragging the pointer down a
  list. They now fade in place, and stay untargetable while faded so they cannot
  swallow the header click that selects the block.
- A healthy block no longer inherits a dead one's "this status could not be
  trusted" tooltip. Only a degraded record sets that tooltip, and only as an
  `if let Some` with no `else`, while the card shells it is set on are pooled
  and reused — so clearing a degraded block and running one ordinary command
  produced a perfectly healthy card explaining why its exit code was inferred.
  Both the pool teardown and the recycled-shell path now drop the tooltip.
- Block mode no longer flickers between two layouts at an idle prompt. A
  finished card's own scrollbar was a box sibling of its output terminal, so
  showing it took ~14px — one column — out of that terminal's allocation. But
  the condition that shows it is VTE's ring overflowing the visible page, and
  that is a function of the same width: hiding the scrollbar widened the
  terminal, the wider terminal rewrapped its ring to fewer rows, the ring
  stopped overflowing the page, and the next frame hid it again. The card (and
  the output row inside it) alternated between two states at frame rate for as
  long as the pane stayed open, burning ~16% of a core; blocks holding a
  transcript that lands near the overflow boundary — anything that entered and
  left the alternate screen, most visibly — reproduced it every time. The cycle
  closes entirely inside GTK, so none of the render-stamp guards on the re-feed
  paths could see it, let alone break it. The scrollbar now rides a non-measured
  overlay instead, exactly as the live card's already does, which removes the
  width edge rather than damping it. A display-backed regression test pins that
  toggling the scrollbar leaves the terminal's allocated width unchanged.

- Block mode no longer flashes a full-screen block on every command. The live
  cell was sized by state, not by content: at `CommandStart` it jumped from the
  ~6-row prompt to the whole viewport and stayed there until the next prompt, so
  the follow-bottom pin pushed every finished block off the top, and the card
  that finally replaced it was only as tall as its output — a page-tall blank
  cell appearing and collapsing around each command, however little that command
  printed. The live card now grows with the output it has actually produced
  (`max(MIN_INPUT_ROWS, rows written)`, capped by the viewport), which is the
  rule ember and frost already use, so history stays on screen and pans up a row
  at a time as output streams in.

  The terminal underneath is untouched: `vte.set_size` still hands a running
  command the full viewport grid — the winsize `pty_grid_size` reports to the
  child, and the rows anything that addresses the screen absolutely (`top`,
  `watch`, a bare `clear`) needs to draw into. Only the *card* is short, via a
  clip: a `gtk::Fixed` inside a non-measured, `Overflow::Hidden` overlay hands
  the terminal its full requested height while the card measures a spacer. GTK
  derives a VTE's grid from its allocation, so nothing else in the widget tree
  can keep the two apart — a ScrolledWindow/Viewport and a plain non-FILL
  overlay child were both measured squeezing the grid to the visible height.

  The row extent is read from the live terminal (top of the screen down to the
  cursor) and latched to a per-command high-water mark, so a `\r` progress bar
  or an `ESC[1A` redraw can never shrink the card under output already on
  screen. A command that clears the scrollback (`ESC[3J`) leaves VTE's
  adjustment and cursor in different coordinate spaces; that is detected and
  falls back to the old page-tall card rather than risk one that hides output.
  With `preserve_live_scrollback = true` the extent saturates the same way.
- Block mode no longer clears and repaints the whole history on every command.
  A finished block's output cap was derived from the space left over *above the
  live input cell*, and that cell grows to the full viewport while a command
  runs and collapses back at the next prompt — so every Enter squeezed each
  finished block down to three rows and expanded it again, re-feeding each
  block's VTE (a `reset` plus a re-render) twice per command. The block count
  was folded into the same change detector, so appending a block re-fitted every
  earlier one as well. The cap is now a constant reserve below the pane's own
  height, matching Forge, and the re-fit key is pure pane geometry: a real
  resize or font zoom re-fits, a command run does not. Measured over a
  five-command session, full-history layout passes went from 14 to 1 and block
  VTE re-feeds from 10 to 0.
- Scrolling the Block history no longer shifts under the reader. Virtualized
  blocks now stay in the document as fixed-height placeholders with their
  contents hidden, instead of being hidden outright and collapsing to zero
  height — which moved the scroll `upper` every time a block crossed the
  viewport edge, so the follow-bottom pin chased the change and blocks flipped
  in and out. Block height metadata now matches what the document really
  allocates, on screen and off.
- A pane resize no longer silently re-collapses a block the user had expanded
  unless the expansion was actually in effect, and no longer rewrites the expand
  button's icon and label on every pass.
- Background session and organism save failures are drained on the UI loop and
  reported before shutdown, with bounded text, per-operation aggregation, and
  a cooldown that prevents a failing filesystem from creating a toast storm.
- A CLI working directory containing non-UTF-8 bytes is rejected explicitly at
  the terminal's String-only boundary. It can no longer be changed silently
  into a different U+FFFD-named directory by `to_string_lossy`.
- Live session capture now shares the decoder's field budgets and session IDs
  consistently accept the configured 1024-byte boundary. Workspace growth is
  stopped with actionable feedback at the restorable 32-tab, 16-pane-per-tab,
  and 64-pane limits; an aggregate-size retry drops optional AI state and then
  replay commands while preserving the tab/pane layout.
- Persistence failures are generation-aware: a recovered write clears only
  older errors, draining an error allows a later failure to be reported again,
  and an older in-flight success cannot hide a newer rejected update.
- File-tree rows retain raw Unix path bytes in a reversible bounded identity
  instead of round-tripping through lossy UTF-8. Distinct invalid byte names no
  longer collide; notebook activation preserves the original `PathBuf`, and a
  normal non-UTF-8 file is refused with feedback rather than inserting a path
  containing U+FFFD. Flatpak notebook execution likewise fails explicitly when
  its working directory cannot cross the UTF-8-only host bridge, rather than
  running in a lossy replacement path.

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

- The AI chat panel now honours `ai_share_command_context` before attaching
  recent shell history to a provider prompt. "Include recent shell context"
  defaulted to on, so with the shipped default (consent off) the last five
  `$ command (exit N)` lines were sent with every question — the terminal
  content the setting promises will not leave the machine without an opt-in,
  and the setting the Codex/agent path already enforced. Both open paths pass
  the same `ai_enabled && ai_share_command_context` projection, the history
  file is not opened without it, and without consent the checkbox is off,
  unclickable and relabelled to name the config key.
- Configuration reads reject group- or world-writable files in addition to
  enforcing ownership, regular-file, link-count, and no-follow rules. This
  closes the local multi-user command-injection boundary for alternate or
  manually chmodded configs that control shells and startup commands.
- Automatic command-correction helpers are resolved only from absolute `PATH`
  entries to canonical executable files whose entire target namespace is not
  writable by the current user, group, or others. The child receives a fixed
  system PATH; Flatpak helper probing fails closed until the host bridge can
  provide the same proof.
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
