# Block workspace navigation and triage

Anvil and Forge provide the same search, selected-output actions, and geometric
pane navigation. Their GTK ownership and dispatch code remain frontend-specific.

## Browse retained commands

Open **Search Blocks** (`Ctrl+Shift+G`) with an empty query to browse the retained
history immediately. Filters are optional. Each eligible block contributes one
representative row, in terminal order, up to the existing 500-result cap.
`Failed`, `Slow`, `Bookmarked`, and `Background` compose before that cap.

`All` prefers the command's first meaningful line. `Cmd` lists command rows.
Neither needs to inspect a command's retained output to build that browse row;
a large output cannot consume the byte budget reserved for subsequent command
metadata. `Out` uses the first meaningful retained output line and continues to
spend the output scan budget. Commandless background records in `All` also need
retained output; no synthetic command or output is invented when it is absent.

Typing a query restores ordinary literal/regex matching. The existing case,
whole-word, scope, bookmark, refresh, exact navigation, and snapshot fallback
rules apply in both modes. Clearing the query returns to browsing. Reset clears
the query and search controls and then browses the retained history again.
Running output is not promoted into completed-record browse results.

Each pane retains one prepared search expression. Continuation slices and
refreshes with the same query and matching options reuse its compiled regex;
invalid regex results are reused too until the query or options change. The
cache stores no terminal output, is never shared between panes, and disappears
with its pane. The 8 KiB query-byte limit is enforced inside the search API as
well as in the dialog, before allocation or compilation, alongside the
independent regex compilation size budget.

Search scans cooperate with the GTK loop. The shared palette slice aims to
yield after 8 ms; live Find uses a 4 ms target and a smaller byte allowance.
These are cooperative checks between units of work, not guaranteed frame
latency limits. Regex compilation, an individual text operation, and GTK row
construction can take additional time. Existing continuation cursors and
search-generation checks still cancel obsolete scans when the user edits the
query, changes scope, or closes the dialog.

## Fold the explicit selection

Select finished cards and open the command palette. Two actions make a mixed
selection deterministic:

| Action | GTK configuration key | Shared action alias |
| --- | --- | --- |
| Collapse selected output | `collapse_selected_blocks` | `block:collapse_selected` |
| Expand selected output | `expand_selected_blocks` | `block:expand_selected` |

Both actions have no default shortcut. For example:

```toml
[keybindings]
collapse_selected_blocks = "F8"
expand_selected_blocks = "F9"
```

The whole selected identity set is validated before any card changes. If
retention removed any selected card, the operation refuses the batch and asks
for a fresh selection; it cannot partially affect a different set. Empty
selection does not fall back to the newest card. A no-output card or a card
already in the requested state stays unchanged. The completion notice reports
the number of cards that actually changed.

The selected ids, active edge, range anchor, keyboard focus, live typed input,
and running command state remain owned by their existing controls. Only
finished Block/Chat output is eligible; conventional or Unified VTE surfaces
and alternate-screen ownership cannot be folded through these actions.
Virtualization heights are updated in one batch; filtered cards keep their
zero document-height sentinel. Fold state remains local to the running pane
and is not saved across restarts.

**Collapse all blocks**, **Expand all blocks**, and **Collapse or expand block**
retain their existing targets. The single-card toggle still targets the active
selected card, or the newest finished card when there is no selection.

## Move between uneven splits

Directional pane focus uses `jterm_core::pane_layout::directional_focus_target`,
the same selector as Ember and Frost. It compares pane edges and perpendicular
span overlap, so an uneven nested split selects an adjacent pane along the
requested direction. Only mapped widgets with valid, nonzero geometry are
eligible. Moving beyond the outer edge leaves focus in place, and zoom-hidden
panes cannot take focus. The active-pane header refreshes with the focus change.
Existing `Ctrl+Alt+Arrow` and `Ctrl+Alt+H/J/K/L` bindings stay available.

## Focused regressions

The pure tests exercise repeated/invalid regex preparation, byte admission,
exact selection validation, keybinding aliases, and large-output browse budgets
against the production code. Run:

```sh
cargo test --locked prepared_search
cargo test --locked selection_folding
cargo test --locked selected_output_actions
cargo test --locked empty_query_browse_does_not_spend_output_budget
```

The native test uses real GTK/VTE finished cards and a nonexecuting PTY. It
checks unfiltered empty-query browsing, the direct-call query limit, mixed
output/no-output selections, repeat operations, stale identities, focus and
input preservation, and alternate-screen refusal:

```sh
xvfb-run -a cargo test --locked selected_output_batch_preserves_live_input_focus_and_exact_selection -- --ignored --test-threads=1
```
