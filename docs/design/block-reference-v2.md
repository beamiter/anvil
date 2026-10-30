# Block native card checkpoint

The checkpoint records below predate the fixed Block input dock. Current Block
panes use the shared native bottom composer described in
[Reference workspace](../reference-workspace.md). The earlier captures and
excluded prototype remain historical evidence; the dock now uses the actual
live clip allocation rather than the prototype's fixed canvas-width estimate.

## Default-font migration

The next reference batch changes the built-in Main default to
`JetBrainsMono Nerd Font Mono 12px`: 12 logical, absolute Pango pixels, not
12 points. This is a shared implementation choice estimated from the raster,
not a claim that its original font has been identified. Missing UI size and
scale now resolve independently to 14pt and 1.0, including when an explicit
Main font or Main zoom is configured. Existing configurations that relied on
an absent UI setting inheriting Main therefore change chrome appearance.
Explicit Main/env/TOML and UI values retain their font strings, px/pt units,
and zoom authority; loading does not rewrite the user's configuration.

Chat's authored default roles remain heading 20px, body 12px and code 10px.
Their Main-group multiplier uses the new 12px production nominal, while the
UI multiplier retains its independent 14pt nominal. Consequently an explicit
older Main size has a different relative authored-message extent under this
new baseline; preserving the explicit value does not imply identical old
rich-text pixels. Settings continues preserving Pango units and independent
group mutations. The experimental Block dock remains disabled, and this font
batch does not change capture guards, running-output projection or PTY input.

The historical checkpoint notes below describe the defaults and evidence at
those earlier source revisions, before this migration.

The local PNG in `block-reference-v2.json` was inspected before implementation.
The raster's text is not treated as actual commands or application state.
This checkpoint changes only the Block center; the shared outer shell, Chat,
Main/Content font authority and terminal input/Stop implementation are retained.

New Block panes use the existing `BlockBackend`, `FinishedBlock` and single
`ActiveBlock`/`OwnedPty`. Legacy Unified session restores keep their existing
continuous terminal. Existing panes are not restarted to change presentation.
Completed output remains the existing readonly ANSI VTE; card command, exit
status and elapsed values come from the existing shell lifecycle and records.
The original native More menu remains the action entry point.

The gutter is a separate non-targetable native drawing area. It numbers
retained logical lines; soft-wrap continuation rows have no repeated number.
Its source index is cached per displayed generation and native column count;
painting visits only visible labels. It never inserts numbers into the ANSI
buffer, terminal selection, copy output or PTY input. This structural separation
does not replace native clipboard/resize/scroll acceptance.

Stage-one native evidence used actual commands yielding exit 0, exit 7 and a
60-second foreground process, captured while running and later interrupted
with Ctrl+C (exit 130). Both 1672×941 and 1356×704 native windows were captured
at DPR1. The explicit fixture Main font was DejaVu Sans Mono 10pt; UI was 10px,
which is too small under the current authored-role scaling and will be corrected
in the next capture configuration without changing production defaults.

This is an incomplete, recoverable checkpoint. Known stage-one differences:

- The old higher-specificity non-Chat rule still adds a 28px outer inset/frame.
- Header badges and More fill the header instead of using compact vertical alignment.
- Live count refreshes too early and can lag retained command insertion.
- The running card can grow to the viewport after a narrower resize and move
  completed cards out of view. This resize case is a failure, not acceptance.
- Running output retains the native prompt and has no separate gutter yet.
- Compact bottom input relocation is the next batch; no second editor is added.
- Native More access to copy/collapse/recall, gutter alignment after private
  output scrolling, unchanged Chat and the new input/IME/anchor gates remain open.

Existing history-tail, PS1-prefix, accessibility and Static-counter failures
remain separately recorded; this checkpoint does not claim to fix them or
complete pixel-level acceptance. At that stage, publication had not been authorized.

## Recoverable publication checkpoint

The current checkpoint keeps the established Block input surface. The new
Block dock is disabled at construction (`BLOCK_DOCK_VERIFIED = false`), and
the existing bottom tools/hint shelf remains visible. Chat's original layout
and input authority remain active without this Block experiment.

