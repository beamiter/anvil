# Block native card checkpoint

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
