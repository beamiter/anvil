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

## Organize several conversations

Check the boxes beside conversations to create a batch selection independently
of the conversation being inspected. **Select visible** adds the current page;
**Clear selection** removes all checks. The counter shows the total selected,
selected on this page, and selected outside the current view. Filtering,
paging, refresh and a completed operation preserve those checked identities.
A batch can contain at most 256 conversations.

Choose **Pin**, **Unpin**, **Archive**, **Restore**, **Add tag** or **Remove tag**.
The tag field takes one tag; an add preserves existing tags and a remove
preserves all other tags. The shared store validates the whole batch and saves
it atomically. A busy store, invalid tag or failed write keeps both the checks
and entered text available. Save or Revert any selected conversation's pending
name/tag edits before changing its tags or archive state.

Batch controls retain the exact provider/session IDs and tag shown when the
interaction begins. A change to the checked selection or tag cancels that
pending interaction. Enter in a name, tag or checkbox never resumes a session.
These actions change local organization only; they do not launch or stop a CLI.

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

## Create work from a project

Choose **New task** in the Tasks panel. Enter a short title, an absolute local
Git repository directory, the provider and the actual goal. The goal supports
multiple lines, up to 16 KiB. An optional validation command is a single shell
command executed at the isolated worktree root with the displayed configured
absolute shell. Enter in an editor does not submit the form.

**Create task** prepares an isolated branch/worktree and registers the task.
It never starts a provider or executes the validation command. The form stays
open while Git prepares the worktree; any preparation or registration failure
shows an inline error with the exact authored draft intact for correction and
retry. Cancel stops this submission from registering late. The authored goal
is not added to the older terminal recovery journals.

The selected-block creation action remains available for failed-command work.
Its exact recorded command/cwd evidence and existing validation rules remain
separate from project-authored goals.

## Inspect, start and review a task

Open **Details** to read the shared task projection: provider, repository,
worktree, branch/baseline, runtime, lifecycle, terminal bindings, validation
result and the authored goal where present. This page is read-only. **Stream**
continues to show actual native activity and turn history; **Diff** shows the
existing baseline-aware review. Details does not fabricate past events.

Native **Start Codex**, **Start Claude** and **Start Kimi** use the existing
provider drivers and preparation/consent gates. **Start terminal** remains
available for compatible provider CLIs, including OpenCode. Authored terminal
tasks require AI and command-context sharing before the local task brief can
be written. The task shows the brief location inside its worktree. Where the
existing launcher cannot carry an initial prompt, ask the CLI to read that
brief; the terminal is never fed synthetic input.

A task with a validation recipe offers **Validate** when the task is ready for
review, and existing passed-validation completion rules apply. For an authored
task without a recipe, the action is explicitly **Accept reviewed work**. It
becomes available only after the task reaches Ready for Review, has no active
Agent stream and has not started validation. Inspect the results and diff
before accepting. Switching tasks cancels a pending button activation, so a
held review action cannot apply to the newly selected task.


## Triage by attention reason and structured task search

Expand **Provider, project and state** to adjust the additional scope controls;
its heading reports how many are active. Search, attention and quick views stay
visible in the compact sidebar.

The **Attention** selector separates pending approvals, requests for a reply,
validation issues, work ready for review, and failed tasks. Each count uses the
current provider, project and text search before applying the state/view
selection. Choosing an attention reason preserves the search and exact project
identity and clears the single-state filter. Choosing a state or ordinary quick
view clears the attention reason;
**Clear filters** resets both. A task can remain selected while hidden, but
hidden details and action controls are not exposed as another task.

Task search now supports explicit fields. Terms combine with AND; enum values
within a field may be separated by commas for OR. Quoted phrases and negative
terms are supported. Expand **Search syntax** for examples.

| Task search | Result |
| --- | --- |
| `provider:codex status:working,review` | Codex tasks working or ready for review |
| `attention:approval` | Tasks awaiting a specific approval decision |
| `validation:failed -runtime:terminal` | Failed validation outside the ordinary terminal runtime |
| `project:repo branch:feature` | Matching project and branch text |
| `"render race" -status:completed` | A phrase with completed tasks excluded |

Malformed expressions and unknown field values show an inline explanation and
zero results. Queries longer than 256 UTF-8 bytes are rejected with an
explanation instead of silently truncating a trailing condition. Enter cannot select an unrelated task from an invalid query.
These task filters do not change execution state or provider transcripts.

## A clear next step and reliable follow-up drafts

The detail header presents the shared next-step explanation and current failure
or question reason. The highlighted action is selected from the runtime's actual
capabilities: start, finish a live native session, run validation, complete a
validated task, or open the relevant output/diff. Pending approvals remain
individual decisions in their existing cards. Guidance never grants permission,
accepts a task, or implies that validation has passed.
A nonempty follow-up draft or a pending submission suppresses the highlighted
Finish suggestion; the explicit secondary Finish control remains available.
Native starts require retained task instructions, and completion remains unavailable
while the native event stream is live.

