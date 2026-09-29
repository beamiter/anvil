# Anvil upgrade rounds

This ledger records the behavior-backed increments in the current upgrade
pass.

Rounds 1–10 record the preceding pass; this pass's additional thirty-three rounds
are numbered 11–43.

The numbering stopped at 43 through 2026-08-29; later repin rounds lived in
`CHANGELOG.md` and `handoff.md` only. Round 44 resumes the ledger for this
evolve pass.

1. **Prefix boundary** — install and uninstall reject empty, relative,
   control-bearing, or parent-traversing prefixes while retaining valid Unicode
   and whitespace.
2. **Binary boundary** — explicit binary directories use the same validation
   and an empty override can no longer silently fall back to `PREFIX/bin`.
3. **Shared-data boundary** — `--data-dir`/`XDG_DATA_HOME` is validated before
   any write, including lexical `..` defense for DESTDIR concatenation.
4. **Recursive purge preflight** — config and state roots are both checked
   before installed files are removed, preventing a late unsafe purge failure
   from leaving a partial uninstall.
5. **Root staging semantics** — explicit `DESTDIR=/` remains staged for cache,
   summary, and legacy-install diagnostics even after slash normalization.
6. **Build-free packaging** — `--binary PATH` bypasses Cargo/Nix while retaining
   the normal assets, configuration, desktop, and staging layout.
7. **Pinned artifact identity** — prebuilt symlinks are rejected and an opened
   `/proc/self/fd` descriptor is matched to the path's device/inode.
8. **Atomic binary update** — a mode-correct same-directory temporary is renamed
   over the destination; before that commit point EXIT cleanup preserves the
   old binary and removes the uncommitted temporary.
9. **Desktop-entry correctness** — separate `Exec`/`TryExec` encoding preserves
   spaces and shell-like characters, action suffixes are no longer eaten, and
   the entry itself is atomically replaced.
10. **Config and installation regression suite** — dangling config symlinks are
    preserved; the new real DESTDIR suite checks modes, source/destination
    symlinks, data overrides, path rejection, purge ordering, and cleanup.
11. **Complete source preflight** — support, shell, workflow, notebook,
    desktop, metadata, icons, and optional config are checked before build or
    first write.
12. **Artifact/backend exclusivity** — `--binary` and an explicit `--backend`
    are rejected together instead of accepting a meaningless build choice.
13. **Empty artifact rejection** — the pinned descriptor must be non-empty and
    a regression proves the existing binary survives failure.
14. **Atomic support tool** — `anvil-support-bundle` is committed through a
    mode-0755 sibling temp and rename.
15. **Atomic shell integrations** — every integration file receives an
    independent mode-0644 atomic commit.
16. **Frozen workflow inputs** — the six workflow sources are captured in an
    explicit manifest, preflighted, atomically installed, and asserted by the
    path contract.
17. **Atomic notebook install** — the welcome notebook cannot be exposed
    partially during reinstall.
18. **Desktop structure validation** — canonical Exec/TryExec counts and the
    absence of alternate command lines are required before rename.
19. **Atomic metadata and icons** — AppStream, SVG, and PNG assets preserve
    their public modes and replace destination links rather than following them.
20. **Hard-link config publication** — initial config uses a same-directory
    temp plus atomic no-clobber link instead of a check/copy race.
21. **Concurrent-writer contract** — a deterministic wrapper makes another
    creator win immediately before link; its bytes survive and temps are gone.
22. **Scoped symlink-ancestor gate** — normalized non-root DESTDIR/data roots
    are checked component-by-component from `/`; disguised root links and
    recursive purge roots fail before mutation without rejecting host
    operations. The preflight does not claim concurrent-race exclusion.
23. **Unset-PATH handling** — legacy/shadow diagnostics remain safe with PATH
    absent under nounset.
24. **Application remote gate** — character and byte budgets, spoofing,
    target/user/session/artifact semantics, and argv total are checked once.
25. **SSH option grammar** — legitimate `-p 22` and `-o Name=value` remain,
    while bare destinations and `--` inside `ssh_args` fail closed.
26. **Checked connection argv** — fresh tabs and every reconnect repeat the
    gate at the process boundary.
27. **Restore-before-spawn safety** — workspace restore rejects invalid managed
    hosts, never resolves profile 129 by name, and falls back locally instead
    of replaying stale remote argv.
28. **Reconnect UI atomicity** — argv is validated before the old pane widget
    is removed, so a bad runtime target cannot destroy the visible pane.
29. **Checked remote-fs probes** — every probe/stream and cancel cleanup applies
    the app gate, with bounded safe labels and a 128-index selector.
30. **Consumer/UI regression suite** — tests cover spoofing, semantic option
    confusion, high indexes, and pre-spawn rejection; picker rows are bounded
    and safe-display normalized.
31. **Exact search render identity** — Block card searches carry render stamps
    and rebuild retained queries after a resize/re-feed even at a stationary
    one-hit edge; cross-block activation reaches the named surface occurrence
    or fails closed. Cargo and Nix consume the same published hardened-core
    revision.
32. **Foreground-owned OSC lifecycle** — definite PTY foreground ownership by
    ssh, tmux, docker, or another child rejects both nested `C` and `D` marks,
    so foreign shell integration cannot change local depth or finish a command.
33. **Composable card states** — outcome, hover, selection, and bookmark
    styling use independent properties plus explicit compound selectors, so
    one state no longer erases another.
34. **Display-backed quality gate** — the real GTK/VTE regressions are named in
    one script, run in isolated D-Bus/Xvfb processes, and are shared by CI and
    `make test-display`.
35. **Density-safe virtual document** — hot density changes update finished
    and inline cards together, preserve filtered zero-height sentinels, and
    synchronize parked placeholders plus the viewport metadata before one PTY
    geometry update.
36. **Fresh bounded branch chips** — a 64-entry `cwd → HEAD` locator LRU avoids
    repeated directory walks while safely rereading HEAD for every card, so
    branch switches are visible immediately and negative lookups expire after
    200 milliseconds.
37. **Focused-card keyboard and safe reuse** — one shared handler preserves
    Block navigation, selection, filtering, folding and bookmarks when focus
    sits on finished-card chrome while printable input still returns through
    the live VTE/IME. A dynamic, spacer-safe hint advertises only actions the
    current selection can perform; lone safe commands gain a foreground-owned,
    clean-prompt `Ctrl+Enter` path that inserts first and sends CR only after an
    exact stable VTE render, consuming every refusal. Alternate-screen takeover clears hidden selection,
    and Delete stays unadvertised until grouped removal has a matching undo.
