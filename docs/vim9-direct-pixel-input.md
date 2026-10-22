# Direct precision scrolling and source-bound pointer input

## Implemented GTK path

The native Vim buffer surface uses shared core's `Vim9PixelScroll` for GDK
surface-unit precision input in supported idle Normal-mode views. Sub-line input immediately updates
logical-pixel presentation; it is not held until Vim acknowledges a whole row.
The residual fraction persists across acknowledgements. Each split has at most
one source/view-checked scroll command in flight. Platform momentum is consumed
once rather than receiving a second synthetic easing curve.

During this gesture, the GTK snapshot paints real semantic overscan rows through
`Vim9Viewport`, including partly exposed text at split edges. Pango shaping,
font fallback, styles and decorations retain the existing native text path.
The split clipping rectangle stays fixed. Idle text retains its cached scene;
discrete wheel input retains the bounded acknowledged-row animation.

A snapshot records its immutable source frame, geometry and applied offsets.
A primary click during a nonzero precision offset resolves against that
submitted image, not a newer frame merely received by the polling loop. The
current source generation and geometry are checked again before dispatch.
Plain primary dragging sends coalesced source-byte targets into real Vim
characterwise Visual selection. Release outside the split finishes at the last
admitted target. It does not switch the drag to a peer split. Keyboard input,
preedit, zoom, focus loss and incompatible geometry/source changes cancel old
unsent gestures. Caret paint and the native IM candidate rectangle share the
same projection offset.

Stationary mouse events are not taken over. Custom mouse mappings, Select-mode,
virtual columns, modifier clicks and multi-click handling remain on the Vim
compatibility path. Complete multi-click equivalence during a fractional
scroll gesture and drag-edge auto-scrolling remain future acceptance items.
Precision mode obeys the existing smooth-scrolling and system animation gates.
Unsupported native states keep the compatibility renderer and input path.
No optional IDE chrome is enabled by this change.

## Fractional input and delayed acknowledgements

Precision events retain their logical-pixel distance, including one-pixel and
quarter-pixel deltas. Neither input accumulation nor the submitted GTK snapshot
rounds that movement to a source row. Whole-line commands only synchronize
Vim's source viewport; they do not determine the native presentation step.

If a fast gesture exhausts the available semantic margin rows, the shared
controller keeps the requested distance separate from the distance already
shown. It commits only sampled rows to Vim. An exact acknowledgement preserves
the visible source position; newly available rows let the remaining distance
catch up through the GUI frame clock. A reversal responds from the visible
position instead of requiring repayment of motion that was never displayed.
Unsupported margins retain their compatibility path.

## Pointer lifetime and submitted caret follow-up

The GTK gesture now retires when its event sequence is cancelled or the native
surface is unmapped. An unpaired primary release still completes the admitted
source gesture. If a compositor loses that release entirely, a subsequent
motion without the primary button finishes at the last accepted source target;
hover movement cannot extend Visual selection or indefinitely capture wheel
input. Disabling precision input also retires the native pointer transaction.

Switching from precision scrolling to a discrete wheel lazily rebuilds the
current outgoing text scene when the first wheel acknowledgement needs it.
That transition retains wheel easing instead of snapping because its old
integer scene cache was no longer current.

Pointer ownership now follows the last submitted GTK snapshot as well as its
source coordinates. Retiring a live pixel controller does not prematurely
reinterpret clicks against an integer grid while the fractional image remains
on screen. Once GTK paints the stationary replacement, normal Vim mouse
ownership resumes. Buffer generation and geometry checks remain in place.

The input-method candidate rectangle uses the caret captured in that submitted
snapshot. Receiving a newer cursor frame no longer moves the candidate anchor
ahead of the visible caret. The next snapshot records the replacement caret;
font geometry and active-buffer switches reject stale candidate coordinates.

Paint frames and pending request IDs are sampled atomically through
`Vim9Session::input_snapshot()`. A worker acknowledgement arriving between
separate reads can no longer retire a fractional gesture using an older frame.
If a still-visible fractional image no longer has a valid source generation,
the press is consumed while that image settles. Modifier clicks, multi-clicks
and other mouse buttons also settle a fractional image before normal Vim mouse
handling resumes. Failed native pointer submission follows the same rule.
Wheel admission checks the submitted buffer generation and geometry as well;
a pending buffer switch cannot scroll a different source. A valid acknowledged
topline change still accepts continued scrolling. Unsupported horizontal or
modified scrolling settles a fractional image. The submitted image is retired
by the replacement GTK snapshot, not merely by receipt of a new frame.

