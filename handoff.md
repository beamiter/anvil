# Engineering handoff

Updated: 2026-08-26 (Block Search 3.3)

This baseline exact-pins the hardened shared core and jagent revisions and now
keeps session persistence plus Palette workflow/history reads off the GTK
thread, makes search state visible across both terminal backends, removes the
default Nerd Font dependency, and preserves raw Linux file-tree path identity.
Agent UI actions, model replies, and terminal execution events remain bound to
the session epoch; workspace snapshots enforce the same budgets while being
captured, queued, written, and restored.

## Completed since the previous handoff

- **Block Search 3.3 (2026-08-26)**: Shift+Enter now keeps the GTK palette
  open, restores query focus, and advances after a successful live jump only.
  Snapshot results retain their dedicated view and unavailable results keep
  their row plus diagnostic, so continuous review never pretends it navigated.

- **Block Search 3.2 (2026-08-26)**: the GTK palette now wraps arrow
  navigation, supports Home/End plus ten-row PageUp/PageDown moves, scrolls the
  selected row into view without moving query focus, and exposes current/total
  position through an accessible status label.

- **Block Search 3.1 (2026-08-26)**: the cross-block palette now has `All /
  Cmd / Out` surface scopes plus a `Ctrl+O` cycle. Scope filtering runs before
  the 500-hit cap and composes with failed/slow predicates without changing
  the scan-to-VTE highlight contract.

- **Block Search 3.0 (2026-08-26)**: cross-block search now composes `Aa`
  case sensitivity, regex, and Unicode whole-word matching. One typed options
  value drives both the bounded record scan and the activated VTE/PCRE2
  highlighter, closing the old scan/jump interpretation split. `Ctrl+I`,
  `Ctrl+R`, and `Ctrl+W` expose all three controls without moving focus. The
  formerly unbounded dialog query now fails visibly above 8 KiB, and Rust-regex
  compilation has a 2 MiB heap ceiling before retained records are scanned.

- **Single-interpretation session restore (2026-08-25)**: every bounded legacy
  workspace, pane-layout, and versioned-envelope decode now runs through
  `jterm_core::bounded_json::validate_no_duplicate_members` before the existing
  allocation-budgeted seeds. This closes the last-wins behavior inherent in
  the hand-written `MapAccess` visitors for repeated `sid`, cwd, layout,
  payload, or supersession fields, and also rejects escaped-equivalent names
  and duplicates inside ignored future objects. The private serde_json RawValue
  sentinel is reserved so feature unification cannot reopen unchecked JSON.
  The shared preflight retains no decoded value tree and never reflects an
  untrusted member name.

- **Shared jsh session identity (2026-08-25)**: configured and persisted pane
  identities now use `jterm_core`'s exact 1..=128-byte ASCII
  `[A-Za-z0-9_-]` contract. Unicode, dotted, spaced, or otherwise
  grammar-invalid ids within the field budget safely degrade to no claimed id,
  while an over-budget field still rejects the snapshot at the bounded decoder;
  neither case can forward an invalid identity to `jsh`.

- **Core-owned Agent claim durability (2026-08-25)**: the exact core pin is now
  `21437ba6f0cb85e74d4ce2a03ef1857de2c55d9d` (transitively jagent
  `a462ec81f3a4c6ad85a455780ced232172f127ea`). Core durably retires the public
  snapshot name before exposing `SessionClaim::Restored` and owns the later
  cleanup sync, so anvil removed its redundant post-restore sync failure gate
  and test-only injection seam. Cargo and Nix source identities move together.

- **Exact search cursor identity and core repin (2026-08-24)**: Block card
  surfaces carry render stamps through the backend/find contract, so a resize,
  fold, or output re-feed invalidates and rebuilds the retained query before
  navigation—including a one-hit pass whose logical cursor does not move.
  Cross-block rows carry their first surface occurrence and activation reaches
  it exactly or fails closed at the 4096-step bound. That round pinned
  `jterm_core` `0f47569`; the current exact pin is recorded above.

- **Exactly-once command lifecycle closure (2026-08-21)**: Block and Unified
  now share one observer-side `C -> finish` latch. An accepted `D` consumes it
  with `shell_reported` evidence; if `D` is lost, only a foreground-shell `A`
  consumes it with `boundary_inferred`/`degraded` evidence and `None` for exit
  status and duration. The inferred fan-out runs before that same `A` finalizes
  the backend record, preserving the normal `D -> A` ordering. Repeated `A`,
  background output, prompt-owned alternate screens, and RIS cannot mint a
  finish without an accepted `C`; RIS remains invalidation, not completion.
  The running-command display copy, engine-owned command identity, and Agent
  correlation remain available for prompt-trust rollback and later
  Block/Unified finalization.

