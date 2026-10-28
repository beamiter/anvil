# Native Inspector reference sections

The Anvil overview now follows Forge's verified native Inspector sections
(9680a2d, with d11e786's single-line title/badge fix). The typed Inspector model,
Agent/Context/live routing and unavailable Files state are retained. The central
conversation renderer's prefix is byte-identical to the prior source.

At1672×941 the idle overview has these actual native bounds:

| Region | x | y | width | height |
| --- | ---: | ---: | ---: | ---: |
| Metadata | 1396 | 180 | 253 | 132 |
| Current step | 1396 | 359 | 253 | 59 |
| Empty queue | 1396 | 466 | 253 | 58 |
| Terminal section | 1396 | 669 | 253 | 250 |
| Terminal heading | 1396 | 688 | 253 | 12 |
| Terminal output | 1396 | 708 | 253 | 199 |

No progress is fabricated for an idle model. An observed or explicitly labeled
fixture model may show progress and real typed queue rows. The five-row fixture
naturally shifts Metadata/Current/Queue to212/391/498 while its output remains
at708 in the reference-sized window. All task badges stay on one line; titles
ellipsis before displacing the badge and preserve full tooltips.

The existing overview-only GtkScrolledWindow handles short windows. Only the
inter-section spacer expands; the terminal section explicitly does not inherit
expansion from its centered empty-state content. At1356×704 both idle and
fixture views scroll to the complete199px output pane; the fixture's fifth queue
row remains reachable. Under stable native states, the original central content
pixels are unchanged by Inspector wheel input. This is a visual position proof,
not a claim to expose the private reading-anchor state.

Release check/build passed. Both modes passed the native reference-bound,
four-width footer and short-window checks; Chat's ordinary Bash input and native
Copy spot check also passed. Terminal heading icon and text ink are verified in
actual native screenshots. No protocol/input/Ready/PTY or central Chat component
changes are included. Main/UI font defaults and their ownership remain intact.

The original diagnostic failures are retained: a full-window dark-border scan
mistook the26px profile avatar for a footer button, and a live Block pixel
comparison captured a legitimate sticky timer change from6s to7s. The corrected
footer observation uses its actual native rectangle. A separate static Block
control really interrupted the task, verified exit130, and sampled matching
natural visible-cursor phases; it kept the exact zero-difference assertion.

Running prompt/command echo, live gutter and the known large active-area/follow
cases remain outside this Inspector batch. Full typography/contrast and arbitrary
long-title pixel acceptance remain separate; this is not a1:1 completion claim.

## Heading typography follow-up

The scoped Inspector section labels now match Forge d11e786's existing
8px/700/1px tracking roles. Only `font-weight` and `letter-spacing` were added;
font size, color, margins and minimum height are unchanged. The rule applies to
CURRENT STEP, TASK QUEUE and Terminal output, without affecting central Chat
or terminal fonts.

The native before/after comparison uses the frozen90a205 application and the
6a50dfe application plus this CSS change, with the same explicitly labeled
fixture, idle real Bash prompt, independent12px/14pt defaults and1672×941/DPR1.
Both public modes have identical native region bounds before and after. Each
comparison changes only the three heading glyphs (530/420/479 pixels); every
pixel outside their actual text rows is identical. Inspector output remains
at708/199, and the composer remains68px with its28px control row at897.

The first failed observation is retained: three antialiased glyph pixels extend
one pixel left of the logical text box, and the initial Chat geometry log
preceded the asynchronous composer map. The final observer accounts for that
measured glyph bearing, waits for actual footer paint, and gives all four runs
the same1356→1672 preparation. The zero-difference assertion is unchanged.
This is a bounded typography proof, not an input/IME or resize-authority test.

## Current Step content alignment

Only the detailed Current Step row now centers its symbol and text column as
one group. Title and description each use one ellipsized line and retain full
tooltips. The normal two-line example therefore has natural height25 with17px
above/below; the long Unicode fixture measures28 with15/16px above/below.
The card remains253×59. No CSS padding, queue-row behavior, central renderer,
font, input or native feed code changes are included.

The isolated native test uses the real Inspector renderer and production CSS,
normal/long Unicode content, and844/607px Inspector heights. It verifies actual
border bounds, centered non-overlapping children, at least10px vertical space,
single-line title/description, full tooltips, real Pango ellipsis and access to
the199px output pane by scrolling. The first test fixture incorrectly gave a
297px outer window a1px Inspector border, producing296px content; the corrected
fixture preserves the real298px outer/297px content relationship and253px card
assertion. The initial two-line description also failed the long-content10px
space check. Both failures remain recorded; the final product uses a bounded
title-plus-description two-line group without shrinking its font or card.

The application comparison uses the frozena990 version and the new version in
both public modes at1672×941 and1356×704. All native region bounds,59px card,
68px composer and28px footer remain unchanged. Chat at both sizes and Block at
1356 change only the Current Step interior. The first Block1672 comparison
retains a strict failure:720 pixels in the live terminal's right soft-glow edge
differed. A bounded four-start natural-state control produced matching old/old,
new/new and old/new pixels after at least2s; comparison of the original new
image with the same new binary reproduced exactly that720px strip alone.
Its initial paint state varies across runs; the mechanism and first-frame
timing remain unknown. This does not mark the original failure as passed or
certify frame continuity. Long-CJK GTK minimum/natural-height warnings are
also retained despite passing measured bounds and Pango content checks.