## Follow-up validation — 2026-10-21

This follow-up was validated from master base
`0b67690bcbccb8bdbf78ffc4745fdb9b680cb0d1`, using the real locked core revision
`9dc44d8bb521b4d8ad9435e2bd6e09aecdf090b0`. The tested buffer source blob is
`df916d4c7420bf7f44983f5d4d2cec12a323de5c`; no local path override was used.

| Check on the follow-up source | Result |
| --- | --- |
| `cargo fmt --all -- --check` and `git diff --check` | Passed |
| `cargo check --locked --offline --all-targets` | Passed |
| `cargo clippy --all-targets --all-features --locked --offline -- -D warnings` | Passed |
| `cargo test --locked --offline vim9 -- --test-threads=1` | 7 passed, 5 ignored, 0 failed |
| `native_pixel_snapshot_and_source_hit_round_trip`, explicitly run with `--ignored --test-threads=1` | 1 passed, 0 ignored, 0 failed |
| `native_vim9_fractional_scroll_and_split_isolation`, explicitly run with `--ignored --test-threads=1` | 1 passed, 0 ignored, 0 failed |

The two display tests ran under DBus/Xvfb with the X11 backend and Cairo
renderer. The temporary build environment used Rust 1.99.1, GTK 4.14.5,
Pango 1.52.1, libadwaita 1.5.0 and VTE 0.76.0. The scroll regression launched
real Vim 9.2 with patches 1–1217. Its final screenshot run used a temporary
Noto Sans CJK SC font configuration, with Chinese glyphs visible in both splits.

The enhanced snapshot regression checks submitted caret/source consistency,
lost-release hover behavior, cancellation before repaint, stale-source and
unsupported fractional clicks, scroll-source rejection and stationary mouse
compatibility. The real-Vim regression verifies sub-line accumulation,
inactive-split isolation, the precision-to-wheel transition, 32 successive
wheel acknowledgements and keyboard cancellation. It first exposed the stale
outgoing-scene defect described above; the corrected final source passes.
The strict lint run also exposed an unnecessary `unwrap` in wheel history
retention; the final source uses checked option handling without a lint allow.

## Original precision-input release validation — 2026-10-21

Anvil Actions run `39542232927`, job `119032352414`, passed against master base
`10c98953199998e7962789930adc86debaa574c4` on Ubuntu 24.04. The clean source
candidate was `0ceedae6b9e6b811d4a7c3a82313049040ea7461`.

The original precision-input release used these tested source blobs:

| File | Blob SHA |
| --- | --- |
| `Cargo.toml` | `513f3e75a02ce8caf9aedd9b263a58d4b4405033` |
| `Cargo.lock` | `42b435f9d12be96add2729adc7f8c780a8dd0788` |
| `src/vim9_ide_buffer.rs` | `5cb70c32d140e22a7130f0343b60b1e817e4869f` |

- Rust formatting and whitespace checks passed.
- `cargo check --locked --all-targets` passed.
- The `vim9` test filter passed 7 tests, with 5 ignored.
- The normally ignored `native_pixel_snapshot_and_source_hit_round_trip` test
  was separately executed under DBus/Xvfb: 1 passed, zero ignored or failed.
- That test constructs actual GTK/Pango render nodes and checks immediate
  fractional presentation, matching caret coordinates, margin and empty-line
  hits, split bounds, source generation rejection, last-submitted-frame hit
  testing and preservation of stationary Vim mouse ownership.

The regular `Native Vim pixel viewport` workflow now executes that display
check explicitly. Temporary staging scripts/workflows are not on master.

## Scope of the evidence

The snapshot regression uses controlled frames and actual GTK/Pango render
nodes. The real-Vim regression starts the frontend and Vim session, then drives
the frontend scroll-admission method. These do not synthesize physical device
events or measure OS-event-to-GPU-to-physical-display latency. Xvfb reported
lack of DRI3 acceleration, so this is software-renderer evidence.
Physical touchpad/high-refresh latency, Wayland presentation, Chinese IME and
a controlled VS Code comparison have not been measured. Nix packaging has not
been rerun for this follow-up. Validation covers the exact checks listed above;
it does not claim the complete application test suite. No installation or
restart of the user's applications was performed.