- **Agent-validation hardening repin (2026-08-16, seventh round)**:
  `jterm_core` repinned to `cf0dd2c9cd369c1d8113eadde0ec6254d3fb81b1`.
  Core's pre-restore validation now enforces the stricter audit rules forge
  used to keep local (pending-must-be-final-turn,
  approved-requires-observation-or-note, turn-counter arithmetic,
  final-turn/state matching — verified against every reachable jagent live
  shape, including the AwaitingObservation normalization round-trip), and
  claimed Agent snapshots are read with `read_bounded_private`, so a
  group-readable snapshot quarantines as tampering. Also adds a block-view
  integration test pinning that a reset aborting a parser-owned OSC fires
  its barrier exactly once, and a comment documenting the capability
  observer's deliberate ESC-leniency divergence from the parser.

- **Strict-ST parser repin (2026-08-15, sixth round)**: `jterm_core` repinned
  to `73c1411f23ea41626187013133f1e2c27620ae94`, which adds the
  `AgentIntegrationReady`/`EraseScrollback`/`HardReset` ParserEvent barriers
  and unifies the parser on strict ST-only termination for APC/DCS/PM/SOS with
  abort-and-reprocess on ESC + non-ST (OSC keeps the BEL convention).
  `handle_event` gained the three arms: `EraseScrollback` →
  `backend.erase_scrollback()`, `HardReset` → `on_hard_reset()`,
  `AgentIntegrationReady` ignored (the raw `ShellCapabilityObserver` remains
  the trust authority because it advances in reset-splitter order ahead of
  dispatch). The splitter's `Reset` part no longer invokes those handlers
  itself — core emits each barrier exactly once when the part's exact bytes
  are fed, so the old direct call would have doubled every reset; the part
  survives purely as the ordering barrier that interleaves capability
  observation with the reset wipe. Behavioral change to note: an aborted OSC
  no longer fires its payload, so a malformed OSC 133 can no longer produce
  bogus PromptStart/CommandStart marks (fail-closed). Core also consumes OSC
  7771 now (never forwarded to the VTE); the observer still sees the raw
  bytes pre-parse, so capability trust is unchanged. Stale "pinned core
  predates / does not frame" comments were corrected; the APC capture path
  stays local pending a dedicated equivalence check.

- **text_safety removed (2026-08-15, fifth round)**: `jterm_core` repinned to
  `592d6632b7f51239c0d7ece7dc1796e708fab400`, whose `review_input` spoof table
  now covers `\u{fff0}..=\u{fff8}` and the full `\u{e0000}..=\u{e0fff}` tag
  plane — exactly anvil's old local superset — and which adds the bounded
  `safe_inline_display`/`safe_multiline_display` sanitizers. All
  `bounded_display_text` call sites moved to the core helpers (the one
  variable-multiline site in `agent.rs` branches locally), and
  `src/text_safety.rs` is deleted. Accepted cosmetic change: the truncation
  suffix is now a bare `…` (appended when `max_bytes >= 3`) instead of
  `… [truncated]`. `read_history_checked` stays for one policy difference
  (unsafe cwd kept and sanitized at display time); its spoof check now rides
  on the identical core table.

- **Inherited-environment freeze (2026-08-15, fourth round)**: anvil already
  consumed `jterm_core::child_env` but never wired the freeze; now
  `capture_inherited_environment()` is the first statement of `main` (every
  `set_var` — `ANVIL_CONFIG`, the input-method writes — runs strictly after;
  capture failure is fatal). The PTY spawn builds children from
  `envp_from_captured`, and executable resolution now reads `PATH` out of the
  frozen block instead of the live process environment, so resolution cannot
  diverge from the PATH the child will actually run with. The VTE spawn pairs
  `vte_envv_from_captured` with `VTE_SPAWN_NO_PARENT_ENVV_BITS` (OR'd in
  numerically — gtk-rs predates the named flag), stopping libvte from merging
  the live GTK-mutated environment back in; its failure path reports a launch
  failure in the terminal like the existing async errors. Adversarial review
  verified capture ordering on every path, the frozen-PATH semantics against
  `execvpe`, and the flag's bit value against the system libvte headers; it
  found only comment nits (fixed). Accepted test weakness, same as forge's:
  the spawn tests tolerate an `AlreadyExists` capture race, so the test
  binary's snapshot can contain another test's env mutation (no assertion
  depends on it).

- **Repin round (2026-08-15, third)**: `jterm_core` repinned to
  `04f63283090591d9ad88500224e848dbb69b1f61` (picks up `helper.rs`,
  `link.rs`, `bounded_json.rs`, `command_history::prepare_path`, and the
  upstreamed `read_recent_with_status`). The palette's stale
  "pinned core predates these inode checks" comment was corrected — twice:
  the first replacement cited the 256 KiB command cap, which core's reader
  actually enforces identically; the local `read_history_checked` in fact
  remains for `text_safety`'s wider spoof set and for keeping records with an
  unsafe cwd (sanitized at display time instead of dropped during the read).
  Deleting it in favor of core's `read_recent` plus a post-filter is viable
  but would change those two policies; deferred deliberately.