38. **One-shot Block orientation** — an empty Block pane exposes card selection,
    context actions, and cross-block search without measuring or intercepting the
    live surface; a completion or restored history dismisses it permanently,
    while Unified/VTE and inline-notice ownership remain untouched.
39. **Selection-owned Enter** — both recall and re-run refuse with a bell while
    retaining key ownership, so busy/dirty prompts and running applications
    cannot receive an advertised selection Enter as unrelated input; the hint
    explicitly scopes those actions to a ready prompt.
40. **Lossless recall with accessible controls** — selected multiline commands
    are never reduced to their first line when bracketed paste is unavailable,
    while focused header buttons retain ordinary GTK Return/Space activation and
    only the explicit Ctrl+Enter chord enters Block re-run; the natural-width cap
    now follows the longest hint so `Esc cancel` is not permanently ellipsized.
41. **Alternate-screen-safe orientation** — the first-use overlay suspends while
    a full-screen program owns the surface and returns on exit, so an initial TUI
    is never hidden behind guidance intended for an empty prompt.
42. **Truthful, focus-safe Block interaction** — first accepted human input
    permanently retires orientation guidance; selection hints expose counts,
    recall-all scope, and visible refusal reasons without claiming readiness.
    Every history recall shares the verified empty-prompt gate, alternate-screen
    navigation cannot create a hidden selection, and faded action strips leave
    both pointer targeting and keyboard focus.
43. **Visible-selection copy and current feedback** — native and cross-VTE text
    highlights take precedence over whole-card copy and oversized aggregation
    fails atomically; generation-owned refusal timers refresh repeated status
    and can restore only the steady legend, never an older transient message.

44. **Quit waits for the execution journal** — `force_quit` now pins a bounded
    `execution_journal::flush` before `quit_allowed` and `window.close`, so the
    last command's captured output is not abandoned when closing the window wins
    the race to the background writer. A structural regression guards the
    ordering because nothing is observable once the process has left.
45. **Agent-task anchor hardening** — isolated worktree tasks now refuse
    background-output blocks even when the shared block preflight still treats
    commandless captured output as attachable evidence. Fix/Create-from-block
    keeps requiring exact shell-reported command metadata for foreground blocks.
    Unit tests pin the stricter gate against the shared preflight contract.

46. **Validation launch failure bookkeeping** — a validation PTY that never
    execs is cancelled without blocking the next attempt; a regression now pins
    that `record_task_terminal_launch_failure` leaves validation in
    `Cancelled` while agent launch failures remain retryable.

47. **Launch failure role isolation** — agent pre-exec spawn failures mark the
    task failed and retryable without touching validation state. A regression
    pins that `record_task_terminal_launch_failure` for agent terminals leaves
    validation at `NotRun` instead of cancelling an unrelated validation attempt.

48. **Validation launch failure isolation** — validation pre-exec spawn failures
    cancel the validation attempt without marking the agent task failed when
    both terminals are bound. A regression pins that role-specific bookkeeping
    stays independent.

49. **Validation launch failure agent retry isolation** — validation pre-exec
    spawn failures must not register an agent terminal retry pin when both
    terminals are bound. A regression pins the agent session stays off the
    retry queue.

50. **Persisted earlier-output notice** — `BlockData` now carries
    `output_head_dropped` so the finished-card "Earlier output not retained"
    notice survives history save/restore and clear undo. Pre-notice frames still
    decode with the notice off.

51. **Organism vigil-tier UI contract** — Full-motion bridge selection mirrors
    `VisualTransition::between` for the vigil-tier arcs (second-failure settle
    and idle Failure→Stuck / Recovery→Cautious); Calm/Static still snap. Attach
    discloses when the inline organism card cannot mount so Unified panes do not
    look like a silent organism failure. Behavior itself lands with core
    `fbfcafa` (pending push/repin from `33093da`).
52. **Organism error-hold heal settle UI contract** — Full motion also mirrors
    SitNearError/InspectError→Recovery/Cautious/Stuck and Cautious→Recovery
    once core recognizes those bridges (pending push/repin). Calm/Static still
    snap.
53. **Shared finished-block output notice** — the earlier-output display string
    comes from `jterm_core::output_notice` (pending core push/repin). Disk
    schema stays the bool `output_head_dropped`.
54. **Organism celebrate-hold relapse UI contract** — Full motion mirrors
    Celebrate/CelebrateBig→Failure/Stuck once core recognizes those bridges
    (pending push/repin). Calm/Static still snap.
55. **Resumable cross-block search idle continuation** — budget stops return a
    `CrossBlockSearchCursor`; the dialog runs `glib::idle_add_local` slices
    cancelled by search generation (parity with forge round 91). Unit
    regressions pin resume cursors, hit-cap vs `scan_incomplete`, and status
    copy for budget-stopped scans.
56. **Organism failure-push + error-hold success UI contract** — Full motion
    mirrors GuardFailure/Stuck→RestAfterPush and
    Inspect/SitNear→Celebrate{,Big} once core recognizes those bridges
    (pending push/repin). Calm/Static still snap.
57. **Organism celebrate-hold push UI contract** — Full motion mirrors
    Celebrate/CelebrateBig→RestAfterPush once core recognizes those bridges
    (pending push/repin). Calm/Static still snap.
58. **Organism WatchSettled fail + idle settles UI contract** — Full motion
    mirrors WatchSettled→Inspect/SitNear and
    Celebrate{,Big}/RestAfterPush→Idle once core recognizes those bridges
    (pending push/repin). Calm/Static still snap.
59. **Organism WatchCommand/WatchAgent finish UI contract** — Full motion
    mirrors WatchCommand→Celebrate{,Big}/Inspect/SitNear/RestAfterPush and
    WatchAgent→Celebrate/Inspect/SitNear once core recognizes those bridges
    (pending push/repin). Calm/Static still snap.
60. **Shared CrossBlockSearchCursor** — resume cursor / mid-record /
    generation-current predicate / cross-block budget constants come from
    `jterm_core::cross_block_search` (pending core push/repin). GTK idle and
    `CrossBlockSearchReport`/`FindScanBudget` stay local.
61. **Organism UnknownOutcome settle/overwrite UI contract** — Full motion
    mirrors UnknownOutcome→Idle and UnknownOutcome→InspectError once core
    recognizes those bridges (pending push/repin). GlanceAside stays live-only.
    Calm/Static still snap.
62. **Organism UnknownOutcome success/sit overwrite UI contract** — Full motion
    mirrors UnknownOutcome→Celebrate{,Big} and UnknownOutcome→SitNearError once
    core recognizes those bridges (pending push/repin). Calm/Static still snap.
