# Engineering handoff

Updated: 2026-08-01

This baseline exact-pins the hardened shared core and jagent revisions and upgrades
terminal rendering, history, configuration, persistence, notebook workflows, AI,
and command review.

## Remaining boundaries

### Bind every Agent UI action to the session epoch

The UI still routes approval/edit/reject through transcript indexes. Carry
`(AgentSessionEpoch, ProposalId)` through `src/agent.rs`, `src/agent_ops.rs`,
`src/app_msg.rs`, and `src/main.rs`, and reject stale clicks and async completions
after New Task, restore, or session replacement. Add stale-card and stale-edit tests.

### Make local generations and model-input limits fail closed

Replace `wrapping_add` execution generations with checked, non-reusable counters.
Apply an app-level 128 KiB cap to a raw Agent model reply before parsing or transcript
mutation. Test `u64::MAX` exhaustion and the first byte over the reply limit.

### Decode workspace snapshots with schema-aware budgets

`src/session.rs` has a 4 MiB file cap and post-decode validation, but both current and
legacy `SavedSession` formats are fully deserialized before tab, pane, tree-depth,
argv, field, and cumulative limits apply. Implement shared bounded visitors for both
formats and add wide-array, deep-tree, escaped-text, and cumulative-argv tests.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
```