Confirmed source fixes include preserving the hidden collapse button's owner,
using the relocated header as the filter row's real sibling anchor, matching
native display normalization for gutter rows, keeping v2's gap inside the indexed
holder, fixed v2 geometry across density updates, compact status/menu alignment,
and preserving explicit Unified startup/config and saved local Unified panes
with a journal session ID. A local session ID is no longer treated as proof of
a remote shell; managed/external remote integration keeps its Block exception.

The larger follow-up built successfully and four targeted logic tests passed
(logical line/wrap boundaries, explicit Unified config/CLI, saved Unified, and
local saved Unified with a session ID). Its default-font native captures used
the production 14pt Main font with UI size/scale unset. Native diagnostics
measured an 18.667px command role, 12px toolbar, 10px status, and 11×29px VTE
cells. Those larger-prototype results are not a full validation of this smaller
publication checkpoint or of every input path.

Real native checks also exposed a new command-capture failure in the dock:
ordinary short commands can be truncated or receive the truncation sentinel.
The guard remains unchanged. A candidate identity policy fixed one measured
18→22-row top-anchor case but still failed another real resize and lost a
command suffix; it is not accepted. The complete excluded follow-up is archived
as `../checkpoints/unverified-block-dock.patch`, not applied. Original complete
diffs, failed runs and actual kernel-size samples remain in task evidence.

Still open: running prompt de-duplication/gutter, compact real Block input,
default-font responsive behavior, native More fold/filter/copy/recall checks,
private-output scrolling, IME/focus, stable output-reading anchors, and final
visual acceptance. No guard was weakened and no fake command/status substituted.
Local build/private-native actions were permitted; two driver failures are
preserved separately and are not counted as application passes. Publication
authority was subsequently provided to the coordinating task; this worker does
not push or change remote branches.

## Frozen-checkpoint native result

The frozen `48c2867` source compiled in release mode. Its exact test binary passed
the four targeted gutter/Unified startup/saved-session logic tests. Default-font
native captures at 1672×941 and 1356×704 used the same immutable application
binary and recorded actual exit statuses 0, 7 and 130. Configuration validation
reported no warnings or errors.

Command integrity **still fails with the Block dock disabled**. The first short
command received the truncation sentinel after the window grew between its
PromptEnd mark and submission; another running command lost a suffix; a further
short burst command received the sentinel after shrinking and restoring the
window. Numeric capture diagnostics showed an anchor row beyond the native
cursor. The failure is therefore not isolated to the excluded dock, and this
checkpoint must not be described as command-capture safe. Native kernel-size
samples remained 24×95 throughout the initial fixed-size lifecycle, changed to
16×66 on the actual smaller window and returned to 24×95 after restoration.
These samples do not establish that every resize signal was observed.

The ordinary PS1-prefix discrepancy is recorded separately from lost bytes and
sentinels. Guards remain intact; fixture strings do not repair history. Running
echo/gutter and native More layout synchronization remain open. The captures
are evidence of an incomplete implementation, not visual or behavioral acceptance.

## Optional Bash command metadata

The Bash integration now carries the existing Forge logical-command metadata
capability while preserving Anvil's token delivery, re-source cleanup and
controlling-terminal ownership. It reports a command only when the fresh Bash
history number, current HISTCMD, known privacy policy and unchanged DEBUG trap
agree. It does not enable history, alter HISTCONTROL/HISTIGNORE, install an input
observer or write a guessed command. Unknown policies, disabled history, custom
traps, ambiguous history and commands above the byte cap retain bare markers.
Native capture and reviewed-submission guards are unchanged.

The added 22 real-Bash PTY tests cover full logical commands, Unicode/readline
edits, multiline input, privacy omissions, duplicate/space filtering, stale or
imported history, trap changes, byte bounds and unchanged shell status/locale.
The existing locale tests and token/FD/environment checks passed. The existing
transport suite passed unchanged on the available Python 3.13 runtime; its
Python 3.10 run failed on the pre-existing BaseException.add_note requirement,
which remains recorded rather than changing its assertion.