63. **Organism WatchAgent/WatchSettled→RestAfterPush UI contract** — Full motion
    mirrors WatchAgent→RestAfterPush and WatchSettled→RestAfterPush once core
    recognizes those bridges (pending push/repin). Calm/Static still snap.

Verification: `bash scripts/test-install-paths.sh`, `bash -n
scripts/{install,uninstall,test-install-paths}.sh`, plus the full Cargo gates.
64. **UnknownOutcome→Guard* Full-motion UI** — Full motion mirrors
    UnknownOutcome→GuardFailure/Stuck/Recovery/Cautious vigil settles once core
    recognizes those bridges; Find overlay scan caps come from core
    `FIND_OVERLAY_SCAN_*`.

65. **Forge round-numbering parity note** — anvil and forge keep independent
    upgrade counters. Shared-feature catch-up for this evolve pass maps forge
    rounds 89–103 onto anvil 50–64 (same titles: vigil-tier through
    UnknownOutcome→Guard* + Find overlay scan caps). No new behavior; ledger
    only, so a forge-ahead number does not imply anvil is missing the feature.

66. **Ambient VisualTransition N/A + FIND overlay literal probe** — ambient
    poses do not route through `VisualTransition::between`; Find overlay
    production already uses `FIND_OVERLAY_SCAN_*` (no leftover CROSS_BLOCK_*
    literals in `find.rs`).

67. **Block-history sticky failure surface deferred** — forge’s
    `history_notice` Retry bar is not mirrored yet. Anvil stays log/toast-only;
    Relm4 chrome is not the blocker — see round 72 for concrete unblock APIs.

68. **FindScanBudget constructor semantic pin** — palette
    `FindScanBudget::for_cross_block` uses shared `CROSS_BLOCK_SCAN_*`
    (8 MiB / 48 ms); live overlay `FindScanBudget::new` uses
    `FIND_OVERLAY_SCAN_*` (4 MiB / 12 ms). Unit test pins both constructors
    (pairs forge round 107).

69. **Full-motion semantic_bridges catch-up** — Full-motion UI contract list
    mirrors every core `VisualTransition::between` pair (64 bridges), closing
    gaps left by the staged UnknownOutcome / Watch* / vigil rounds (pairs
    forge round 108).

70. **Forge 104–106 catch-up note** — forge rounds 104 (Find-overlay /
    finished-output test pins), 105 (Ambient VisualTransition N/A), and 106
    (idle cross-block continue TODO close) map onto anvil 66 (Ambient + FIND
    overlay literal probe) plus forge-only ledger for the idle TODO already
    closed in anvil's round-60/68 CrossBlock form. No missing anvil behavior;
    numbering only so a forge-ahead counter does not imply a feature gap.

71. **WatchAgent→UnknownOutcome None + agent Celebrate UI** — Full motion
    pins WatchAgent→UnknownOutcome as None; agent-driven recovery stays
    Celebrate (never CelebrateBig), matching core quiet-nod contract
    (pairs forge round 109).

72. **Block-history sticky surface — concrete unblock (still deferred)** —
    Survey of forge `ui/history_notice.rs` + `retry_history_persistence`
    against anvil shows Relm4 is fine for a minimal sticky bar (plain
    `gtk::Box` under the top bar, same shape as forge — no missing Relm4
    Banner API). What is missing, and blocks a non-heroic port:

    1. **Worker-labeled Save Block history.** Forge enqueues saves as
       `BLOCK_HISTORY_PERSIST_OPERATION` (`"Save Block history"`) so
       `drain_failures` → `persistence_failure_surface` can raise the bar.
       Anvil’s `TermView::save_history` is still synchronous on the GTK
       thread; failures only `log::warn` at Drop/clear/undo. A sticky bar
       wired only to `report_persistence_failures` would never light up.

    2. **`HistoryLoadOutcome` + `retry_history_persistence`.** Forge Retry
       is ReloadFirst when load Failed (saving again would refuse and must),
       else SaveAgain. Anvil has revision/baseline sync load/save but no
       Failed-load outcome that refuse-to-overwrite rides on, so Retry
       cannot choose correctly.

    Unblock when both exist (or an intentional SaveAgain-only interim with
    sync-error promotion into the same surface). Then: sticky bar in
    `AppModel` view!, route that operation off toast cooldown, Retry walks
    every Block `TermView`. Pairs forge round 78; toast-only stays
    intentional until then.

73. **WatchCommand→UnknownOutcome None UI** — Full motion pins
    WatchCommand→UnknownOutcome as None (missing exit status snaps), pairing
    core intentional None beside WatchAgent→UnknownOutcome (pairs forge
    round 110).

74. **Core tip STAGE_PREFIXES strace/scriptlive (len 60)** — path-patched
    local `jterm_core` peels `strace` / `scriptlive` (STAGE_PREFIXES 58→60)
    with `classify_command` see-through; Cargo manifests stay on the
    published pin (pending push/repin). Pairs forge round 111.

75. **Block-history sticky Retry foundation** — landed the round-72 APIs
    without sticky chrome yet: `HistoryLoadOutcome` / `HistoryRetryAction` /
    `BLOCK_HISTORY_PERSIST_OPERATION`, Failed-load refuse-to-overwrite,
    `TermView::retry_history_persistence`, and `history_notice`
    `persistence_failure_surface` + sync-failure parking. Still open:
    labeled async enqueue of Save Block history, and AppModel sticky bar
    that drains parked/worker failures and calls Retry. Pairs forge round 78.

76. **FindScanBudget / Options / Report from core** — re-export
    `FindScanBudget`, `CrossBlockSearchOptions` / `CrossBlockSearchScope`, and
    hit-generic `CrossBlockSearchReport<CrossBlockHit>` from path-patched
    core; `CrossBlockHit` stays local (exit_code / duration_ms / cwd). GTK
    idle stays here. Pairs forge round 112.

77. **Guard*/Watch*/Celebrate*/Glance intentional None UI** — Full motion
    pins WatchSettled→UnknownOutcome, Watch*→Guard*, Guard*→Celebrate*,
    Celebrate*→Watch*, Idle/Rest→Guard*, and GlanceAside as source/target
    (pairs forge round 113; core audit wave).

78. **Block-history sticky chrome stub** — AppModel hosts forge-shaped sticky
    Retry bar under the top bar (`history_notice::build_block_history_notice`).
    `report_persistence_failures` routes `"Save Block history"` off toast
    cooldown onto the bar and drains parked sync refusals; Retry walks every
    Block `TermView::retry_history_persistence`. Still open: labeled async
    `persistence::enqueue` for Save Block history (sync GTK-thread save +
    parking remains the production path). Pairs forge round 78.

