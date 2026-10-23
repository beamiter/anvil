# Agent management

Open **Agent Sessions** from the top-bar grid button, **Ctrl+Alt+Shift+G**, or the
command palette. The window is named **Agent Manager**. It discovers local Codex
and Claude conversations across projects and custom provider-home directories.

## Find and organize conversations

The header offers **Active**, **Needs input**, **Live CLI**, **Pinned** and
**Archived** quick views. Counts honor the current search and project scope.
Needs input is evidence from the last recorded transcript state; Live CLI is
separately observed process evidence. The project selector includes descendant
directories and shows active and locally archived counts. Labels are bounded
for display, while selection retains the original path.

Select a conversation to edit its local name and comma-separated tags. **Save**
commits both fields together; **Revert** reloads the saved values. An empty name
restores the provider title. Names allow 160 UTF-8 bytes; up to 12 tags of 32 bytes
each are normalized, deduplicated and sorted. Invalid input or a failed save
keeps the draft available. Drafts are bound to provider and session ID, so
switching rows, hiding results with a filter or refreshing the catalog does not
transfer or discard another conversation's edits.

**Archive locally** hides a conversation from default browsing. Choose the
Archived view and **Restore to active** to undo it. Pins, names, tags, local
archives and successful resume history are shared across Anvil, Forge, Ember
and Frost on the same account. Local archiving does not delete a transcript,
terminate a CLI or change the provider's own archived state.

Search terms combine with the provider, recorded-state, project, pin and archive
controls. All terms must match; use quotes for phrases and a leading minus for
exclusions:

| Search | Result |
| --- | --- |
| `renderer gui` | Both terms across title, local name, tags, path, branch or preview |
| `provider:codex tag:review` | Codex conversations with the exact normalized tag |
| `title:"release plan"` | A phrase in the provider title or local name |
| `project:/work/repo` | That absolute path and its lexical descendants |
| `branch:feature -tag:done` | A matching branch with the tag excluded |
| `is:live is:pinned` | An observed live CLI that is also pinned |
| `status:waiting` | A recorded request for input or approval |
| `is:archived` | Locally archived conversations, including from the default view |

Malformed quotes and unsupported `is:` values show an error.
Search is over bounded session metadata and previews, not the entire transcript.
**Clear filters** restores active browsing across all projects.

The detail view keeps the original project path, exact resume command and
provider identity visible. Resuming uses that original identity and directory;
the new tab uses the saved local name. A conversation already open in this
window focuses its live tab. Editing or saving organization does not resume it.

## Prioritize native tasks

Enable the experimental Agent Tasks setting to use the **Tasks** panel
(**Ctrl+Alt+Shift+T**). These tasks retain their isolated worktrees, provider,
lifecycle, approval requests, validation and diff review.

Alongside status, provider and repository filters, choose **Needs you first**,
**Recently updated**, **Oldest update**, **Task name** or **Project**.
**Next needs you** cycles only the currently visible tasks requiring input or
review. Selecting or sorting a task does not start or approve it. Follow-up
drafts remain attached to stable task IDs. If a filter hides the current task,
the panel offers to reveal it instead of silently selecting a different task.

## Shared metadata compatibility

Organization lives in the private shared session-library JSON, separately from
provider histories. The core uses bounded reads, a process lock, field-level
merges and atomic replacement. Two terminals changing different fields preserve
both changes; simultaneous edits to the same field use the later saved value.

The first metadata write upgrades version 1 metadata to version 2 while
preserving pins and resume receipts. Older binaries reject the newer metadata
version instead of overwriting it, so update all four frontends together. This
upgrade does not require modifying the provider's own session files.