- **config_store test fixtures are umask-proof (2026-08-15, second half)**:
  the 11 tests that wrote `config.toml` fixtures with plain `fs::write` now go
  through a `write_fixture` helper that chmods `0600` after writing, so the
  full suite passes under the default `umask 002` as well as `077`. Test-only
  change; mode-matrix and permission-rejection tests that set explicit modes
  were deliberately left untouched.

- **Architecture unification round (2026-08-15)**: the last local duplicates
  of core modules are gone, and `jterm_core` is repinned to
  `1b7598de5530b7b8ca39582a77610b22987f66bc`. `src/snapshot_file.rs` and `src/atomic_file.rs`
  were deleted — organism memory now reads through the new
  `jterm_core::snapshot_file::read_bounded_private` (any owner-only mode
  accepted, group/other bits rejected on the open descriptor) and all atomic
  writes use `jterm_core::atomic_file`. `src/command_correction.rs`'s
  hand-rolled `waitid(WNOWAIT)`/group-signal probe was replaced by the newly
  public `jterm_core::supervised::SupervisedChild`; on probe/reap failure
  paths the worker detaches its output reader instead of joining it, because
  a disarmed (unsignalled) group can hold the pipe open forever.
  `src/text_safety.rs` stays: its spoof table deliberately extends core's
  with `\u{fff0}..=\u{fff8}` and the full `\u{e0000}..=\u{e0fff}` tag plane,
  and `bounded_display_text` has no core equivalent. Adversarial review of
  the round also caught the reader-join hang and a stale comment claiming
  `\u{ffa0}` was anvil-only (core already covers it). Still local by design:
  `src/notebook.rs`'s long-lived terminal children keep their own wait logic
  (`SupervisedChild` scopes itself to short-lived helpers).

- ASCII organism frontend parity now includes the five-part embodiment pass:
  visible juvenile/adult/seasoned phenotypes through a composable render
  context; content-free busy/waiting/resumed output rhythm; four-frame semantic
  bridges in Full motion; process-local repo territory habits; and a
  window/session attention arbiter with shared focus, cue-local cooldowns, and
  no replay queue. The implementation keeps Anvil's Relm4 ownership model while
  matching Forge's reducer and visible semantics. No memory schema or
  content-bearing perception was added; typing, alternate screen, and geometry
  failure still preempt every live expression.

- Workflow discovery is a startup-prewarmed, single-flight cache refresh; the
  Palette presents immediately and accepts only the matching asynchronous
  history snapshot for its current opening. Slow results from a closed or
  replaced dialog are discarded, and worker failures leave actions/workflows
  available with visible status instead of freezing GTK.
- Background persistence failures are surfaced during the session through a
  bounded, aggregated, per-operation rate limiter. The shutdown drain remains
  as the final catch for failures produced after the last UI tick.
- CLI cwd validation stops raw non-UTF-8 paths before the terminal's UTF-8
  launch boundary, eliminating U+FFFD path substitution. Config reads now
  reject group/other write bits, and automatic command-correction helpers use
  canonical non-writable system targets (Flatpak probes fail closed).
- Source installation, release installation, documentation, and uninstall now
  converge on `${PREFIX}/bin`; legacy `~/.cargo/bin/anvil` copies receive a
  migration warning only. Top-bar, file-tree, remote-host, AI, and Agent
  icon-only controls also expose explicit and state-aware AT-SPI labels.
- `src/session.rs` captures only budget-valid pane/tab fields and queues owned
  snapshots through `src/persistence.rs`. JSON work, atomic replace, both fsyncs,
  claim cleanup, and pruning run on a dedicated capacity-one coalescing lane;
  shutdown stops both lanes under one deadline and joins session persistence
  before ordinary history/organism work. Runtime growth stops before exceeding
  the 32-tab / 16-pane-per-tab / 64-pane restore envelope, and a 4 MiB aggregate
  overflow retries without optional AI state and then replay argv while keeping
  the workspace structure.
- Persistence failure suppression carries a monotonic attempt number. A success
  clears only failures no newer than itself, drained targets can report a later
  failure, and a completed old write cannot erase a newer enqueue rejection.
- Find-in-terminal has a backend-neutral status contract. Block search keeps a
  deterministic navigation sequence; VTE uses native PCRE2 as the search
  authority and a bounded
  10,000-row / 2 MiB Rust-regex mirror for counts. Bounded, PCRE2-only, or
  cross-engine regex totals are marked with `+`; switching the active pane
  clears the old backend and replays the open query. The bar exposes counts,
  regex errors, and accessible previous/next/close controls.