79. **Core tip STAGE_PREFIXES systemd-cat/aa-exec (len 62)** — path-patched
    local `jterm_core` peels `systemd-cat` / `aa-exec` (STAGE_PREFIXES 60→62)
    with `classify_command` see-through (and full STAGE membership pin);
    Cargo manifests stay on the published pin (pending push/repin). Pairs
    forge round 114.

80. **Labeled Save Block history enqueue** — `TermView::save_history` snapshots
    on the GTK thread then `persistence::enqueue_weighted` under
    `BLOCK_HISTORY_PERSIST_OPERATION` (`"Save Block history"`). Baselines and
    pending Clears are `Arc<Mutex<_>>` so the worker can commit revision /
    tombstone authority; worker refusals drain onto the round-78 sticky bar
    via `report_persistence_failures`. Failed-load / admission refusals still
    park sync. Pairs forge round 78 (closes the round-72 worker-label gap).

81. **Block-history sticky gap close** — survey against forge sticky/Retry:
    (1) toast routing completeness via `partition_persistence_failures` so
    `"Save Block history"` never enters toast cooldown even when mixed with
    routine drains; (2) ReloadFirst UX pins — revalidate without reinstalling
    cards, then labeled save; (3) Clear parking — armed Explicit Clear bypasses
    the Failed-load overwrite refuse and lifts Failed→Idle (forge
    ExplicitReplace contract), so Clear no longer parks a sticky refusal
    instead of writing the tombstone. Pairs forge round 78 / 115 note.

82. **Ambient / Typing VisualTransition N/A pin** — Full-motion UI pins that
    Idle/Explore/Sleep/Approach disposition exchanges and Typing-surface entry
    onto WatchCommand stay `VisualTransition::between` None (pairs core ambient
    N/A pin and forge round 116). Round 66 documented Ambient N/A; this lands
    the regression.

83. **Core tip STAGE_PREFIXES systemd-inhibit (len 63)** — path-patched local
    `jterm_core` peels `systemd-inhibit` (STAGE_PREFIXES 62→63) with
    `classify_command` see-through; Cargo manifests stay on the published pin
    (pending push/repin). Catch-up after ambient round 82. Pairs forge round 117.

84. **Core tip STAGE_PREFIXES systemd-socket-activate (len 64)** — path-patched
    local `jterm_core` peels `systemd-socket-activate` (STAGE_PREFIXES 63→64)
    with `classify_command` see-through and membership pin; Cargo manifests
    stay on the published pin (pending push/repin). Pairs forge round 118.

85. **Clear-vigil Idle Full-motion UI contract** — Full motion mirrors
    core Inspect/SitNear→Idle and Guard*→Idle bridges (`between()` 64→70).
    Pairs forge round 119; core clear-vigil Idle survey wave.

86. **Error-hold/Unknown→Rest + Watch*→Idle UI contract** — Full motion
    mirrors core Inspect/SitNear/Unknown→RestAfterPush and Watch*→Idle
    (`between()` 70→76). STAGE docs rounds 83–84 stay membership-only
    (len 63/64); the 70 count was round 85, this round catches 76.
    Pairs forge round 120.

87. **Find overlay continue bookmark-empty status** — `pending_scan_continue`
    idle slices reuse `overlay_scan_status` so a finished Bookmarked scan
    that added no hits keeps the bookmark-empty copy instead of generic
    "No matches." Pairs forge round 121.

88. **Core tip STAGE_PREFIXES daemonize/setlock/s6-setuidgid (len 67)** —
    path-patched local `jterm_core` peels `daemonize` / `setlock` /
    `s6-setuidgid` (STAGE_PREFIXES 64→67) with `classify_command` see-through
    (including `--` before the s6 account) and leftover pin for `s6-envdir` /
    `s6-log` / runit helpers; Cargo manifests stay on the published pin
    (pending push/repin). Pairs forge round 122.

89. **Celebrate*/Rest/GuardRecovery hold-overwrite UI contract** — Full motion
    mirrors core Celebrate*/RestAfterPush/GuardRecovery finish overwrites
    (`between()` 76→90). STAGE docs round 88 stays membership-only (len 67);
    the 76 count was round 86, this round catches 90. Rest/GuardRecovery→Watch*
    stay None. Pairs forge round 123; core hold-overwrite survey wave.

90. **Find bookmark empty-query browser pin** — Bookmarked empty-reason stays
    `None` when an empty query still has eligible scoped text (browser, not
    QueryMismatch). Pairs forge round 124 (stale bookmark ids).

91. **GuardFailure/Stuck/Cautious→error-hold None UI** — Full motion pins
    Failure/Stuck/Cautious→Inspect/Sit/Unknown as None beside Recovery's
    finish-overwrite bridges. Pairs forge round 125.

92. **Core tip fail-closed deepen (STAGE nest + transparency; len 68)** —
    path-patched local `jterm_core` / `jagent`: timeout/nice nests around
    `daemonize`/`setlock`/`s6-setuidgid`, jagent peels optional `--` before
    the setuidgid account, membership + DISPATCHES pin `len == 68` (wave-23
    triple + `gnome-session-inhibit`), and transparency partition documents
    STAGE names outside `select_execution_wrappers_mode` plus intentional
    PIPE-only `unshare`/`nsenter`. Cargo manifests stay on the published pin
    (pending push/repin). Pairs forge round 126.


93. **Find stale-bookmark empty-reason pin** — bookmark ids absent from the
    current retained records list are `NoRetainedBookmarks`, matching forge
    round 124's identity. Pairs forge round 127 (dedicated empty-query browser
    pin).

94. **Error/unknown→Watch* None UI** — Full motion pins Inspect/SitNear/
    Unknown→Watch* as None beside Celebrate*/Rest/Recovery→Watch*. Pairs forge
    round 128; core `error_and_unknown_holds_never_bridge_to_watch_poses`.

95. **Sticky near-miss operation labels** — padded / cased / truncated
    `"Save Block history"` strings stay on the toast surface; only the exact
    worker label raises the sticky bar. Pin
    `near_miss_operation_labels_stay_on_the_toast_surface`.

96. **Find mixed stale+live bookmark empty-reason** — stale ids beside a live
    bookmark do not collapse to `NoRetainedBookmarks`; empty query stays a
    browser and a live query miss stays `QueryMismatch`. Pairs forge round 129.

97. **Core tip STAGE_PREFIXES uclampset/gamemoderun (len 70)** — path-patched
    local core teaches util-linux util clamp + GameMode env launcher peels;
    GuardFailure/Stuck/Cautious→Celebrate* None survey pin; `between()` stays
    90. Manifests stay on published pins. Pairs forge round 130.

98. **InspectError→SitNearError Full-motion (90→91)** — Full-motion
    `semantic_bridges` mirrors core Inspect hold second-failure overwrite.
    Pairs forge round 132.

