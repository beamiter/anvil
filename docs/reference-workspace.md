# Shared native workspace reconstruction

This is a recoverable implementation checkpoint; 1:1 visual acceptance remains
open. Frozen native tests of the safety change passed canvas admission/reading
anchors, unique ownership, 320 px Enter geometry and 420 px resize/full-text
checks. The lifecycle test still fails the accessibility-description assertion
also reproduced on the archived baseline; this is not an all-tests-passed claim.

The window shell now binds the family contract in
`jterm_core::reference_shell`: a 42 px titlebar, 55 px icon rail, 237 px
navigation, 49 px canvas header and 306 px inspector. The center receives
surplus width. Allocation changes collapse the inspector first and navigation
second, without rebuilding the terminal or moving its PTY into another process.

`src/workspace_shell.rs` owns GTK presentation only. The existing Relm4 stack,
session tabs, file tree, native command center, Vim entry, agent session manager,
Tasks and AI panels retain their original state and callbacks. All window
controls invoke the existing close/minimize/maximize actions.

## Presentation and compatibility

The visible choices are Block and Chat. Help and shell completions expose the
same two choices. Legacy `vte` and `unified` CLI/config spellings remain readable.
Local public Block panes use the existing continuous Unified VTE backend; it
retains OSC lifecycle, searchable zone history, native selection and fractional
scroll behavior. Managed external panes keep the existing Block backend rather
than changing their remote reconnect/history contract without validation.

Chat retains its single live VTE, independently scrolling transcript and guarded
send/stop controls. Metadata is packed at a fixed six pixel gap. The reference
light palette applies only to a private Chat pane snapshot when the user still
has the unmodified default palette. It never changes shared/persisted Config,
safe-mode VTE, explicit themes or custom colors. Block keeps its terminal colors
inside the light surrounding canvas.

The Block composer frame contains shared tool affordances and a hint directing
input to the terminal. It is not a second editor. Unsupported attachments,
images and code insertion are visibly disabled. Context opens a real inspector
page. The right Files tab is disabled until connected; the existing file tree
remains available in the left navigation. The neutral agent card reports that
no task is selected; opening Tasks exposes the existing real task panel.

## Rich native components and truthful data boundaries

`src/workspace_conversation.rs` renders the shared typed conversation model as
GTK widgets: a wrapped user bubble, avatar/role line, proportional heading and
paragraph, fixed-gap icon pills, task rows, a nine-line code diff with native
clipboard Copy, and expanded inline terminal. The inspector uses five metadata
rows, a current-step surface, a five-row queue and a bottom-anchored output pane.
The source data and presentation are separate; `JTERM_REFERENCE_FIXTURE=1`
selects visibly labelled sample content, without inserting sample recent items
into real history or executing an agent.

Production shell commands use the existing native command capsule and output
VTE, with a truthful Shell role header. Selection, find, terminal copy, images,
folding and retained document ownership stay with the existing widgets. Real
structured assistant headings/steps/diff have not yet been projected from the
agent store into the central typed renderer. These components currently receive
sample semantic data only; raw stdout is never interpreted as agent progress.
The existing real AI and task panels remain available through their original
actions.

The command capsule reserves wrapped text rows independently of the VTE's last
full-pane grid. Chat Enter geometry uses a one-row minimum only in compact
prompt-at-top mode; raw/full-screen and legacy Block retain their existing
height floors. Native geometry tests load the production workspace CSS, so
extra padding and toolbar widths cannot escape the narrow-window assertions.

## Remaining acceptance work

- Existing panes cannot yet switch between Unified and Chat render backends in
  place. The two mode controls explicitly apply to new/restored panes and never
  restart an existing shell. Safe hot-switching requires reader/backend and
  history-model migration.
- The external-pane exception needs native remote reconnect/history acceptance.
- Right-side Files inspection, attachment/image/code insertion, model selection,
  Apply and live agent-card projection remain unconnected and visibly disabled
  or neutral. The Tools popover lists these unavailable capabilities honestly.
- Font identity and pixel-level reference acceptance are owned by the parent
  task, which has the original JPEG. This execution environment has not viewed
  the original pixels and does not claim a 1:1 pass.
- IME preedit, 125%/150% scaling, retained scrolling under output bursts and
  long-content acceptance must be recorded separately from a successful build
  or ordinary keyboard-input check.

Native evidence is stored in the task's `evidence/anvil/` directory with mode,
window geometry, DPR, source hash and binary hash. Block screenshots may execute the shared, explicitly labelled real-shell
fixture. Chat visual acceptance uses native typed widgets and labelled sample
data. Neither path fabricates actual agent progress or test success. Screenshot
JSON records the distinction as well as a real keyboard-to-shell marker.

Detailed test results and known failures are recorded in the task evidence
report; a build or fixture screenshot alone is not interaction acceptance.

## Shared composer and companion admission

Both modes construct the same native `composer_tools` row. The caller supplies
the existing guarded Chat Send/Stop controls or Block's disabled Send control;
the factory never owns a PTY or another editor. Terminal is the same outlined
pill and Model unavailable reserves the shared minimum width. Narrow retained
canvas tests may hide secondary unavailable affordances into the same Tools
popover while preserving the model and input controls.

