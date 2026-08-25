# Anvil upgrade rounds

This ledger records the behavior-backed increments in the current upgrade
pass.

Rounds 1–10 record the preceding pass; this pass's additional thirty-one rounds
are numbered 11–41.

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

Verification: `bash scripts/test-install-paths.sh`, `bash -n
scripts/{install,uninstall,test-install-paths}.sh`, plus the full Cargo gates.
