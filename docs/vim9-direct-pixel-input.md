# Direct precision scrolling and source-bound pointer input

## Implemented GTK path

The native Vim buffer surface now uses `Vim9PixelScroll` from shared core
`338011a4d0bb889df6e4116ac5221b50efca0620` for GDK surface-unit precision
input in supported idle Normal-mode views. Sub-line input immediately updates
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

## Observed validation — 2026-10-21

Anvil Actions run `39542232927`, job `119032352414`, passed against master base
`10c98953199998e7962789930adc86debaa574c4` on Ubuntu 24.04. The clean source
candidate was `0ceedae6b9e6b811d4a7c3a82313049040ea7461`.

Published source blobs are unchanged from that tested candidate:

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

The source-bound Vim protocol has separate real-Vim PTY regression coverage in
jterm_core. The GTK test above uses controlled frames and actual render-node
construction; it is not an OS-event-to-GPU-to-physical-display end-to-end test.
Xvfb reported lack of DRI3 acceleration, so this is not hardware-GPU evidence.
Physical touchpad/high-refresh latency, Wayland presentation, Chinese IME and
a controlled VS Code comparison have not been measured. Full application CI,
Nix packaging and pre-existing frontend lint failures remain separate gates.
No installation or restart of the user's applications was performed.