A native prototype used the frozen application together with the explicitly
sourced changed integration as two separately hashed inputs. The original
three commands and the post-resize burst then matched app history byte for byte,
with actual statuses 0, 7 and 130. This is shell metadata evidence, not proof that
VTE's asynchronous capture fallback is repaired: the native anchor diagnostic
still failed after restoration when command metadata was available. History-off
and other unsupported-policy fallback behavior remains open.

A native component gate separately passed real clipboard bytes without painted
gutter numbers, fold/unfold ownership, the filter's actual parent/sibling and
zero transcript rescans on repeated draw/private scrolling. It invokes the
production callbacks; More popover document-height synchronization and the
keyboard admission path still require their own gates. Its first attempt omitted
the fixture's action binding and failed before clipboard contents were supplied;
the corrected fixture kept every assertion.

The subsequent `1c351e0` release and test builds completed. The native run sourced
the integration extracted from that exact application binary, first checking its
bytes against the committed script. All three original commands and the
post-resize burst matched history exactly; eight targeted logic/token/guard
tests and the native component gate passed on that snapshot. Default Main 14pt,
UI font unset, DPR1 and both real window sizes were retained. This closes only
the history-enabled metadata path. The previously recorded history-off capture
failures, running-card resize geometry, More document-height synchronization,
compact Block dock and remaining interaction/visual acceptance stay open.

## More fold document synchronization

More now updates the exact record's retained height when it changes fold state,
then samples the actual native allocation after two frames. The selected-card
chrome participates in that sample. Filtered records keep their zero-height
sentinel; weak card/pane references and identity checks retire stale callbacks.
The pane's viewport and visible-card set are reconciled without returning the
reader to live output or writing input bytes. The zero-output row-count estimator
now agrees with the text estimator and constructor's header-only geometry.

A strict display-backed test used the same test harness with old committed
`b20178e` product source and the local fix. Old source failed: the folded card
was 140px while metadata remained 226px. The fix passed in both Block and Chat:
fold/unfold metadata equaled actual height, all output bytes survived, reading
intent stayed set and no terminal input was emitted. The old failure and five
external native frames remain preserved; the fixed run retained 27 native frames.
The original complete More/focus/retired-card regression and visible/parked fold
regression passed unchanged in their assertions. Their More lookup now follows
the actual button field after the new Block header moved it out of quick actions.
This does not claim that every older keyboard/bulk-fold estimate has become an
exact post-layout allocation; the older regression's geometry trace is retained.

History-disabled control runs separately reproduced sentinel/PS1/tail loss on
old `686202a`/core `366c554` and the current frozen history-enabled-metadata
checkpoint with history explicitly disabled. The strict pair held the first
prompt until the same 1356×704 window size, then ran the original commands and
resize sequence. The PTY budgets differed between implementations, so this
confirms the old failure class but leaves any increased impact from the new UI
unattributed. No capture, input-shadow or reviewed-submission guard was changed
in the More fix.

## Block footer state colors

The retained Block shelf now uses the dark control surfaces from the new Block
reference. Enabled Send keeps its blue state, while disabled Send has a visibly
different deep blue-gray fill, border and readable muted arrow. Sensitivity and
input authority are unchanged. The selectors require the Block workspace and
retained shelf; no spacing, dimensions, Main font defaults or Chat controls change.
The first bright disabled-Send rendering was retained as a rejected visual stage.

The final release was captured at both 1672×941 and 1356×704, DPR1, default
Main 14pt and UI size unset. Real command metadata remained byte-exact across
the original three commands and post-resize burst. Separate before/after Chat
captures used the same explicitly labelled native acceptance fixture and a
fixture-only disabled caret blink: all 954624 window pixels were identical.
These static images are not a no-flash, full IME or complete behavior certification.
The earlier More/fold logs retain native allocation warnings whose old/new
attribution is unresolved, distinct from their passing height/ownership assertions.
