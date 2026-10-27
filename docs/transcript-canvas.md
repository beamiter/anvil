# Continuous Block and Chat canvas

Anvil's Block and Chat presentations retain one history scroller per pane.
Finished command widgets keep their identity, off-screen cards keep measured
placeholder heights, and output changes advance the existing canvas position.
The shared `jterm_core::transcript_canvas::CanvasMotion` supplies the same
frame-time-based movement used by the other terminal frontends.

## Interaction contract

- Discrete wheel steps accumulate a 54 logical-pixel destination, independent
  of window height. A reversal cancels the outstanding direction immediately.
- Precision touchpad deltas move the outer document by their original logical
  pixels, including fractions. GTK's native gesture-ending `decelerate` signal
  supplies a bounded exponential coast. The coast runs only for a gesture whose
  final event was actually consumed by the outer document.
- PTY/contents updates only retarget the live edge. One GTK frame-clock callback
  follows the changing layout; there are no periodic 16 ms bottom-snap timers
  and no synchronous outer-scroll jump on every output chunk.
- A user wheel, scrollbar drag, native find, or selection navigation owns its
  new position. Incoming output cannot reclaim a reader's viewport. Jump to
  latest explicitly resumes animated follow.
- Mapping an unseen or returning live-following tab establishes its current
  live-edge baseline without flying through hidden history. A returning reader
  keeps the reading position. Unmapping stops the clock and discards inertia.
- Retiring a bounded-history prefix measures the removed GTK geometry and
  rebases both the presented position and its destination in the same layout
  turn. Surviving content stays at the same screen location.
- `gtk-enable-animations=false` settles programmatic/wheel motion immediately;
  precision input remains direct. A settled or hidden canvas schedules no ticks.

The ordinary fit-to-content output body has no private vertical scroll range,
so text, command headers and gaps move the same outer canvas. Very large
outputs keep the existing bounded VTE viewport and explicit scrollbar/expand
controls. Only that overflow scrolls internally; precision movement is converted
using the actual terminal character height, never a fraction of window height.
Alternate-screen applications and Unified mode retain their existing native
terminal input/scroll ownership.

This is a continuous presentation over retained, bounded command history. It
does not remove history/memory budgets or synthesize text outside the real PTY.

## Validation

`block_view::scroll::canvas_native_tests::retained_canvas_scrolls_through_frames_and_yields_to_reading`
is registered in `scripts/test-gtk-display.sh`. Its real mapped GTK window
checks multiple intermediate presentation frames, monotonic live growth,
retained widget identity, exact fractional input, reading during subsequent
output, scrollbar interruption, measured prefix rebasing, hidden-tab callback
retirement and desktop reduced motion. Run it in its own Xvfb/D-Bus process,
like the other native regressions. The shared core independently tests motion at
multiple refresh rates, retargeting, reversal, bounds, and invalid timing/input.

These automated checks cover X11/Cairo here. Hardware touchpad feel, Wayland,
fractional scaling and physical high-refresh display pacing require a native
desktop acceptance pass; the mathematical frame-rate tests are not a substitute
for that observation.

## Continuous surface and trim stability

The native transcript now removes raised-card shadows and rounded outer panels.
Neutral finished output shares the canvas background; command capsules, outcome
stripes/washes, bookmark markings and keyboard selection remain distinguishable.
Chat output also uses the flat canvas, while the docked composer gains an inset
focus ring. These are decoration-only changes: existing padding, margins, border
widths, terminal columns and alternate-screen allocation remain unchanged.

Prefix retirement snapshots motion before GTK can clamp its adjustment to the
shorter document, then rebases the snapshot and bounds atomically. The native
canvas regression explicitly publishes the shorter adjustment range before the
rebase while reading near the old bottom. This covers the layout ordering that
previously could subtract the retired height twice and jump away from the reader.

CSS contract tests check decoration/geometry separation and retained selection
cues. They do not establish visual acceptance: theme, scale and actual rendered
pixels still require a native display run.

## Fractional reading ownership

Reading ownership is independent of the half-pixel animation/layout settling
threshold. A precision movement of 0.375 logical pixels away from the live edge
now holds the reader's position during subsequent output, including the deferred
geometry probe. Returning exactly to the live edge resumes follow; geometry drift
alone still does not create reading intent. An external fractional adjustment also
interrupts an active wheel animation rather than being overwritten on its next
frame. Existing motion settling and stable-layout thresholds are unchanged.

The registered retained-canvas native regression now tests output arrival after
that fractional gesture without a second, larger wheel movement, and uses a
fractional scrollbar interruption. Pure intent regressions cover the policy but
do not substitute for running that mapped GTK test.

### Wheel arrival and reading ownership

A discrete wheel destination at the bottom does not release reading ownership
until the displayed position arrives there. Output arriving during that motion
cannot replace the user’s destination with a newer live edge. Precision input
and reduced-motion input still arrive directly and release ownership at the edge.