99. **Failure/Stuck/Cautious→Watch* + tier overwrite None UI** — Full motion
    pins Failure/Stuck/Cautious→Watch* beside Rest/Recovery→Watch*, and
    SitNear→Inspect / Celebrate↔CelebrateBig as None. Pairs forge round 133.

100. **Sticky near-miss whitespace labels** — tab / CR / NBSP variants of
     `"Save Block history"` stay on the toast surface beside round 95's
     padded/cased/truncated pin. Pairs forge round 134 file-tree leftover.

101. **Sticky near-miss invisible labels** — ZWSP / BOM / VT variants of
     `"Save Block history"` stay toast-only beside round 100 whitespace.
     Pairs forge 135.
102. **FS permission vs missing public copy** — `user_facing_fs_error` keeps
     PermissionDenied distinct from NotFound (pairs forge 134/136).
103. **Find MetadataMismatch with stale extras** — live bookmark failing
     metadata filters stays MetadataMismatch when stale ids remain. Pairs
     forge 137.
104. **Ambient→Watch* None UI** — Idle/Explore/Sleep/Approach never bridge to
     WatchCommand/Agent/Settled under Full motion. Pairs forge 138.

105. **Ambient→Guard*/Celebrate* None UI** — Explore/Sleep/Approach never
     bridge to GuardFailure/Stuck/Recovery/Cautious or Celebrate/CelebrateBig
     under Full motion (Idle/Rest→Guard* already pinned). Pairs forge 139.
106. **Find NoRetainedTextInScope with stale extras** — Output-scope live
     bookmark without retained output stays NoRetainedTextInScope when stale
     ids remain. Pairs forge 140.
107. **Sticky near-miss soft-hyphen/WJ/bidi labels** — soft hyphen / word
     joiner / LRM / ZWNJ / figure-space variants of `"Save Block history"`
     stay toast-only beside rounds 100–101. Pairs forge 141.

108. **CelebrateBig finish arcs Full-motion UI** — Full motion pins all fifteen
     CelebrateBig inbound/outbound Some bridges and WatchAgent→CelebrateBig
     None (pairs core `celebrate_big_finish_arcs_mirror_celebrate_except_watch_agent`
     and forge round 142).

109. **output_notice Earlier-only disk schema pin** — Truncated/Partly stay in
     the shared known set for forge's `Option<String>` persistence; anvil
     BlockData keeps bool `output_head_dropped` only. Pairs forge round 143
     Truncated/Partly history round-trip.

110. **Find Hit optional chrome converge note** — forge gained optional
     exit_code/duration/cwd on `CrossBlockHit` plus palette outcome suffix;
     anvil comment/report docs stop calling the fields anvil-only. Pairs forge
     round 144; core Hit pin retargeted.

111. **Sticky Retry sync-refusal reopen** — `retry_block_history` hides the
     bar optimistically then re-raises via `show_block_history_failure` when
     `retry_history_persistence` returns Err. Pairs forge 145 ReloadFirst
     labeled-save chain.

112. **Find Hit optional chrome comment converge** — report/docs treat
     exit_code/duration/cwd as optional shared chrome (not anvil-only) after
     forge round 144. Pairs forge 146.

113. **semantic_bridges list len == 91 + CelebrateBig fifteen lockstep** —
     Full-motion contract list asserts length against core `between()` 91;
     CelebrateBig Some arcs stay fifteen; Celebrate↔CelebrateBig Nones join
     the finish-arc UI pin. Pairs forge 147.

114. **Ambient→Inspect/Sit/Unknown/Rest None UI** — Explore/Sleep/Approach
     never bridge to error/unknown holds or RestAfterPush under Full motion.
     Pairs forge 148 / core ambient hold/rest pin (`between()` stays 91).

115. **Find Command-scope NoRetainedTextInScope + stale** — whitespace-only
     command with retained output stays NoRetainedTextInScope under Command
     scope when stale ids remain. Pairs forge 149.

116. **Sticky near-miss line/paragraph separators** — U+2028 / U+2029 variants
     of `"Save Block history"` stay toast-only beside rounds 107/100. Pairs
     forge 150.

117. **Inspect/SitNear→UnknownOutcome Full-motion (91→93)** — Full-motion
     `semantic_bridges` mirrors core error-hold missing-exit overwrites.
     CelebrateBig fifteen lockstep recounts against `between()` **93**.
     Pairs forge 151.

118. **Idle→hold/cele/rest None UI** — Idle never bridges to Inspect/Sit/
     Unknown/Celebrate{,Big}/RestAfterPush under Full motion (live finishes
     via Watch*). Pairs forge 152 / core Idle pin.

119. **CrossBlock continue cancel scheduled-ahead + gen-0** — palette idle
     continue pins scheduled generation ahead of live and gen-0 finished
     (no resume) as cancel. Pairs forge 153 / core cancel edge.

120. **Sticky near-miss NNBSP/MMSP/invisible-math labels** — U+202F / U+205F /
     U+2062 / U+2063 / U+2064 variants of `"Save Block history"` stay toast-only
     beside rounds 116/107. Pairs forge 154.

121. **Find All-scope NoRetainedTextInScope + stale** — whitespace-only command
     and output stay NoRetainedTextInScope under All scope when stale ids remain.
     Pairs forge 155.

122. **Sticky near-miss ZWJ/RLM/FA/ideo/NEL labels** — U+200D / U+200F /
     U+2061 / U+3000 / U+0085 variants of `"Save Block history"` stay toast-only
     beside rounds 120/116. Pairs forge 156.

123. **Find Command/Output-scope QueryMismatch + stale** — live bookmark whose
     scoped text misses the query stays QueryMismatch under Command and Output
     when stale ids remain (All-scope query miss already pinned). Pairs forge 157.

124. **CrossBlock cancel ahead-without-resume + wrapping finished** — scheduled
     generation ahead of live cancels even with an empty resume; wrapping
     MAX→0 bump with a finished cursor cancels the same way. Pairs forge 158.

125. **Core tip STAGE_PREFIXES openvt (len 71)** — path-patched local core
     teaches kbd `openvt` VT launcher peels (`-c`/`--console` meta) plus
     timeout/nice nest classify; Inspect/Sit→Unknown `between()` **93** already
     ledgered. Manifests stay on published pins. Pairs forge 160.

126. **WatchSettled finish arcs Full-motion (6 Some)** — Full motion pins all
     six WatchSettled→Celebrate{,Big}/Inspect/Sit/Rest/Idle bridges and keeps
     UnknownOutcome intentional None. Pairs forge 161 / core
     `watch_settled_finish_arcs_cover_pass_fail_rest_and_idle`.

