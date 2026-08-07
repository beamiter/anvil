# Engineering handoff

Updated: 2026-08-08

This baseline exact-pins the hardened shared core and jagent revisions and upgrades
terminal rendering, history, configuration, persistence, notebook workflows, AI,
and command review. Agent UI actions, model replies, and terminal execution
events are now bound to the session epoch; execution counters are checked rather
than wrapped, and workspace snapshots decode under their own budgets.

## Completed since the previous handoff

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
- Agent snapshot restore uses `jterm_core::agent::claim_session_file` while
  holding the private parent lock. The public name is consumed once, invalid
  evidence is quarantined, and the parent namespace is synced before a claimed
  session is accepted.

## Remaining boundaries

### Bound the remaining restored strings at their source

Decoding bounds `title`, `mode`, `cwd`, `remote_name`, and `sid`, but the
constants live in `src/session.rs` while the same shapes are produced by the
capture path. A shared constructor that cannot build an over-budget `PaneLayout`
would make the decoder's limits provably reachable rather than duplicated.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
```
