# Shared shell geometry and task state

This batch supersedes the earlier 55px/x244/68px Anvil/Forge workspace variant.
Both Block and Chat now consume the common four-native outer contract: titlebar
42, rail 55, navigation 237, Inspector 306, canvas header 49, and composer 80
with 28px horizontal and 11px bottom insets. At 1356×704 the composer target is
`(320, 613, 702, 80)`. The dark Block canvas, 36px Live toolbar, retained command
cards, 12px default VTE font, and existing input owner are preserved.

The footer uses shared base widths: Plus 28, Attach 62, Image 60, Code 56,
Terminal 76, Tools 68, Model 120, and Send/Stop 28, with packed 6px gaps.
Compact decisions use the actual composer's border-box width: secondary tools
yield below 564, Model below 392, and Terminal below 304. Existing Tools actions
remain available. User UI font enlargement scales control widths, heights, and
icons through the existing role ratio; framework DPR is not multiplied again.
Smaller UI fonts retain the minimum hit area. Explicit Main/UI font settings
and their px/pt units are not changed or saved by this presentation code.

The Inspector returns to the bordered original card and compact metadata flow:
metadata row pitch 19, Current Step 45, queue rows 24, output 140 with an 8px
bottom edge. Current Step text stays centered, bounded to title plus description,
and retains complete tooltips. The native geometry test targets are updated
from the superseded 59px card to 45px, retaining exact size, centering, nonoverlap,
ellipsis and tooltip checks. The minimum vertical text padding is 8px in the
new 45px box instead of 10px in the old 59px box.

Task presence determines active versus neutral badge classes. Active uses
`#283d6f` / `#dde5f4`; No task and unavailable states use `#162133` / `#90a0b9`.
An idle progress slot shows an empty track and “No progress”, without a fabricated
count or activity. Fixture data remains visibly identified.

Fresh configurations now use opaque window compositing (`opacity = 1.0`) so
the shared semantic colors are not darkened by a default 95% window alpha.
Explicit file/environment opacity preferences retain their existing precedence
and clamping. No saved user configuration is rewritten.

The first canonical native capture observed correct outer geometry but exposed
real minimum-width overflows: Attach 63 instead of 62, Image 62 instead of 60,
and Tools 71 instead of 68. Footer-only horizontal padding is reduced to allow
the existing exact width requests to fit the unchanged 10px labels and 12px
icons. Those original failed results remain available. The next native capture
must verify actual allocations; the CSS source change is not acceptance.

## Diagnostic timing

The retained `chat-input-v2` failure logged a 293px composer before a raw PNG
showed its complete 68px frame. The previous opt-in diagnostic was triggered by
the outer overlay's resize and sampled once in an idle callback. Child layout
could subsequently settle without another outer resize, leaving the stale
diagnostic as the last value. This does not establish that the visible surface
was 293px: the raw image and the independent successful input/copy checks are
separate evidence. The original failed result is retained.

The new opt-in sampler observes actual after-paint bounds, requires two equal
in-window samples, and stops after at most eight observed frames or a newer
resize epoch. It explicitly requests frame-clock updating for this bounded
diagnostic; it is not ordinary startup continuity proof or a native parser,
prompt-readiness, input-admission, or PTY acknowledgement. It never changes
layout, feeds bytes, or authorizes input. Validation is recorded independently
of the earlier failure; the source change alone is not a passing native result.
The diagnostic also reports the computed badge foreground and whole-window
opacity. At 8px, glyph antialiasing can contain no fully covered foreground
pixel; computed color plus visible contrast is recorded separately from the
original exact-RGB pixel-count failure.