127. **CrossBlock cancel live wrapping ahead of scheduled** — live generation
     wrapping ahead of scheduled (`0` vs `u64::MAX`) cancels with or without a
     resume — reverse of the MAX→0 schedule bump. Pairs forge 162 / core cancel
     edge.

128. **Core tip PATH wave-30 ctl/notify leftovers** — path-patched local core
     keeps `systemctl` / `busctl` / `journalctl` / `timedatectl` / `resolvectl`
     / `systemd-notify` / `systemd-mount` / `chvt` / `aa-status` out of STAGE
     beside openvt 71. Manifests stay on published pins. Pairs forge 163.

129. **Sticky near-miss punct/thin/hair/ALM/bidi-isolate labels** — U+2008 /
     U+2009 / U+200A / U+061C / U+2066–U+2069 variants of `"Save Block history"`
     stay toast-only beside rounds 122/120. Pairs forge 164. Leaves 126–128 for
     WatchSettled/CrossBlock/PATH wave-30 cohort.

130. **Find Command/Output empty-query browser + stale** — empty query under
     Command and Output scopes stays `None` (browser) when stale ids remain
     beside a live scoped bookmark (All-scope empty-query already pinned).
     Pairs forge 165.

131. **Sticky near-miss en/em/quad-space + RLI/FSI labels** — U+2000–U+2006 /
     U+2067 / U+2068 variants of `"Save Block history"` stay toast-only beside
     rounds 129/122. Pairs forge 167.

132. **Find All-scope empty-query browser + stale** — empty query under All
     scope stays `None` (browser) when stale ids remain beside a live scoped
     bookmark (Command/Output empty+stale already pinned). Pairs forge 168.

133. **Sticky near-miss Hangul-filler/Braille/format-control labels** — U+115F /
     U+1160 / U+3164 / U+FFA0 / U+2800 / U+206A / U+206F variants of
     `"Save Block history"` stay toast-only beside rounds 131/129. Pairs forge
     170.

134. **Find Command/Output whitespace-query QueryMismatch + stale** —
     whitespace-only `" \t "` under Command and Output stays QueryMismatch when
     stale ids remain (empty-query browser already pinned). Pairs forge 171.

135. **Watch*→Guard* Full-motion None** — Full motion pins WatchCommand/Agent/
     Settled→Guard* intentional None beside WatchSettled finish Somes. Pairs
     forge 173 / core `watch_poses_never_bridge_to_repo_vigil_guards`.

136. **Watch*→ambient Full-motion None + semantic_bridges WatchSettled note** —
     Full motion pins Watch*→Explore/Sleep/Approach None; `semantic_bridges`
     comment notes WatchSettled finish six sit inside between() **93**. Pairs
     forge 174 / core `watch_poses_never_bridge_to_ambient_utility`.

137. **Sticky near-miss Ogham/MVS/CGJ/VS/Khmer labels** — U+1680 / U+180E /
     U+034F / U+FE00 / U+FE0E / U+FE0F / U+17B4 / U+17B5 variants of
     "Save Block history" stay toast-only beside rounds 133/131. Pairs forge
     175.

138. **Find All-scope whitespace-query QueryMismatch + stale** —
     whitespace-only `" \t "` under All stays QueryMismatch when stale ids
     remain (Command/Output whitespace already pinned). Pairs forge 176.

139. **Guard*→Celebrate* Full-motion None** — Full motion pins
     GuardFailure/Stuck/Recovery/Cautious→Celebrate{,Big} intentional None.
     Pairs forge 178 / core `repo_vigil_guards_never_bridge_to_celebrate`.

140. **Celebrate*→Watch* Full-motion None** — Full motion pins
     Celebrate{,Big}→WatchCommand/Agent/Settled intentional None. Pairs forge
     179 / core `celebrate_holds_never_bridge_to_watch_poses`.

141. **Hold/rest→ambient Full-motion None** — Full motion pins Celebrate{,Big}/
     Inspect/Sit/Unknown/Rest→Explore/Sleep/Approach intentional None. Pairs
     forge 180 / core `hold_and_rest_poses_never_bridge_to_ambient_utility`.

142. **Guard*→ambient Full-motion None** — Full motion pins
     GuardFailure/Stuck/Recovery/Cautious→Explore/Sleep/Approach intentional
     None. Pairs forge 181 / core `repo_vigil_guards_never_bridge_to_ambient_utility`.

143. **Watch* mode-switch Full-motion None** — Full motion pins
     WatchCommand↔WatchAgent↔WatchSettled intentional None (live SurfaceMode
     remaps). Pairs forge 182 / core `watch_pose_mode_switches_have_no_visual_transition`.

144. **CrossBlock cancel finished at wrap gen** — palette idle continuation
     drops `MAX,MAX` finished walks (no resume) beside MAX→0 schedule bump.
     Pairs forge 183 / core cancel edge.

145. **Sticky Retry continues after sync refusal** — `retry_block_history`
     re-shows on Err but does not break/return before later panes retry.
     Pairs forge 184.

146. **Sticky near-miss Mongolian FVS / mid ZWNBSP / interlinear labels** —
     U+180B–U+180D / mid U+FEFF / U+FFF9–U+FFFB variants of "Save Block history"
     stay toast-only beside rounds 137/133. Pairs forge 185.

147. **Find Command/Output/All NBSP/ZWSP-query QueryMismatch + stale** —
     NBSP / ZWSP-only queries stay QueryMismatch when stale ids remain (ASCII
     whitespace already pinned). Pairs forge 186.

148. **Idle/Rest→Guard* Full-motion None** — Full motion pins Idle/RestAfterPush→
     GuardFailure/Stuck/Recovery/Cautious intentional None (shipped beside
     hold/ambient pins). Pairs forge 188 / core
     `idle_and_rest_never_bridge_to_repo_vigil_guards`.

149. **Rest/Guard*→Watch* Full-motion None** — Full motion pins RestAfterPush/
     Guard*→WatchCommand/Agent/Settled intentional None. Pairs forge 189 / core
     `rest_and_repo_vigil_never_bridge_to_watch_poses`.

150. **Inspect/Sit/Unknown→Watch* Full-motion None** — Full motion pins
     InspectError/SitNearError/UnknownOutcome→Watch* intentional None. Pairs
     forge 190 / core `error_and_unknown_holds_never_bridge_to_watch_poses`.

151. **GuardFailure/Stuck/Cautious→holds Full-motion None** — Full motion pins
     those vigil poses→Inspect/Sit/Unknown intentional None (Recovery alone
     animates). Pairs forge 191 / core
     `failure_stuck_cautious_never_bridge_to_error_or_unknown_holds`.


