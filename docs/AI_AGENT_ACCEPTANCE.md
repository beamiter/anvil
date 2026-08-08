# AI and Shell Agent acceptance checklist

This checklist covers the Forge-aligned AI command review, Shell Agent, and
review-first correction flows. Use Block mode with shell integration enabled so
finished commands report reliable prompt and exit metadata.

## Setup

1. Configure a working provider in Settings → AI, or use Ollama locally.
2. Keep `ai_enabled = true` and `agent_enabled = true`.
3. Start anvil from the repository with `cargo run` and open a clean Block prompt.
4. **AI command correction** defaults on. Confirm it is enabled in Settings or
   set `command_correction_enabled = true` explicitly.

## Inline `?` command suggestion

1. Press `Ctrl+Shift+P`, enter `? list the five largest files here`, and submit.
2. Verify an inline card appears in the initiating pane immediately with the
   captured cwd, shell, and selected-Block context (when one was selected).
3. While loading, press **Stop**. Verify the card stays present and offers
   **Retry**, with no late result appearing.
4. Retry and verify the result provides **Copy**, **Regenerate**, and **Insert
   for review**. Copy must not alter the prompt; Insert must not press Enter.
5. Edit the command and verify the risk label updates. A multiline paste,
   terminal control, bidi/invisible mark, empty command, or edit over 256 KiB
   must be rejected rather than truncated into an executable prefix.
6. Start another suggestion, switch panes before it completes, and verify the
   result remains attached to and inserts only into its initiating pane.
7. Press `Escape`, close the pane, disable AI, and hot-reload `ai_enabled =
   false` in separate runs. The card/request must disappear cleanly.

## Shared review behavior

1. Compare an AI suggestion, a correction, and an Agent proposal. All three
   should use the same editable command field, **Copy** action, live risk text,
   narrow-width action wrapping, and validation feedback.
2. Enter a visibly dangerous command such as `rm -rf /` only for UI inspection.
   Insert-only cards must remain insert-only, and a verified correction must
   downgrade to insertion. An Agent **Approve & Run** action must require the
   second destructive confirmation; cancel it and do not execute the example.
3. Navigate each card with only `Tab`, `Shift+Tab`, `Enter`, and `Escape`.
   Accessible names should announce the command field and status feedback.

## Shell Agent

1. Press `Ctrl+Alt+G`. Verify the dashboard shows provider/model identity,
   pane cwd, review-required status, prompt readiness, turn budget, and a
   settings button.
2. Select a finished Block, choose **Attach selected Block**, and verify the
   dashboard names its command/exit status and explicitly marks truncated output.
3. Send a simple task. Verify **You**, **Thought**, **Say**, error, and lifecycle
   events appear as separate Block-style messages rather than inside a nested
   transcript panel.
4. On a proposal, edit it and test **Copy**, **Insert only**, **Reject**, and
   **Approve & Run** in separate tasks. Only explicit approval may submit a
   command; Insert only must leave it at the prompt.
5. Stop an in-flight model request and retry it. A stopped or stale reply must
   not mutate the active task.
6. Close the dashboard and reopen it. Existing activity Blocks should remain.
   Choose **New task** and verify prior activity remains visible while the model
   context and turn budget reset.
7. Open the Agent settings dashboard. Toggle command correction and verify the
   safety row states that automatic execution is retired. Open full AI settings
   and verify the same retired switch is disabled there.
8. Resize the pane to roughly 360 px wide. Header metadata and action buttons
   must wrap or ellipsize without clipping the approval control; the composer
   and context controls must remain reachable.

## Review-first correction

1. With correction enabled, run `git statsu` in a Git repository. Git's target
   “most similar command” output should produce an unverified, insert-only
   `git status` review card without using AI.
2. In a Block-integrated shell that emits a plain `command not found` (for
   example Bash), run a safe typo such as `gti status` on a system where `git`
   is on `PATH`. The bounded local PATH matcher should propose `git status`
   with **Run verified command**. Copy must not run it. If the shell itself
   prints a concrete alternative first, that target-output suggestion correctly
   remains insert-only instead.
3. Edit the verified command and verify its primary action immediately becomes
   **Insert for review**; returning exactly to the safe verified text restores
   **Run verified command**. Insert must not press Enter. If testing Run, use a
   harmless command and verify it submits exactly once.
4. On a Debian-like host, run a harmless misspelled package lookup/install in a
   disposable environment and verify local APT evidence is marked verified.
   Do not install anything merely for this test.
5. Verify **Copy**, **Dismiss**, and `Escape`. A remote, target-output, or AI
   suggestion must never expose the verified-run action.
6. Disable correction and repeat the failure; no correction should appear.
7. Close the target pane, open Agent, or disable AI while a fallback request is pending.
   No stale card or reply may attach to another pane.
8. Fail an unrelated command that does not match command-not-found,
   unknown-subcommand, or invalid-option evidence. No speculative correction
   should appear.

## Automated regression suite

Run:

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build
git diff --check
```

When reporting a UI issue, include the step, provider, shell, pane width,
expected result, actual result, and a screenshot if layout is involved. Do not
include API keys or sensitive terminal output.