- File-tree model rows store versioned bounded hex for the original Unix path
  bytes. Non-UTF-8 names display as escaped bytes without collisions, notebook
  paths stay as `PathBuf`, and prompt insertion rejects paths that cannot cross
  the application's UTF-8 review boundary safely. A Flatpak notebook cell also
  fails explicitly when its raw cwd cannot cross the host bridge, rather than
  executing under a U+FFFD replacement path.
- The default font is `Monospace 14`; Block UI status/action PUA glyphs are GTK
  symbolic icons or text with accessible names. The Settings font list injects
  the active configured family when Pango does not enumerate it, preserving
  custom Nerd Font configurations during size changes.

- Completed block outcome now delegates to the pinned
  `jterm_core::block_contract` after `resolve_command_text` has combined OSC 133
  metadata with the bounded screen fallback. Renderer-owned `BlockStatus` and
  persisted records remain local, while cards, failure markers/navigation, and
  exact-exit filters share the core four-way result. Classification consumes the
  raw `Option<i32>` before the legacy i32-only history/notification/Agent
  adapters synthesize a value, so commandless output carrying a stray nonzero
  status cannot enter a failure or exact-exit result.

- `[[remote_hosts]]` gained `deploy` ("off" by default, "persist", or
  "incognito"). With it on, a remote tab runs `jterm_core::jsh_remote`'s vendored
  `jsh-remote.sh`, which places a verified static jsh on the destination for the
  life of the session and removes it afterwards — so blocks, cwd tracking, exit
  codes and the Commands timeline work on a machine nobody prepared, without
  anything being installed there, without root, and without touching the
  destination's `.bashrc`, `.profile`, or login shell. `remote_shell` is ignored
  in that mode. An unrecognised spelling rejects the host and is reported by
  config validation; it deliberately does not fall back to "off", because the
  difference between the modes is whether the destination's `$HOME` is written
  to. `build_remote_argv` splits into `build_deployed_argv` (pure, given a
  launcher path) and the publish step, so the argument order is asserted in
  tests without writing into the real cache directory, and a failure to publish
  degrades to plain ssh rather than refusing the tab.

- Every Agent UI action carries `AgentProposalRef { epoch, id }` instead of a
  transcript index. `src/agent.rs`, `src/agent_ops.rs`, `src/app_msg.rs`, and
  `src/main.rs` all route the pair, and `resolve_proposal` refuses an action
  raised against an earlier task generation — a stale click, a stale edit dialog,
  or a queued message after New Task or a session replacement. The edit dialog
  also forgets its target on close, so a Submit cannot approve a stale proposal.
- LLM callbacks capture `AgentSessionEpoch` and route it through `AgentLlmReply`.
  Both the reply handler and the post-`ask` handle writeback require the exact
  epoch, so a callback queued before New Task or session replacement cannot
  mutate the new transcript or install its handle there.
- Terminal execution identity is `AgentExecutionRef { epoch, generation }`, not
  a naked counter. The typed reference passes unchanged through
  `PendingAgentCommand`, `VteInput`/`VteOutput`, the block view's armed and active
  one-shot slots, `AppMsg`, and completion/start-failure handlers. Every accepting
  edge compares both fields; manual block completions remain the `None` path and
  still become untrusted context. Generations use `checked_add`, and exhaustion
  seals the session instead of reusing an identity.
- A raw model reply over 128 KiB is recorded as a provider failure before parsing
  and before any transcript mutation, so the oversized bytes never reach the
  parser or the transcript, and no model turn is consumed.
- `src/session.rs` decodes both the versioned envelope and the bare legacy
  `SavedSession` through `DeserializeSeed`/`Visitor` implementations that charge
  tab, per-tab pane, total pane, tree-depth, and per-field budgets while decoding.
  `SavedSession`, `SavedTab`, and `PaneLayout` no longer derive `Deserialize`.
  Unknown fields are still ignored so snapshots from other releases restore, and
  `session_within_restore_limits` remains as the post-decode semantic backstop.
- Agent snapshot restore uses `jterm_core::agent::try_claim_session_file` while
  holding the private parent lock. The public name is consumed once, invalid
  evidence is quarantined, typed claim-acquisition errors retain the public
  path, and core syncs the retired public namespace before a claimed session is
  accepted. Anvil deliberately adds no second post-restore durability gate.

## Remaining boundaries

The current session transaction protects cooperating anvil writers with locks,
revision checks, atomic replacement, and durable directory sync. As with most
Unix editors, a non-cooperating external process can still replace a watched
configuration path outside that protocol; conflict detection is advisory at
that boundary.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
```
