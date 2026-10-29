# Block status and surface refinement

This local UI checkpoint follows the inspected `Forge Developer Terminal
Workspace.png` reference. It adds shared native vector symbols and bold labels
to actual Completed, Failed, and Running badges. Unknown/interrupted/background
states retain neutral labels. Failed command icon tiles use a muted coral tint;
card headers have a subtle dark gradient. The existing More button displays the
shared horizontal ellipsis while keeping its original action and hit area.

Only Block presentation is styled. The minimum 40px command header, 59px Current
Step card, 68px composer, shared shell geometry, user fonts, native terminal,
PTY/input authority, selection, and scrolling implementation are unchanged.
Long commands retain the existing native wrapping behavior. No protocol guard,
ACK, hold, crop, or running gutter is included.

## Verification

- Format checking, release checking, and the release build passed.
- The same native binary rendered Block and Chat at 1672×941, 1356×704,
  1000×704, and 700×704, with the existing footer and Inspector geometry checks.
- Actual Bash commands produced success, exit 7, and a foreground running state.
  A user-scrolled history view shows the real completed/failed cards; the first
  follow view correctly showed only the running terminal and is retained.
- The actual More menu's Copy Output action copied the exact plain output,
  including its internal newlines, without ANSI sequences or gutter numbers.
  A long command remained exact in the application record, and a native wheel
  check changed the history view without adding command records.
- Chat's actual input record, Bash output bytes, and whole-command native copy
  spotcheck passed. This is not a physical IME or complete input-matrix claim.

The first interaction observer failed to recognize small text and antialiased
button corners. Its result is retained separately from the corrected bounded
menu-target test. A later Chat spotcheck also retained a strict geometry failure:
its initial diagnostic reported a 293px composer although the raw PNG showed
the complete 68px composer. No replacement pass or product fix is claimed for
that diagnostic discrepancy. The prior first-paint differences remain open.

Raw screenshots retain the full native window. Native frame properties,
window sizes, GTK logical sizes, and DPR were recorded without guessing a
client crop. Running command echo, large full-VTE height, and running gutter
remain functional boundaries. This checkpoint is not 1:1 design acceptance.
