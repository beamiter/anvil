# Organism surface contract: time boundary (A09, first slice)

The private `organism_ui::surface_time` module is a display-only contract shared
by the Block/Chat runtime and the neutral VTE companion. It is not a new shared
core release or a capability expansion. `Cargo.toml` and `Cargo.lock` stay pinned.

## Implemented contract

- Accepted input retreats for the half-open interval `[0, 900ms)`. Missing input
  is not a retreat. At 900ms and later, the retreat has expired.
- Each adapter converts its latest monotonic input timestamp to an age using
  `saturating_duration_since`. An observation before that timestamp conservatively
  has age zero. Repeated input replaces the last timestamp; it does not create
  another independent timeout.
- Full motion preserves the selected frame. Calm and Static paint frame zero.
  Block's command and cross-pane cue epochs remain distinct; VTE receives only
  its existing global neutral epoch and never synthesizes command reactions.
- The module contains no scheduler, GTK reference, terminal writer, focus change,
  or semantic state. It cannot authorize a wake, move focus, or write to a PTY.

Block and Chat share the `BlockTerminal` creation/attachment branch in
`src/main.rs` and therefore the same runtime time adapter. This is source-level
routing evidence, not a claim that native Chat geometry has been validated here.

## Intentional host differences

Block retains its semantic maintenance heartbeat when backgrounded or Static;
its Static representation is the inline card. VTE retains its existing
Static/background no-animation-source policy and neutral card. The existing
Block boundary catch-up remains one millisecond when a paint crosses the input
expiry. None of these differences is flattened into a new universal timer.

## Verification and remaining boundary

`surface_time_adapter_tests` applies one timestamp trace to the production
Block mode selector and VTE retreat adapter, and compares the common idle frame
selection across Full, Calm and Static. It also tests repeated input, clock
ordering, exact expiry, Block reaction/cue epochs and scheduling-policy guards.
The pure module covers nanosecond boundaries and extreme ages/frames.

These tests run with ordinary Cargo tests on hosted CI. A bounded headless
extraction can exercise those exact adapter functions without GTK; that is not
a full-crate build or native acceptance. The existing display fixture
`live_settings_reuse_body_preserve_focus_and_stop_disabled_timers` continues to
cover the native hosts, including VTE Static/background source retirement,
neutral-only behavior, nonexecuting PTY checks and accessibility assertions.

A09 as a whole remains open: a single native event trace still needs to compare
geometry, occlusion, focus retreat, local/background ownership and hidden
accessibility nodes across Block, Chat and VTE before further surface-state
abstraction. This slice does not change any of their widget, geometry, focus,
capability, accessibility or PTY paths.

## Shared native adapter trace (second slice)

The existing display fixture now runs the same named sequence against its real
Block/Chat runtime and its real VTE companion: Full, Calm, input retreat, expired
input, background, focus return, Static, Full again, disabled, reenabled.
`surface_adapter_trace` owns the sequence and expected observations; each host
applies it through its existing settings, focus and refresh paths. Accepted
input is supplied at the timestamp evidence seam, without emitting a terminal
commit or inventing a command outcome.

Each step checks primary paint, card visibility and the actual pending wake.
Expected differences are explicit: Static leaves Block's inline card and its
semantic maintenance wake, while VTE keeps a neutral fixed glyph with no wake.
Background retains Block's maintenance wake but retires VTE's source. Disabling
from Full must destroy the captured source and remain retired after draining
longer than the old frame interval. PTY recorder bytes must be empty both before
and after every step's drain, as well as at entry and exit. VTE's exact neutral
glyph is checked at every observation.

This adds adapter coverage, not a new product state abstraction. It changes no
production geometry, timer, capability or accessibility path. The Block/Chat
routing is still shared rather than a separately mounted native Chat run. A
single shared geometry/occlusion/local-versus-remote/accessibility trace remains
out of scope; the existing dedicated native tests continue to cover those
boundaries. Pure trace-table/negative-driver tests do not count as running the
native adapters; native acceptance requires the exact commit's display gate.

The trace's background step means pane ownership: Block loses its presence
owner, while VTE moves actual GTK focus to the existing sibling entry. It is
not an OS-window deactivation test. Input expiry is supplied through the
monotonic timestamp evidence seam rather than waiting 900ms in each adapter;
the earlier dedicated native expiry fixture still tests the real deadline.


## Independently mounted modes and capability gate (new third-slice candidate)

The existing display fixture additionally constructs separate Block and Chat
panes through `TermView::new_with_spawner`, selecting each actual layout. Only
the child-process spawner is replaced by a nonexecuting raw PTY recorder. It
mounts each pane in the same native window and attaches its runtime through the
production `OrganismHub::attach_ascii_organism_to_view` method. An in-memory hub
avoids persistence; it does not replace attachment or capability decisions.

Both local layouts run the original ten-step trace, 120ms drains, and zero-PTY
assertions before and after every drain. The ordinary Block control still must
paint in every eligible step. Default Chat additionally reports actual terminal
and sprite measurements: if its native compact prompt cannot fit the full idle
body plus grid-aligned margins/gutter, the expected body is hidden while card and
wake assertions remain intact. A separate Chat constructor uses the actual
`monospace 32` terminal font configuration and must both measure larger cells and
paint in every eligible step. No widget allocation or body size is overridden.
The fit oracle uses measured dimensions and simple aligned legal intervals,
never the production placement function or the observed paint result. Their log
identities are now separately `Block` and `Chat`; VTE still runs its existing
neutral adapter and exact-neutral-glyph oracle. No VTE semantic reaction or
universal scheduler is introduced.

Each constructor also runs through the actual remote attachment gate and global
settings refresh for all ten settings/ownership steps. These cases assert no
eligible pane/runtime, rejected interaction, and zero recorder bytes. Remote
is only the production capability argument: no network connection is made.

After the local trace, a fresh Full source is started and the pane is unmounted
and destroyed, including release of the fixture's own root-widget reference.
The recorder outlives the pane, proving zero bytes after a 120ms
drain, while weak pane/runtime references and the captured source prove that the
old epoch cannot resurrect. This is destruction coverage; merely hiding a live
Block/Chat pane intentionally retains its maintenance heartbeat. Remote panes
are independently released and checked after the same drain.

This is a newly rebuilt candidate, not recovery of the former unpublished patch.
Static/source checks do not establish native success. Exact-head hosted display
CI must execute these cases before acceptance; full A09 geometry/occlusion and
cross-mode accessibility acceptance remains open.
