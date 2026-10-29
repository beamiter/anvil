# Right inspector toggle

The application toolbar keeps a 28px right-sidebar toggle visible in both Block
and Chat. Its checked state follows the actual panel, with “Hide right sidebar”
or “Show right sidebar” as its accessible label and tooltip.

A wide window uses the existing inspector slot. Hiding it uses the shared
`Layout::without_inspector()` contract, giving the center, header and composer
the complete released width. The existing Agent/Context/live stack and its
scroll state remain the same widgets. Files keeps its existing unavailable
state; this change does not invent a file-inspection backend.

Ordinary narrow windows initially retain automatic collapse. An explicit Show
opens the same inspector as a non-measuring 306px drawer below the toolbar.
When the window becomes wide, that widget docks again. Explicit Hide/Show is
application-wide through tab changes and splits, and does not depend on the
AI/provider or task-panel visibility preferences. A panel that already owns
focus also moves to the drawer when resized. Only a window too small for the
panel plus minimal remaining canvas has a disabled entry with a reason.

The center input is not replaced, reparented, committed or cancelled. Pointer
activation does not take its focus. Moving the inspector reuses the existing
same-window focus-return guard, which yields to newer owners. Closing an
inspector that owns focus returns to the current input. No new periodic tick,
PTY write, window reset, or history reload is introduced.

The opt-in existing shell diagnostic records actual panel/toggle geometry,
selected page, retained object identity and focus identity for native acceptance.
Ordinary operation does not emit those records. Test input is kept private.

This source checkpoint requires native wide/narrow, tab/split, running-output,
keyboard and reading-anchor verification before acceptance. Source and format
checks alone do not establish the behavior.

AI/Tasks explicit open actions reveal the retained live page, even after an
outer Hide or while Context was selected. Passive snapshot/task refresh does
not change the user's outer visibility. Provider/task preferences remain
separate and their existing priority is retained. Closing a function page
returns to the ordinary overview instead of destroying the outer inspector.

All legacy AI width restore requests now use the shell's same layout callback;
there is no second immediate/deferred Paned-position writer. Width observation
only accepts the visible inspector actually docked in that Paned, never a
hidden or drawer panel. Startup also uses this single responsive owner.
