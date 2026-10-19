# Unified navigation

Unified keeps one persistent VTE for the prompt, running output, and retained
scrollback. Its navigation row is a separate, compact layout row above that
surface. It never covers terminal text. At narrow pane widths, Find, Browse,
and Live use icons with tooltips and accessible names. The controls remain
keyboard-focusable; clicking them does not take keyboard focus merely because
the button was pressed.

## Read history and return to input

**Live** means the native VTE viewport is at the live end, allowing its original
one-row rounding tolerance. **Reading history** reports a viewport above that
end. The tooltip gives the distance in rows, and the row counts foreground
commands that finish while you read. Commandless background output does not
inflate the completed-command count. Returning to the live end clears it.

Both native adjustment value changes and range changes update this state.
The outer layout scroller has no authority over Unified history. Immediate
scroll bookkeeping is constant work; display refreshes are merged into one
pending GTK idle callback. No history search or regex compilation runs during
scroll status updates, and the old floating Block return button is replaced
by the permanent navigation row in this mode.

**Return to live** scrolls the actual VTE adjustment to its bottom, restores
terminal keyboard focus, clears the command navigation cursor, and resumes
any output held during native text selection. It does not submit or replace
the current input. Wheel scrolling, native selection, readline, and the PTY
remain under their existing owners.

## Move between commands

At Live, **Previous command** selects the newest completed foreground command.
With an explicitly selected retained record, Previous and Next move through
completed commands in terminal order. They do not wrap. Previous at the oldest
record gives a boundary notice; Next after the newest returns to live input.

Native scrolling retires that explicit command cursor. While reading history,
the next navigation request instead chooses the nearest proven command head
in the requested direction relative to the current viewport. A range-only
change does not discard the cursor. Find clears it explicitly, including when
another match lies on the same screen and VTE need not scroll.

Every jump revalidates the existing authenticated zone proof. Unavailable or
expired row authority never becomes a guessed line number. An explicit
retained record can use the existing read-only output snapshot fallback;
navigation without a provable nearby head asks you to use Browse. Multiple
commands near the bottom can share a clamped scroll position: explicit record
identity still advances between them. A retained record removed by the history
limit cannot silently retarget another command.

**Browse** opens this pane's completed-command browser. An empty query lists
commands immediately; metadata filters and text search remain available.
**Find** opens the existing text search for the clicked pane. When a command
finishes or the surface resets, an open Find is invalidated so Next/Previous
rebuild the visible query instead of retaining a dead match cursor.
This includes an earlier zero-hit query when new output adds its first match.
Unified's complete native search domain follows its viewport/history wrap
plan; it does not require the separate running-card anchor used by Block/Chat.

## Copy and keyboard controls

**Copy current command** uses only the explicitly selected retained record,
chosen by command navigation or Browse. It does not copy an inferred viewport
command, change the normal VTE text selection, paste, or execute anything.
The complete recorded command is copied with its original newlines and tabs.
Truncated command text, hidden terminal/visual controls, blank commands, and
commands over 256 KiB are refused rather than copied partially. A screen-derived
record remains screen-derived; copying does not upgrade its provenance to
shell-reported text. Output snapshots keep their existing bounded/truncated
labels and are not presented as complete output by this control.

These actions are in the command palette and may be assigned in `[keybindings]`:

| Palette action | Configuration key | Default shortcut |
| --- | --- | --- |
| Previous command | `previous_command` | None |
| Next command | `next_command` | None |
| Return to live | `return_to_live` | None |
| Copy current command | `copy_current_command` | None |
| Compare command output | `compare_command_output` | None |

Existing Find, Browse, and line-scroll bindings are unchanged. In particular,
Ctrl+Up/Down continue to scroll lines. Return and Space activate a focused
navigation button through GTK instead of being redirected into the shell.

The row hides while an alternate-screen/full-screen program owns the terminal;
its layout space returns to the VTE and the PTY grid follows the allocation.
The row returns when ordinary terminal ownership resumes. Its command
navigation, current-command copy, and Return to live actions refuse while the
full-screen program owns the pane. The existing Browse dialog can still offer
a retained read-only snapshot; unavailable row authority prevents it from
moving the full-screen terminal viewport.

## Compare repeated commands

Choose a completed command with Previous, Next, or Browse. The compare icon
and **Compare command output** palette action find the nearest earlier
retained record with the same raw recorded command text and the same known
folder. Unknown folders cannot match one another, shortened commands cannot
choose a previous run, and an evicted previous snapshot is not silently
replaced by a different run. If either output snapshot is unavailable, the
action explains why comparison is unavailable.

The read-only dialog shows both stable record IDs, local start times when
authoritative, command text, folder, exit status, and command provenance.
Comparison runs on a bounded background worker. Removed lines use `-`, added
lines use `+`, and retained unchanged lines use a space. Capture truncation,
input bounds, coarse alignment, and report limits remain explicit. The
comparison covers retained text; it does not compare graphics or terminal
layout. **Copy displayed comparison** includes its metadata and limit notes.
Opening, closing, and copying this view do not insert or run a command.

The same palette action compares exactly two explicitly selected finished
cards in Block/Chat. Their Review dialog also provides **Compare selected
output**; the pair stays in terminal order even when its numeric IDs are not
sorted. Missing selected IDs refuse the complete operation.

## Focused validation

`block_view::unified_navigation::tests` covers range-only geometry changes,
invalid geometry, viewport-direction selection, clamped-position ties,
identity retirement, boundary behavior, and complete command copy admission.
The display regression
`block_view::tests::unified_navigation_preserves_native_ownership_and_follows_live`
uses the real Unified constructor, GTK widgets, VTE scrollback, and a
nonexecuting PTY recorder. It covers native selection, external focus, Find
completion invalidation, foreground unread counting, wide/narrow allocations,
the kernel PTY size, alternate-screen transitions, and closed-pane ownership.
It is included in `scripts/test-gtk-display.sh`. Set
`ANVIL_UNIFIED_SCREENSHOT_DIR` when running that test to export its wide and
narrow native window snapshots.