The navigation status is a sibling below its scroller, anchored at the bottom.
Recent and Spaces rows consume the shared heights and fixture state. The
existing unique companion admits only complete blank regions below output,
including idle wandering and its in-transit poses. The same owner paints the
shared 64 px native graphical body; canonical ASCII frames are transparent
accessibility and lifecycle metadata.

## Finite shared native components

The Tools popup consumes `reference_popup::COMMANDS`: Terminal calls the
original pane's focus path; Command palette and Search canvas use explicit
Relm outputs to existing application actions. Disabled Attach/Image/Code rows
have no execution callback. GTK popup visibility diagnostics can record the
retained scroller adjustment and first intersecting native child, plus mapped
VTE cursor/grid/scroll values; these logs are observations, not a test pass.

Product and Spaces identities decode the shared, unmodified repository PNGs
once into cached GTK textures, displayed in fixed 24/22 pixel GtkImage slots.
The workspace selector shows the real session path's basename and a neutral
branch slot when branch state is unavailable. Diff headers expose language,
counts, a real clipboard Copy surface and an unavailable Apply surface.

The existing organism presence owner paints `reference_organism::SHAPES` with
native Cairo paths. The canonical textual frames remain transparent metadata
for existing lifecycle tests; both spatial and compact seats use the shared
picture. The same attention/behavior clock drives drawing. Speech uses only
actual reactions, with dismiss and original-pane focus controls. Full speech
and body space is admitted below real output; insufficient space collapses
speech before hiding the fixed 64 px body. A rich acceptance Chat fixture hides the body so native
terminal blank cells cannot be mistaken for blank message space. No new mind,
per-message companion, PTY input source or timer is created.

Chat uses a non-measuring seat on the retained message canvas. Admission reads
actual message children, virtual placeholders and mapped floating controls;
adjustment changes hide a stale seat before the existing owner remeasures.
A passive post-layout guard refuses newly occupied space before paint, without
requesting extra frames. Native checks retain exact reading anchors through
body settings, speech, code expansion and streaming output, and refuse an
actually allocated 59 px viewport rather than shrinking the shared contour.

Native Bash history comparison remains a separate correctness issue: archived
master and this implementation both retained the literal visible PS1 prefix
in every controlled test record. Strong exact-command assertions remain
failed. Seven controlled runs, including a matched 85×26 PTY, retained all
command bytes after that prefix. Later native first-command comparisons
reproduced nonempty partial captures on both archived master and this
implementation while Bash retained and executed every input byte. In a
controlled settled-prefix/adjacent-tail-and-Enter case, archived master lost
the same final five bytes as the earlier implementation screenshot, and its
complete application-history value was byte-identical to that observation.
Other valid runs lost one, three, or eight bytes; this is not an invariant
five-byte crop introduced by the GUI rebuild. No prompt stripping or
persisted-history rewriting is used to hide either issue.

The Bash integration and command-range/provenance/finalization code are
byte-identical across that baseline and the implementation. Bare Bash marks
provide no full command metadata, so CommandStart scrapes the current VTE
range; finalization prefers any nonempty scrape and uses the typed shadow
only when it is empty. The evidence is consistent with the documented
asynchronous-feed capture race, but no per-packet VTE parse/paint trace was
recorded, so the exact scheduler ordering and any GUI-induced change in
failure probability remain unproven. The existing partial-history defect is
not repaired here and remains an acceptance limitation independent of PS1
prefix contamination.

The reference canvas header and complete overview inspector now have real UI
font capture roots. They do not enclose the terminal/content stack; native
popup isolation and the embedded Vim wheel exception remain intact. UI zoom
keeps the stylesheet's different role sizes and multiplies them by the
independent UI scale, rather than replacing every label with one size. At
scale 1 the reference header/task/metadata roles remain 12/11/9 pixels.

The existing independent UI size also applies to reference roles. It is
normalized against nominal 14pt (the existing default) using resolved
absolute font sizes in the same actual Pango context, then multiplied by the
independent UI scale. Point versus absolute-pixel units and legacy None
freezing remain unchanged; GTK handles device scale without another DPR
multiplier. Size and scale retain their separate configuration limits, with
no extra clamp on their combined factor. If font resolution is unavailable,
the layer retains its previous scale-only appearance until the next normal
appearance update.

This contract preserves the default fixed logical role sizes, including at a
different font DPI. A point-size ratio therefore cancels that DPI, whereas an
absolute-pixel value relative to nominal 14pt varies with actual font DPI.
It does not claim to restore OS font-density enlargement of the authored
default roles. The reference fixture now explicitly uses default 14pt UI
size. Older fixture captures used 10px, whose effect had been hidden by the
size-control omission; those captures are retained with their original
configuration rather than being relabeled as default14pt.

A subsequent Content-only follow-up gives the authored transcript its own
font scope: heading, body, metadata, task rows, diff headers, code and line
numbers retain their different default sizes and follow the independent
Content scale. Chrome and composer are outside that scope. The same existing
transcript continues to own culling, streaming, reading anchors and follow
intent; the font layer does not replace it or request continuous redraws.
Native PTY zoom remains independently verified rather than standing in as
evidence for authored message fonts.