152. **Sticky near-miss bidi embeddings / deprecated format labels** —
     U+202A–U+202E / U+206B–U+206E variants of "Save Block history" stay
     toast-only beside rounds 146/137. Pairs forge 192.

153. **Find WJ/figure/soft-hyphen/bidi-query QueryMismatch + stale** —
     U+2060 / U+2007 / U+00AD / U+202A / U+202E-only queries stay
     QueryMismatch under Command/Output/All when stale ids remain (NBSP/ZWSP
     already pinned). Pairs forge 194.

154. **Ambient disposition exchange Full-motion None completeness** — Full
     motion pins Idle↔Sleep/Approach, Explore↔Approach, Approach↔Sleep
     intentional None beside the partial ambient table. Pairs forge 195 /
     core `ambient_disposition_exchanges_have_no_visual_transition`.

155. **Sticky near-miss mid-range VS / Mongolian FVS4 labels** —
     U+FE01 / U+FE0D / U+180F variants of "Save Block history" stay toast-only
     beside rounds 152/146. Pairs forge 196.

156. **Find ZWNJ/ZWJ/LRM/RLM/ALM-query QueryMismatch + stale** —
     U+200C / U+200D / U+200E / U+200F / U+061C-only queries stay
     QueryMismatch under Command/Output/All when stale ids remain (WJ/figure/
     bidi already pinned). Pairs forge 198.

157. **GlanceAside↔ambient Full-motion None completeness** — Full motion pins
     GlanceAside↔Explore/Sleep/Approach intentional None beside the prior
     GlanceAside overwrite table. Pairs forge 199 / between() 93.

158. **CrossBlock cancel near-wrap bump** — palette idle continuation drops
     MAX-1→MAX generation bumps (with or without resume) and keeps a live
     resume at matching MAX. Pairs forge 200 / core cancel edge.

159. **CrossBlock cancel scheduled-ahead near-wrap** — scheduled MAX vs live
     MAX-1 cancels with or without resume (speculative gen / rewound live
     beside the MAX-1→MAX bump). Pairs forge 201 / core cancel edge.

160. **GuardFailure/Stuck/Cautious→Celebrate* Full-motion None** — Full motion
     pins those vigil poses→Celebrate{,Big} intentional None (success finishes
     via Watch*; Recovery→Celebrate already covered in round 139). Pairs forge
     202 / core `failure_stuck_cautious_never_bridge_to_celebrate_holds`.

161. **Sticky interior mid-range VS / Mongolian nirugu labels** —
     U+FE02 / U+FE0C / U+180A variants of "Save Block history" stay toast-only
     beside rounds 155/152. Pairs forge 203.

162. **Find Hangul/Braille/ideo/Ogham-query QueryMismatch + stale** —
     U+115F / U+3164 / U+2800 / U+3000 / U+1680-only queries stay
     QueryMismatch under Command/Output/All when stale ids remain (marks/WJ
     already pinned). Pairs forge 205.

163. **WatchAgent→CelebrateBig + Watch*→Unknown Full-motion None** — Full
     motion pins WatchAgent→CelebrateBig and WatchCommand/Agent/Settled→
     UnknownOutcome intentional None. Pairs forge 206 / core watch_* None pins.

164. **CrossBlock cancel finished at near-wrap gen** — palette idle continuation
     drops MAX-1,MAX-1 finished walks (no resume) beside the MAX-1→MAX bump.
     Pairs forge 207 / core `continue_idle_resume_edges_drop_stale_or_finished_walks`.

165. **Sticky closer mid-range VS / Mongolian Todo soft-hyphen labels** —
     U+FE03 / U+FE0B / U+1806 variants of "Save Block history" stay toast-only
     beside rounds 161/155. Pairs forge 208.

166. **Find FVS/MVS/VS/Khmer/CGJ-query QueryMismatch + stale** —
     U+180B / U+180E / U+FE00 / U+17B4 / U+034F-only queries stay
     QueryMismatch under Command/Output/All when stale ids remain (Hangul/
     ideo/Ogham already pinned). Pairs forge 210.

167. **CrossBlock cancel near-near-wrap bump** — palette idle continuation
     drops MAX-2→MAX-1 generation bumps (with or without resume), scheduled
     ahead MAX-1 vs MAX-2, and finished walks at MAX-2. Pairs forge 211 /
     core cancel edge.

168. **Sticky Retry skips missing TermView without aborting** — `continue`
     past panes without a Block TermView so non-Block chrome cannot starve
     later panes of `retry_history_persistence`. Pairs forge 212.

169. **Sticky Retry hides before walk / stays quiet on Ok** — optimistic
     `set_visible(false)` precedes the pane walk; only Err re-raises via
     `show_block_history_failure`. Pairs forge 213.

170. **SitNear/Celebrate tier overwrite Full-motion None** — Full motion pins
     SitNear→Inspect and Celebrate↔CelebrateBig intentional None. Pairs forge
     214 / core `sit_near_and_celebrate_tier_overwrites_stay_none`.

171. **Sticky inner mid-range VS / Mongolian syllable-boundary labels** —
     U+FE04 / U+FE0A / U+1807 variants of "Save Block history" stay toast-only
     beside rounds 165/161. Pairs forge 215.

172. **Find FE03/Todo soft-hyphen-query QueryMismatch + stale** —
     U+FE03 / U+FE0B / U+1806-only queries stay QueryMismatch under
     Command/Output/All when stale ids remain (FVS/Khmer already pinned).
     Pairs forge 217.

173. **CrossBlock cancel near-near-near-wrap bump** — palette idle continuation
     drops MAX-3→MAX-2 generation bumps (with or without resume), scheduled
     ahead MAX-2 vs MAX-3, and finished walks at MAX-3. Pairs forge 218 /
     core cancel edge.

174. **Celebrate*→UnknownOutcome Full-motion bridges** — Full motion animates
     Celebrate{,Big}→UnknownOutcome; Calm/Static snap. Pairs forge 219 / core
     `CelebrateToUnknownOutcome` / `CelebrateBigToUnknownOutcome`.

175. **Sticky nesting mid-range VS / Mongolian Manchu-comma labels** —
     U+FE05 / U+FE09 / U+1808 variants of "Save Block history" stay toast-only
     beside rounds 171/165. Pairs forge 220.

176. **Find FE04/syllable-boundary-query QueryMismatch + stale** —
     U+FE04 / U+FE0A / U+1807-only queries stay QueryMismatch under
     Command/Output/All when stale ids remain (FE03/Todo already pinned).
     Pairs forge 222.

177. **CrossBlock cancel near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-4→MAX-3 generation bumps (with or without resume),
     scheduled ahead MAX-3 vs MAX-4, and finished walks at MAX-4. Pairs forge
     223 / core cancel edge.

