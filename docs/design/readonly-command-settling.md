# Readonly command snapshot settling

The 12px Main default exposed a native command-card regression. The unchanged
strict resize test passed with the old 14pt default, then failed at its first
900px allocation with the new default: a 206-character command retained only
its final 56 visible characters. Native diagnostics showed the final surface
already had the correct 75-column, three-row grid, so allocating more final
height would not recover the prefix.

The command terminal had two asynchronous settling owners. Its generic
constructor scheduled a one-logical-row pass, while its command map handler
subsequently fed the snapshot using its actual wrapped three-row budget. The
stale one-row pass could shrink the parsed snapshot while scrollback was zero;
the later three-row pass could not restore discarded prefix cells.

Only command-terminal construction now skips that generic settling hook. The
existing command map handler remains responsible for its actual columns,
wrapped rows, immutable command bytes and bounded settling. Output terminals,
ActiveBlock, the PTY, capture guards and submission authority are unchanged.
This does not repair the separately open history-disabled command-anchor
fallback issue.

The original strict test file is byte-identical to the old passing harness.
Its 900→420→760 geometry, full-command and font-change assertions pass after
the fix. The same compiled candidate also passes the native Unicode/cursor,
point/pixel and Main/UI scope checks, compact Chat Enter transitions, More
selection/lifecycle, More document-height round trip and visible/parked fold
checks. Together these are 12 native tests; the headless font subset has 32
passes. Existing GTK allocation warnings remain recorded separately, so these
results do not claim a warning-free renderer or complete visual acceptance.
