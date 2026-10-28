# Native parser nonce experiment

The isolated, ignored `native_ack_tests` experiment verified a native parser
receipt on the existing VTE 0.82.3 SDK. It registers one UINT extension property
with NONE flags before any Terminal exists in that exact-test process. The
0.78-only install/accessor functions are resolved with optional `dlsym` lookups;
Cargo retains `v0_76`, and the test ELF has no direct undefined reference to
either optional function. No production startup, feed, crop, gutter or PTY
geometry uses this experiment.

The actual detailed termprop signal calls only the UINT getter on its terminal
and copies the numeric nonce into test-owned memory. Cursor, text, title and
widget APIs run outside the signal. Following native feed is held until an
outside idle reads the matching prefix cursor. The successful case matched an
unmarked native prefix oracle, then flushed the original suffix once in order.
The suffix deliberately splits SGR and UTF-8 sequences across feed chunks.

All six native cases passed: matching nonce, wrong nonce, timeout, resize,
input and unavailable capability. Original native text and title were retained
in every case. Marker/feed processing wrote no PTY bytes; the explicit input
positive control wrote exactly `z`, proving that the PTY observer was live.
The test-owned OpenPTY begins at 0×0 and is never resized: this is not evidence
for production shell sizing. Late matching notifications did not revive a
cancelled fence.

Limits are deliberate. The 4096-byte assertion bounds this fixture, not a
production streaming overflow implementation. Timeout omits the private marker
as a fault injection; it is not an observed SDK notification loss. Resize is
cancelled at its request and subsequently applied by GTK. Input uses an
explicit native commit signal, not physical keyboard or IME evidence. The
actual 0.76 runtime was not run. Cursor values are native API coordinates, not
paint completion or calibrated ring/adjustment/pixel origins. Partial-prefix
parser states, production reset/input/resize receipts, and output-only display
projection remain unimplemented.