178. **RestAfterPush→UnknownOutcome Full-motion bridge** — Full motion animates
     RestAfterPush→UnknownOutcome; Calm/Static snap. Pairs forge 224 / core
     `RestAfterPushToUnknownOutcome` beside Celebrate*→Unknown inside between()
     93.

179. **Sticky deeper nesting mid-range VS / Mongolian Manchu full-stop labels** —
     U+FE06 / U+FE08 / U+1809 variants of "Save Block history" stay toast-only
     beside rounds 175/171. Pairs forge 225.

180. **CrossBlock cancel near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-5→MAX-4 generation bumps (with or without resume),
     scheduled ahead MAX-4 vs MAX-5, and finished walks at MAX-5. Pairs forge
     227 / core cancel edge.

181. **Celebrate/Rest→UnknownOutcome UI-bridge membership + Calm/Static snaps** —
     semantic_bridges len 93 lists Celebrate{,Big}→UnknownOutcome and
     RestAfterPush→UnknownOutcome beside core between() 93; Calm/Static snap in
     the Full-motion bridge loop. Pairs forge 228.

182. **Find FE05/Manchu-comma-query QueryMismatch + stale** —
     U+FE05 / U+FE09 / U+1808-only queries stay QueryMismatch under
     Command/Output/All when stale ids remain (FE04/syllable already pinned).
     Pairs forge 229.

183. **SitNear/Inspect→UnknownOutcome Full-motion bridges** — Full motion
     animates SitNearError/InspectError→UnknownOutcome; Calm/Static snap.
     Pairs forge 230 / core SitNear/Inspect→Unknown inside between() 93 beside
     Celebrate*/Rest→Unknown.

184. **Sticky center mid-range VS / Mongolian birga labels** —
     U+FE07 / U+1800 variants of "Save Block history" stay toast-only
     beside rounds 179/175. Pairs forge 231.

185. **CrossBlock cancel near-near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-6→MAX-5 generation bumps (with or without resume),
     scheduled ahead MAX-5 vs MAX-6, and finished walks at MAX-6. Pairs forge
     233 / core cancel edge.

186. **GuardRecovery→UnknownOutcome Full-motion bridge** — Full motion animates
     GuardRecovery→UnknownOutcome; Calm/Static snap. Pairs forge 234 / core
     `GuardRecoveryToUnknownOutcome` beside Celebrate*/Rest/SitNear/Inspect→
     Unknown inside between() 93.

187. **Find FE06/Manchu-full-stop-query QueryMismatch + stale** —
     U+FE06 / U+FE08 / U+1809-only queries stay QueryMismatch under
     Command/Output/All when stale ids remain (FE05/Manchu-comma already pinned).
     Pairs forge 235.

188. **Sticky Mongolian ellipsis label** —
     U+1801 variant of "Save Block history" stays toast-only beside FE07/birga
     (round 184). Pairs forge 236.

189. **CrossBlock cancel near-near-near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-7→MAX-6 generation bumps (with or without resume),
     scheduled ahead MAX-6 vs MAX-7, and finished walks at MAX-7. Pairs forge
     238 / core cancel edge.

190. **GuardRecovery→UnknownOutcome Full-motion UI bridge sync** — semantic_bridges
     membership + Calm/Static snaps list GuardRecovery→Unknown beside
     Celebrate*/Rest pins (dedicated Full-motion hold already at round 186).
     Pairs forge 239 / core `GuardRecoveryToUnknownOutcome` inside between() 93.

191. **Find FE07/birga-query QueryMismatch + stale** —
     U+FE07 / U+1800-only queries stay QueryMismatch under
     Command/Output/All when stale ids remain (FE06/Manchu-full-stop already pinned).
     Pairs forge 240.

192. **Sticky Mongolian comma label** —
     U+1802 variant of "Save Block history" stays toast-only beside ellipsis
     (round 188). Pairs forge 241.

193. **CrossBlock cancel near-near-near-near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-8→MAX-7 generation bumps (with or without resume),
     scheduled ahead MAX-7 vs MAX-8, and finished walks at MAX-8. Pairs forge
     243 / core cancel edge.

194. **UnknownOutcome→GuardRecovery Full-motion bridge** — Full motion animates
     UnknownOutcome→GuardRecovery; Calm/Static snap + semantic_bridges
     membership sync. Pairs forge 244 / core `UnknownOutcomeToGuardRecovery`
     reverse of GuardRecovery→Unknown inside between() 93.

195. **Find 1801/ellipsis-query QueryMismatch + stale** —
     U+1801-only queries stay QueryMismatch under Command/Output/All when stale
     ids remain (FE07/birga already pinned). Pairs forge 245.

196. **Sticky Mongolian full stop label** —
     U+1803 variant of "Save Block history" stays toast-only beside comma
     (round 192). Pairs forge 246.

197. **CrossBlock cancel near-near-near-near-near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-9→MAX-8 generation bumps (with or without resume),
     scheduled ahead MAX-8 vs MAX-9, and finished walks at MAX-9. Pairs forge
     248 / core cancel edge.

198. **UnknownOutcome→GuardCautious Full-motion bridge** — Full motion animates
     UnknownOutcome→GuardCautious; Calm/Static snap + semantic_bridges
     membership sync. Unknown↔GuardRecovery already synced in rounds 190/194.
     Pairs forge 249 / core `UnknownOutcomeToGuardCautious` inside between() 93.
199. **Find 1802/comma-query QueryMismatch + stale** —
     U+1802-only queries stay QueryMismatch under Command/Output/All when stale
     ids remain (1801/ellipsis already pinned). Pairs forge 250.

200. **Sticky Mongolian colon label** —
     U+1804 variant of "Save Block history" stays toast-only beside full stop
     (round 196). Pairs forge 251.

201. **CrossBlock cancel near-near-near-near-near-near-near-near-near-near-wrap bump** — palette idle
     continuation drops MAX-10→MAX-9 generation bumps (with or without resume),
     scheduled ahead MAX-9 vs MAX-10, and finished walks at MAX-10. Pairs forge
     253 / core cancel edge.

202. **UnknownOutcome→GuardStuck Full-motion bridge** — Full motion animates
     UnknownOutcome→GuardStuck; Calm/Static snap + semantic_bridges
     membership sync. Unknown→GuardCautious already synced in round 198.
     Pairs forge 254 / core `UnknownOutcomeToGuardStuck` inside between() 93.
203. **Find 1803/full-stop-query QueryMismatch + stale** —
     U+1803-only queries stay QueryMismatch under Command/Output/All when stale
     ids remain (1802/comma already pinned). Pairs forge 255.