Changing the selected task or changing a highlighted button's operation cancels
any delayed GTK activation before that control gains its new meaning. Existing
secondary actions and their domain checks remain available under **All task actions**.
The detail pane scrolls independently, so guidance, approvals and follow-up input
remain reachable in smaller windows without clipping the controls.

Sending a follow-up keeps the authored text until the runtime acknowledges the
exact submission. While that attempt is pending, duplicate sends are disabled.
A rejected submission shows its error inline and retains the draft for correction
or retry. Successful acknowledgement clears only the submitted text; a newer edit,
a draft on another task, or a newer retry is never removed by an old receipt.
These in-memory drafts are not written into terminal recovery journals.

## Safer resume, steadier refresh and hidden tasks

**Selecting is not resuming.** A single click only selects and inspects a
conversation. Resume with Enter, a double click or **Resume in New Tab**. Space on
a focused row toggles its batch checkbox and never resumes it.

**A conversation that is already running elsewhere asks first.** When the latest
scan observed a Codex or Claude process already holding the selected
conversation, and it is not open in this window, the first Resume only explains
that a second CLI would write to the same history. **Resume anyway** for the same
conversation proceeds. A second press within about half a second of the warning
is ignored, so a double-click or a quick double Enter cannot confirm it unread.
The confirmation covers that one attempt: if its launch check fails, or Esc
cancels it while the check runs, the next Resume warns again. Selecting another
row, a refresh that removes the conversation, or Esc withdraws the warning. A
conversation open in this window still switches to its tab.

**Refresh never moves your selection.** If a background refresh removes the
conversation you were inspecting (for example, another terminal archived it),
nothing else is selected in its place: the detail pane says it left the view and
Resume stays disabled until you pick again. It is restored if a later refresh
shows it again. When your own change moves it out of the view (archiving it,
restoring it from the Archived view, unpinning it in Pinned, or a name or tags
that no longer match the filters), the pane says so and which view finds it.
Searching or changing filters yourself still selects the first match.

**Exact commands.** The detail pane shows, and **Copy command** copies, the exact
resume command, including an `env CODEX_HOME=…` or `env CLAUDE_CONFIG_DIR=…`
prefix when the conversation belongs to another provider home. The background
readiness check computes it, so it appears a moment after you select a
conversation (Copy command waits for it) and selecting never touches the
project directory or history file on the window's thread. A missing CLI still
leaves the command copyable; a missing project or history explains why there is
none.

**Saving is distinct from refreshing.** The shared library is reloaded in the
background to pick up other terminals' changes; that no longer disables Save,
Pin, Archive or the batch buttons or flashes "Saving". A save that fails stays
reported even if a reload finishes first. Names, tags and the batch tag are
checked against the shared byte limits while you type, and Save or Add/Remove tag
stays disabled with the reason shown.

**Refresh cadence.** Background rescans start five seconds after the previous
scan finished, so a large history never triggers back-to-back rescans. Opening
the manager, **Refresh** and F5 all rescan history, reload shared organization
and recheck the selected conversation's launch.

**Esc** first leaves a name, tag or batch-tag field, then withdraws a pending
"Resume anyway" warning, then warns once about unsaved name/tag drafts; only then
does it close the manager. Archive views, sort orders and recorded states use the
shared labels; the provider's own archive appears as **Archived by provider**.
The overview counts live CLI conversations rather than process IDs.

**Hidden tasks can be restored.** **Hide** (formerly Archive) removes a finished
or idle task from every task view without touching its worktree. It is disabled,
with the reason in its tooltip, while the Agent or a validation is still running.
When tasks are hidden, **Hidden tasks (N)** lists them; select one and choose
**Restore task** to return it to the state it had when hidden. The hidden view
offers only Restore and read-only inspection. It gives way to the ordinary views
when you open the panel, create a task or restore one, and once nothing is
hidden, so the task you are working with is never invisible. Hiding keeps a
native Agent's transcript and recovery evidence, so a restored task shows them
again.

**Attention at a glance.** A Tasks button in the top bar shows how many visible
tasks need you, with the shared summary (for example `2 tasks need you:
1 approval, 1 reply`) in its tooltip, and toggles the panel like
Ctrl+Alt+Shift+T. It appears when agent tasks are enabled or tasks exist. Task
messages name the actual provider (Codex, Claude, OpenCode or Kimi), and
notifications show paths and errors literally.

## Shared metadata compatibility

Organization lives in the private shared session-library JSON, separately from
provider histories. The core uses bounded reads, a process lock, field-level
merges and atomic replacement. Two terminals changing different fields preserve
both changes; simultaneous edits to the same field use the later saved value.

The first metadata write upgrades version 1 metadata to version 2 while
preserving pins and resume receipts. Older binaries reject the newer metadata
version instead of overwriting it, so update all four frontends together. This
upgrade does not require modifying the provider's own session files.
