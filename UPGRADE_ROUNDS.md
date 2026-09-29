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
