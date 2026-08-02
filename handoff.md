# Engineering handoff

Updated: 2026-08-01

This baseline exact-pins the hardened shared core and jagent revisions and upgrades
terminal rendering, history, configuration, persistence, notebook workflows, AI,
and command review. Agent UI actions are now bound to the session epoch, execution
identities are checked rather than wrapped, and workspace snapshots decode under
their own budgets.

## Completed since the previous handoff

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
- Execution generations use `checked_add`. Exhaustion seals the session instead of
  reusing an identity a late completion could attach to.
- A raw model reply over 128 KiB is recorded as a provider failure before parsing
  and before any transcript mutation, so the oversized bytes never reach the
  parser or the transcript, and no model turn is consumed.
- `src/session.rs` decodes both the versioned envelope and the bare legacy
  `SavedSession` through `DeserializeSeed`/`Visitor` implementations that charge
  tab, per-tab pane, total pane, tree-depth, and per-field budgets while decoding.
  `SavedSession`, `SavedTab`, and `PaneLayout` no longer derive `Deserialize`.
  Unknown fields are still ignored so snapshots from other releases restore, and
  `session_within_restore_limits` remains as the post-decode semantic backstop.

## Remaining boundaries

### Adopt the shared atomic claim for the Agent snapshot

`src/agent_ops.rs` still reads the Agent snapshot and removes it as two steps.
`jterm_core::agent::claim_session_file` now provides a one-winner
claim/consume primitive that quarantines unusable evidence; adopt it when the
pinned `jterm_core` revision is advanced, and drop the local read/remove pair.

### Bind execution completions to the epoch as well as the generation

`PendingAgentCommand` carries a checked generation, which is enough to reject a
stale *execution* completion. A model completion arriving after New Task is
rejected by the epoch only where the UI routes a proposal reference; the LLM
reply path still relies on the cancellation token. Carry the epoch into the
in-flight request handle so a reply for a previous task cannot be accepted by a
session that has since been reset.

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
```
