# Engineering handoff

Updated: 2026-08-10

This baseline exact-pins the hardened shared core and jagent revisions and now
keeps session persistence plus Palette workflow/history reads off the GTK
thread, makes search state visible across both terminal backends, removes the
default Nerd Font dependency, and preserves raw Linux file-tree path identity.
Agent UI actions, model replies, and terminal execution events remain bound to
the session epoch; workspace snapshots enforce the same budgets while being
captured, queued, written, and restored.

## Completed since the previous handoff

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
  path, and the parent namespace is synced before a claimed session is accepted.
  `jterm_core` is pinned to `48d25f155b960417609ffc85a98b7c9ba44c5772`
  (transitively jagent `a09fd1563b862f96bed7047834720aeb31c163e2`).

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
