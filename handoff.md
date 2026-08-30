# Engineering handoff

Updated: 2026-08-30 (Remote destination probes are link-safe and nonblocking)

This baseline exact-pins the hardened shared core and jagent revisions and now
keeps session persistence plus Palette workflow/history reads off the GTK
thread, makes search state visible across both terminal backends, removes the
default Nerd Font dependency, and preserves raw Linux file-tree path identity.
Agent UI actions, model replies, and terminal execution events remain bound to
the session epoch; workspace snapshots enforce the same budgets while being
captured, queued, written, and restored.

## Completed since the previous handoff

- **Remote target preflight is type-stable and nonblocking (2026-08-30)**:
  `stat` now tests `-L` before `-d`, so a link to a directory remains the same
  leaf type the listing returned. FIFOs, sockets, and devices count as occupied
  `f 0` targets without being opened for a size read; remote relays therefore
  reject them before downloading a source they can never commit. The real-sh
  test covers both a directory link and a FIFO.

- **Remote creation cannot follow a dangling destination link (2026-08-30)**:
  every POSIX probe that creates, renames, copies, uploads, or extracts now
  treats `-L` as occupied alongside `-e`. This closes the concrete `mkfile`
  case where shell redirection followed a dangling link and created its target
  outside the selected directory, and makes all no-overwrite operations return
  the same exit-17 contract. A real local `sh` regression exercises all six
  operations and proves each link and outside target stay untouched.

- **Doctor no longer prints an attacker-shaped config path raw (2026-08-30)**:
  `--config` and `ANVIL_CONFIG` accept a process-local filesystem path, and the
  config check interpolated `path.display()` directly into human output and the
  JSON `detail` field. A newline, OSC sequence, or bidi override in that path
  therefore retained its formatting effect. Non-redacted diagnostics now pass
  it through `safe_inline_display` under a 2 KiB budget; support-bundle mode
  still emits the fixed `<config-file>` token. Tests pin both hostile and
  redacted paths without mutating process-global environment state.

- **`--doctor` identifies the rejected workflow (2026-08-30)**: the workflow
  check keeps the existing available/readable/refused counts and now appends
  the first rejected path plus loader reason in both human and JSON detail.
  Those are untrusted display fields — the directory writer chooses the name
  and a parser error can quote a source line — so each crosses
  `review_input::safe_inline_display` under a 256-byte budget. Support-bundle
  mode retains the counts but replaces both fields with an explicit redaction
  marker, preserving its no-local-path/no-user-content contract. Three pure
  formatting tests pin the normal, hostile-input and redacted shapes.

- **Workflows on the shared library, and the guard four terminals wrote and
  then defeated (2026-08-29)**: the headline is a defect, not a refactor. A
  workflow argument whose file declares no `default` is required —
  `render()` has always been supposed to refuse an unfilled one, anvil
  implemented that rule, and `render_reports_missing_placeholder` unit-tested
  it against an empty values map. The dialog then seeded *every* declared
  argument with `arg.default.clone().unwrap_or_default()` before calling it, so
  the key was always present, always bound, and the guard never fired once in
  production: `kill -9 {pid}` with an untouched Pid field was inserted at the
  prompt as `kill -9 `. All four terminals did the same thing, and three of them
  carried the same green, unreachable test. A guard that is only tested through
  a path no UI takes is not a guard.

  The contract is now stated once and enforced twice. `jterm_core::workflows`'s
  `render()` derives the missing set from the *values themselves* — declared,
  undefaulted, and absent-or-blank is not supplied — so a caller that pre-seeds
  cannot seed past it; and `ArgsForm` carries `Unset` versus `Supplied(String)`
  in the type system, so the dialog's rows start empty for an undefaulted
  argument without that emptiness being a value. anvil's dialog now drives
  `ArgsForm` and the existing error label finally shows
  `missing values: <names>`. The rule cuts both ways on purpose: an argument
  that *declares* a default, `default = ""` included, may still render empty,
  and emptying such a field is a deliberate empty value that does not fall back
  to the default. `scripts/workflows/docker-tail-logs.yaml` declared
  `default: ""` for its required `container`, which under the new contract is an
  explicit empty value — the one bundled example on which the headline guard
  would not have fired. That default is removed.

  The subsystem underneath it moved: `src/workflows.rs` 772 → 437 lines, of
  which ~135 are the shim body and the rest documentation plus five tests.
  That is the only file that shrank. `src/workflow_ops.rs` grew 144 → 241 and
  `src/dialogs/workflow.rs` 248 → 284, because refusal reporting and the
  `ArgsForm` binding are new code, so the honest net across the four touched
  source files is about 225 lines removed, not the 754 raw deletions the diff
  shows. anvil also stops depending on a YAML parser directly: `serde_yaml_ng`
  is gone from `Cargo.toml`, and the only YAML anvil reads now goes through the
  shared loader.
  Discovery, the bounded reader, both parsers, validation and the template
  engine are `jterm_core::workflows` (core repinned `badcce2` → `790d06a`, Cargo
  and the Nix `outputHashes` moved together). Four values are injected rather
  than hardcoded because each would silently change behaviour for two of the
  four apps: the XDG backend (`GlibDirs` — glib's lookups never fail,
  `dirs::config_dir()` returns `None` with `HOME` unset), the app segment that
  `ANVIL_WORKFLOW_DIR` is derived from, `LoadOrder::Precedence` (the palette's
  sort is stable and score-free for an empty query, so load order *is* what the
  user sees), and the source-tree tier — `env!("CARGO_MANIFEST_DIR")` resolves
  against the crate being compiled, so evaluating it in the core would point all
  four apps at `jterm_core/scripts/workflows` while their bundled-library tests
  kept passing. `search_path_spec()` spells the segment out rather than calling
  `SearchPathSpec::for_current_app`, because `identity::init` runs in `main` and
  no test binary calls it: the `Option` would be `None` and anvil's own
  search-path assertions would then be guarding nothing.

  Three divergences were live here. anvil was the copy whose reader passed
  `O_NONBLOCK | O_CLOEXEC` and **not** `O_NOFOLLOW`, so a symlink planted in
  `~/.config/anvil/workflows/` was followed, parsed, and its command became a
  palette entry that gets typed at a prompt — refused by the other three. anvil
  accepted an argument `name` that was not equal to its own trim (`"pid "`),
  which could never bind `{{pid}}` because template names are trimmed: the file
  loaded, the row appeared, the typed value was discarded, and the placeholder
  reached the prompt intact. And `find_close` took the first `}}` anywhere to
  its right, so `awk '{{print $1}' {{log}} | sort -u` rendered as
  `awk '{print $1}' access.log | sort -u` — a different, executable awk program
  — while the same template without a later placeholder round-tripped fine; the
  core counts brace depth instead, so an unmatched `{{` is unmatched regardless
  of what follows, and nested escapes still collapse.

  Gaining `O_NOFOLLOW` is a user-visible loss, because symlinking a file out of
  a dotfiles checkout is deliberate, so the refusal is not left as a log line.
  `workflows::refused_files` re-reads only the candidates that are *not* among
  the loaded workflows — the healthy case costs one directory listing per tier
  and zero extra file reads, and a name-shadowed override, which is a feature,
  never surfaces as breakage — and `workflow_ops` toasts when that set changes,
  keyed on paths and not on reasons or counts. Both halves cross
  `review_input::safe_inline_display` bounded to 256 bytes: an attacker who can
  write to a scanned directory picks the file name, and a parse error quotes the
  offending source line back verbatim, so `command = "echo <ESC>]0;title<BEL>`
  used to write that OSC sequence onto a warn line. Only a symlinked *file* is
  refused; a symlinked directory in the search path is still scanned, so
  `ln -s ~/dotfiles/workflows ~/.config/anvil/workflows` keeps working.

  `src/workflow_ops.rs` keeps the whole off-thread single-flight refresh — named
  thread, `catch_unwind`, keep-the-old-cache-on-error, the only refresh strategy
  of the four that does not stall a UI on a cold or networked home directory —
  but its hand-rolled `WorkflowRefreshState` is now a re-export of
  `jterm_core::workflows::RefreshLatch`, so the invariant is shared and the GTK
  threading is not. `src/diagnostics.rs` lost its second `toml|yaml|yml`
  predicate and its uncapped `read_dir` walk, the one place in anvil that
  ignored every bound the loader exists to enforce; it asks the loader now, and
  as a consequence a *directory* named `x.yaml` counts as an invalid file rather
  than being skipped by an `is_file()` pre-check.

  Not done, deliberately: **Insert is not disabled while `ArgsForm::missing()`
  is non-empty**, which the core's docs offer as an option. `missing()` is the
  superset of outstanding rows, not a prediction of the error — a workflow may
  declare an argument its template never references, and `render()` succeeds in
  that case — so disabling Insert would refuse a command that would have
  rendered fine. The failed render names the exact arguments in the error label
  the dialog already had, which is anvil's existing idiom.

  Environment hazard, unchanged and still worth knowing: `cargo` rewrites
  `Cargo.lock` under a temporary local `[patch]`, after which the flake's
  `outputHashes` refuses to evaluate ("A hash was specified for
  jterm_core-0.2.0, but there is no corresponding git dependency"), so
  `git show HEAD:Cargo.lock > Cargo.lock` must precede each `nix develop`.
  `Cargo.lock` carries exactly two facts: the new core rev, and
  `serde_yaml_ng` moving from anvil's dependency list to the core's.

- **Command correction on the shared engine, and three security holes it was
  hiding (2026-08-29)**: `src/command_correction.rs` went from 1,817 lines to
  843. The engine half — classification, token extraction, ranking, the safety
  gate, the prompt builder, the strict-JSON reply parser, the probe layer, the
  hand-rolled helper-trust predicate and both resolvers — contained no toolkit
  code at all, which is exactly why four terminals each grew their own copy and
  the copies then drifted apart on questions that decide whether a
  model-proposed command may be offered for execution. That half is now
  `jterm_core::command_correction` (core pinned to `badcce2`, Cargo and the Nix
  `outputHashes` moved together). What stays is anvil's presentation and
  submission channel: the inline `CommandReviewCard`, the per-pane session map
  that owns that widget's lifetime, and the relm4 plumbing between them.

  Three of the divergences were live holes here, and all three are closed.

  First, the helper-trust predicate. anvil asked
  `owner_uid == euid || mode & 0o022 != 0`, which answered "not untrusted" for a
  binary owned by a *third* user: `/opt/vendor/bin/bash`, owner `builder`, mode
  0755, placed ahead of `/usr/bin` on a shared machine was resolved by scanning
  the user's own `PATH` and then spawned automatically by any classified
  failure. Clamping the child's PATH — which anvil did, and which the CHANGELOG
  advertised as the mitigation — never helped, because the helper *is* the
  hostile binary. The same expression inverted under euid 0: every root-owned
  system binary looked untrusted, so a container or `sudo anvil` silently lost
  every APT- and PATH-verified correction. `jterm_core::helper` already answered
  both halves and only frost was using it; anvil now reaches it through
  `HelperStrategy::TrustedPathScan`, which keeps the PATH scan that non-FHS
  hosts (`nix develop` included) depend on for evidence.

  Second, the candidate safety gate. `syntax_markers` only asked whether a
  marker was *present*, and the rule was "the candidate introduces no marker the
  original lacked". Against an original that already contained a pipe, appending
  `| sh` introduces no new marker, so the candidate passed and landed pre-filled
  in an auto-focused command field. anvil had no interpreter check of any kind
  (forge had one, as four literal spellings that `|  sh`, `| /bin/sh`, `| zsh`
  and `| python3` all walked past). The shared rule splits the pipeline and
  compares the *set* of interpreters its stages run, pinned in core by a test
  against jagent's own lexer.

  Third, consent. `ai_share_command_context` does not appear anywhere in the old
  file. anvil honoured it in `task_ops`, `ai_palette_ops` and `agent_task_ui`,
  and missed it on the surface with the largest payload of the four — the failed
  command, the working directory and up to 8 KiB of terminal output, posted for
  every classified failure. The engine's payload builder now takes a
  `ConsentProof` that only a consenting policy can mint, so the fallback is
  unreachable without consent by construction rather than by a call-site check
  someone can forget to write.

  The three legitimate disagreements became construction-time policy with no
  `Default` where safety is involved, following the `BusyChatPolicy` precedent
  from the chat-store round: `LocalEvidence` (anvil's `is_flatpak()` answer,
  which used to be an early `return Vec::new()` buried inside the PATH walk),
  `ContextSharing` (read per request, not at startup, because it must be the
  value at the moment the payload would be sent), and `enabled`, which is where
  `--safe-mode` suppression and `ANVIL_COMMAND_CORRECTION_ENABLED` stay —
  `--safe-mode` means something narrower in forge and does not exist in the
  other two, so it cannot live in shared code.

  One plumbing change outside the module was unavoidable, and it is the point of
  the engine's `trusted_completion` fact. anvil gated on "an exit code is
  present", which implies a shell-reported status only by accident:
  `pending_exit_code` happens to be cleared at the two reset boundaries and
  never at finalize. A block closed by boundary inference can therefore carry
  the *previous* command's status and scrollback, and the classifier would read
  "command not found" out of the wrong output with every later step built on
  that misattribution. `CompletionProvenance` now travels the whole
  output-capable bridge — `block_view` fan-out → `terminal/block.rs` →
  `VteOutput::BlockFinished` → `AppMsg::AgentBlockFinished` → the trigger — so
  anvil states the fact instead of inferring it, and a `block_view` harness
  assertion pins that the bridge really delivers `ShellReported`. The trigger is
  now called for every completion rather than only for one with a code, so an
  untrusted completion also *dismisses* a card left over from an older command
  instead of leaving it on screen after the prompt has moved on.

  Two budget gaps closed with it. Classification used
  `review_input::validate`, whose limit is 256 KiB, so a 200 KiB pasted
  one-liner was classified, ranked, probed and prompted by a surface that
  declares a 16 KiB command; and accept used `CommandReviewCard::validated_command()`,
  the same 256 KiB path, so an oversized paste into the correction field could
  be queued to the PTY. Both are the engine's 16 KiB now, the second
  automatically: `CorrectionProposal` is the single place anvil turns entry text
  into a decision, so the card's primary label and what pressing it does come
  from one object. That also ends a smaller disagreement — a leading or trailing
  space typed into a verified proposal used to downgrade it to "Insert for
  review", because the old predicate compared the raw field text to the proposed
  command by exact equality.

  Three adversarial audits ran against the shared module *before* any app
  adopted it and found eight defects in it, including that the merged pipe rule
  was still forge's four-spelling substring match; each fix carries a regression
  test that fails when the fix is reverted. anvil's own six tests deliberately
  do not re-test the engine — they pin the wiring: that each policy is stated,
  that the label and the accept decision come from one proposal, that the
  surface budget applies to an edited draft, and that no card is raised for a
  completion the shell did not report.

  Not done, deliberately. anvil did not adopt
  `jterm_core::command_correction::CorrectionRequestState`: anvil's epoch is a
  global generation counter plus a per-pane session map, and the map removal is
  simultaneously the single-consumption step and the `remove_inline_notice`
  widget teardown; adopting `retire()` would split one step into two and change
  teardown ordering against the widget removal. The session map is documented as
  app-side wiring in the module doc, with that reason. anvil also stays on
  `LocalEvidence::Unavailable` under Flatpak rather than `Bridged`, because
  anvil ships no `flatpak-spawn` helper bridge for this surface — adopting
  `Bridged` would be inventing a capability rather than porting forge's; it is
  now a one-line change in `local_evidence()` if the bridge ever arrives, which
  is the improvement over the old buried early return. And the 16 KiB
  accept-budget test pins `live_proposal(..).accept()`, the one function the GTK
  accept path calls, rather than driving that path: `accept_command_correction`
  needs a live model, panes and a `gtk::Entry`, so a future edit that routed
  accept back through `CommandReviewCard::validated_command()` would not turn
  the test red.

  One non-reproducing SIGSEGV was observed in the anvil test binary during a
  gate run and is *not* attributed to this work: it did not recur in 32
  subsequent full runs of the same binary or in five further clean gate runs,
  the change adds no `unsafe` and no new threading, the correction tests are
  pure, and the crash appeared in a run that also had clippy running under load.
  It is treated as pre-existing flakiness in the fork/PTY-touching tests
  (`remote_fs::tests::kill_tree_reaps_the_whole_process_group`, `pty::tests::*`).
  Worth watching rather than worth a claim.

  `UPGRADE_ROUNDS.md` was not extended. Its numbering stops at 43 and the last
  32 commits, this round included, did not use it; CHANGELOG.md and this file
  are where recent rounds are recorded.

- **AI chat panel on the shared core store, behind the family consent gate
  (2026-08-29)**: anvil's 786-line private `ChatStore` is now a shim over
  `jterm_core::ai::chat_store`, constructed with `BusyChatPolicy::Refuse`
  because the panel has Archive/Delete buttons and no cancel-then-mutate step.
  Persistence goes through `snapshot_for_persistence` (it compacts before
  serialising, which is what stops a grown library from silently saving
  nothing) plus `sync_truncation_markers`, and the persistence clone uses
  `recover_retry_payload_detaching`.

  The panel now honours `ai_share_command_context`. "Include recent shell
  context" defaulted to on and `start_request` attached the last five
  `$ command (exit N)` lines to every question, so the shipped default —
  consent off — still shipped terminal evidence to the provider, while the
  Codex/agent path enforced the same flag. Consent is resolved by the same
  `agent_task_ui::prompt_policy` projection at both `AiPanelMsg::Open` call
  sites; `recent_context` re-checks it before opening the history file, and
  without consent the checkbox is off, insensitive and relabelled to name the
  config key rather than failing silently.

  Streaming no longer rebuilds the transcript per token: `push_delta` only
  appends, so `append_stream_text` splices `active_partial()[rendered..]` and
  scrolls only when the reader was already at the bottom, with at most one
  idle scroll pending. A rebuild still happens on the first fragment, a chat
  switch, or a rollback.

  Related, from the same audit: an out-of-limits `ai_conversation` used to
  invalidate the whole session envelope, so a chat library imported from a
  sibling (ember writes 4 MiB, frost 8 MiB, anvil's own budget is 1 MiB) cost
  the user their tab layout as well; it is now dropped on its own, both while
  decoding and in the post-decode audit. `[[remote_hosts]]` validation adopts
  forge's caps (64 `ssh_args`, 4 KiB per field, 256 KiB of argv) so one
  config.toml validates identically in both apps. The AppStream `<releases>`
  entry for an untagged 0.2.0 is gone, matching the policy frost states and
  frost/ember follow. The default AI-panel binding is spelled
  `Ctrl+Shift+Alt+A`, the order `Chord::display` renders.

- **Files transactional navigation and authority isolation (2026-08-29)**:
  location selection, Parent/Home, directory activation, Back/Forward,
  breadcrumbs, terminal-cwd following, and Ctrl+L absolute paths now stage a
  frozen-authority list and mutate the live tree only after the latest token
  succeeds. Failure or authority remap rejection preserves the old tree and
  selection. Per-authority success-only history is capped at 50; Ctrl+L rejects
  relative, dot-segment, oversized, control, and bidi/spoofing input, and never
  turns a lossy non-UTF-8 display into an actionable local path. Alt+Left/Right
  are keyboard-focus-scoped alongside Alt+Up/Home; pointer hover alone never
  captures a terminal Ctrl+L or Alt navigation chord. Remote Home also owns a
  navigation token while probing, so Home → Up/Ctrl+L/location deterministically
  retires the old callback before it can stage a newer-root overwrite.

  The fixed 16/128 scheduler now keys jobs by immutable authority, caps each
  remote at four running and 32 pending, and round-robins authorities within
  the existing 4:2:1 priority cycle. Queued cancellation still physically
  retires work, and safe status text exposes queue wait plus running duration.
  Typed per-authority/path failures use exponential cooldown for automatic
  expansion and TTL work; explicit Retry bypasses once. While Files is visible
  and active, activation/open plus a five-second tick revalidate a bounded root
  + expanded set. An authority-bound eight-root LRU provides an incremental
  reconciliation seed, and completed operations invalidate exact affected
  roots even after navigation. Stress/fault regressions cover remote caps,
  authority RR, stale token/authority rejection, typed backoff, bounded TTL
  planning, history isolation/cap, path validation, and cache isolation.

- **Files bounded scheduler, snapshots, and remote navigation (2026-08-29)**:
  all tree scans, interactive mutations, and bulk transfers now enter one fixed
  16-worker scheduler with a hard 128-job pending cap. Three FIFO lanes use a
  4:2:1 Interactive/Normal/Background admission cycle and background work is
  capped at four running jobs, so transfers cannot consume every listing slot.
  The status transition is explicit Queued → Loading/Refreshing → success or
  classified Error; a superseded queued listing is physically retired and
  still receives a terminal callback, while the newest eight completed errors
  remain retryable. Stress tests lock capacity, reclamation, weighted FIFO
  order, cancellation-before-start, and the absence of stranded in-flight
  state. User-visible filesystem errors are allow-listed categories and never
  repeat SSH/probe stderr, credentials, endpoints, control characters, or bidi
  text; detailed bounded diagnostics remain log-only.

  Each successful listing carries `completed_at`; the per-path snapshot index
  treats entries as stale after 30 seconds or immediately after an operation
  invalidates its exact affected directories. Expanding a loaded stale row
  refreshes only that directory. F5 now refreshes the root plus up to 63
  materialized expanded directories, while right-click Refresh targets the
  clicked directory or a clicked file's parent. Reconciliation replaces an
  exact same-path file/directory type flip, restores surviving selection and
  cursor identities, and drops only vanished identities plus drag hover.

  The Files header now separates Parent, filesystem Home, and active-terminal
  cwd. Remote Home re-probes the frozen complete backend authority and leaves
  the current last-good tree untouched on failure. Row activation enters a
  still-materialized directory as the new root; Alt+Up and Alt+Home perform the
  two authority-safe navigation actions only within the mapped Files scope and
  do not capture terminal-region keys. Pure regressions cover snapshot expiry
  and explicit invalidation, selection survivors, type flips,
  authority/generation ABA, lexical enter confinement, and keyboard scope.

- **Files remote failure recovery and scoped refresh (2026-08-29)**: the Files
  region now presents distinct Loading and in-place Refreshing states. A failed
  first expansion keeps its lazy placeholder retryable; a failed refresh keeps
  every last-good row and loaded subtree visible. Errors remain in the Files
  region with a real focusable, accessibly labelled Retry button, and the
  status tracker retains a bounded set of concurrent directory failures so
  unrelated progress or success cannot erase them. A retry is bound to the
  exact root, expansion, or refresh target and stale completions cannot replace
  its state. Same-path
  refresh tickets now cooperatively cancel their predecessor: a superseded
  worker retires before scheduler dispatch or starting its probe, and an
  already-running remote list uses the existing watchdog/process-group kill
  path. Plain F5 refreshes only when focus or the pointer is within the mapped
  Files header, status, or tree; it does not capture terminal-region or
  modified F5 input. Pure state and scope regressions plus scheduler-wait and
  real in-flight process-group cancellation coverage lock these contracts.

- **Files remote listing and reconciliation hardening (2026-08-29)**: the
  remote `list` protocol now receives the retained entry cap plus one, validates
  that positive integer in the POSIX probe, and stops enumeration on the far
  side at the hard boundary. The extra record is preserved as explicit
  `DirectoryListing::truncated` metadata while only 4096 rows are retained.
  Remote names must be valid UTF-8 before the same exact text is used to build
  an actionable path; malformed names are skipped, identical records collapse,
  and conflicting file/directory claims for one name suppress that path. The
  probe checks `-L` before `-d`, keeping symlinked directories as non-expandable
  leaves. A successful in-place reconciliation that actually changes rows now
  restores surviving selection/cursor identities, clears drag-hover state, and
  advances a separate content revision. Visible menu/drop/header intents capture
  it, so a removed row's
  delayed menu or confirmation cannot act on a replacement path; already
  dispatched filesystem settlement remains governed by its frozen backend
  authority and is not cancelled by presentation churn. Tests cover hostile
  UTF-8 and duplicate output, type collisions, production entries-plus-one
  argv, real POSIX-sh hard limiting, symlink-to-directory typing, truncation,
  and delayed-versus-dispatched intent semantics.

- **Files remote refresh ordering (2026-08-29)**: in-place directory refreshes
  now carry a latest-wins revision per absolute path in addition to the tree's
  navigation generation. A second refresh supersedes the first even when root
  and remote authority do not change, so an older SSH/Docker listing cannot
  roll the model back after a newer reply. Root and row refresh targets are
  distinct variants: a non-root `TreeRowReference` that disappears or resolves
  to another identity fails closed instead of accidentally merging that
  directory's entries into the model root. Existing merge behavior remains
  non-destructive for surviving rows, preserving loaded descendants and
  expansion; refresh errors continue to leave the last good rows visible.
  Pure regressions cover out-of-order same-path completion, one-shot
  publication, reroot cancellation, replaced identities, and vanished targets.

- **Files hidden-entry policy (2026-08-29)**: the header now has a focusable,
  accessible eye toggle. Dot-prefixed entries are hidden by default and are
  revealed by refiltering the already-loaded GTK tree, so no local/remote scan,
  navigation generation, filesystem intent, or loaded expansion state is
  disturbed. Name filtering composes with the preference, and pure policy
  coverage locks its independence from query state.

- **Process-observed SSH Files follow (2026-08-27)**: the active pane's existing
  `/proc` foreground-command probe now uses `jterm_core`'s exact-pinned
  `process::observed_ssh_command` contract
  (`1f5f0fbcfd91a084da9216392fe5ab26a5994adc`), never the generic restorable
  argv path or terminal/OSC text. The shared observer accepts direct SSH and
  the provenance-checked real `jsh-remote.sh` launcher while refusing remote
  commands, `-F`/provider loading, `ProxyCommand`, `LocalCommand`, and hidden
  launcher SSH arguments. One transport-identical configured profile wins;
  zero or ambiguous matches become a validated, non-persistent
  `FsLocation::Transient`. Its stable base target is kept separate from an
  immutable execution profile: a proven jsh ControlPath or a direct command's
  explicit `-S`/`-o ControlPath` is moved only to the latter and revalidated.
  The live observed socket takes precedence, with a saved explicit socket as
  fallback. That frozen endpoint is carried through the home probe, scans,
  file operations, clipboard, and transfers; saved/temporary views of the same
  stable namespace paste directly and choose a live endpoint from either side.
  The old tree stays intact
  during the BatchMode probe. A result commits and reveals Files only if its
  token, stable pane, exact live foreground argv, base target and execution
  overlay, frozen tree authority, managed-profile uniqueness, and user
  file-action revision remain current. Same-target follow still stages a probe
  for every changed execution overlay, then refreshes its socket and reveals
  the existing tree without losing rows/root. Failure preserves it and offers
  an exact pane/token-bound Retry;
  leaving SSH revokes pending authority but deliberately does not yank an
  already-open remote tree back to Local. An unsaved location is labeled
  `(temporary)`, never persisted, and long cloud names are middle-ellipsized
  while their complete safe endpoint remains in the selector tooltip. Its
  header terminal action opens plain
  interactive SSH through the execution overlay without an implicit remote
  jsh command. Safe mode and
  Anvil-managed remote panes skip observation. Regressions include the actual
  jsh wrapper argv, execution-only ControlPath identity, unique/ambiguous saved
  transport matching, retry gates, navigation/process/file-action ABA, labels,
  remap, and plain-terminal argv validation.

- **Block live-card SSH burst settling (2026-08-27)**: the running Block card
  now keeps its command-start height in the expanded VTE coordinate system and
  coalesces each burst of `contents-changed` notifications into one next-frame
  measurement. A long, soft-wrapped `ssh` command followed by the bridge banner,
  locale warnings, and remote prompt therefore grows the clip to the complete
  first burst without waiting for another keypress. The immediate streaming
  layout remains in place, and the extra pass is one-shot and Block-only so
  Unified and idle prompt sizing are unchanged. Pure geometry coverage plus a
  DISPLAY-backed real-VTE regression exercise the compact-to-full baseline and
  verify that an eight-row burst becomes visible without a second feed.

- **File Tree terminal entry (2026-08-27)**: the Files header now exposes a
  focusable, explicitly named terminal button. Local activation opens the
  configured shell at the exact tree root; SSH/Docker activation revalidates
  the selected managed profile and intentionally makes no remote-cwd promise,
  which its tooltip states before Enter/Space or pointer activation. Settings
  edits and config reloads now remap an active remote tree only through one
  field-for-field identical complete profile; reorder is safe, while an edit,
  removal, invalid old slot, or ambiguous duplicate falls back to Local and
  clears the old remote model before it can be redirected by index. New,
  Rename, Delete, Copy, Cut, Paste, and Refresh menu intents freeze the scan
  generation, location, and complete remote identity; a refresh, location
  switch, or profile edit before activation/confirmation now cancels visibly
  before any filesystem call. Remote-home completion checks that authority both
  before enqueue and again on message consumption, closing the A@slot0 → Local
  → B@slot0 ABA. Header terminal clicks and OS drops carry the same click-time
  authority through Relm dispatch, so a queued old root/path cannot target a
  replacement profile. Pure target,
  delayed-intent, and identity-remap regressions plus a DISPLAY-backed header
  component test cover launch authority, focusability, icon, and changing
  tooltip semantics. The file clipboard now remaps through that same exact
  profile identity while preserving a per-Copy/Cut token. Paste resolves its
  open-menu token through the live reconciled clipboard. Rename, delete, and cut
  completions retire only their captured token—even a later identical payload
  survives—and batch delete considers only paths that actually succeeded.
  Partial/all-failed batch cuts retain every unconsumed item; cancellation
  settles only the successfully moved-and-deleted prefix under the dispatch
  token. Cross-location cut cleanup retains the transfer's original validated
  host snapshot instead of resolving an old index against edited settings.
  Worker completions now settle exact clipboard state before separately gating
  all progress, success/error/cancel toasts, and refresh messages on the frozen
  tree authority plus a monotonic transfer identity, with a second gate when a
  queued refresh is consumed. Active remote-cwd following and reconnects retain
  an immutable complete configured profile apart from learned/restored session
  state: exactly one valid full-profile match may survive reorder, while a
  same-name replacement, edit, removal, or duplicate fails closed.

- **Block Search 4.4 (2026-08-26)**: the capture-phase picker key router now
  confirms only when focus belongs to the query editor or a result row. Every
  other focused widget receives `Return`/`KP_Enter` normally — including
  Refresh/Reset, scope and filter controls, row bookmark stars, and
  `AdwHeaderBar`'s implicit Close button — instead of jumping and closing on
  an unrelated selected result. Query/list confirmation and Shift+Enter
  advance semantics remain unchanged; pure routing and DISPLAY-backed GTK
  focus-classification regressions cover both sides of the allowlist.

- **Block Search 4.3 (2026-08-26)**: exact VTE occurrence jumps now roll back
  transactionally when any native step fails, removing both the search regex
  and VTE's partial selection so an unavailable target cannot leave a wrong
  match highlighted. A real DISPLAY-backed VTE regression covers two
  successful steps followed by a failed third occurrence.

- **Block Search 4.2 (2026-08-26)**: pane-local, runtime-only bookmarks now
  share one revisioned source of truth across Block cards and Unified records.
  `Bookmarked` composes with Failed, Slow, and Background before scope and the
  hit cap, including empty-query browsing. Every result exposes a star toggle;
  `Ctrl+Shift+B` toggles the selected live record, synchronizes duplicate-hit
  buttons and Block chrome, and immediately refreshes an active Bookmarked
  result set. Unified retirement prunes only the retired record IDs, while
  snapshot-budget and visual-chrome eviction leave live-record bookmarks intact.

- **Block Search 4.1 (2026-08-26)**: Cross Block Search now exposes a
  `Background` metadata condition backed by the same commandless-record
  identity in Block and Unified backends. It composes before the hit cap and
  persists with the dialog's process-lifetime intent, but deliberately matches
  no Failed/exit/duration predicate because background output has no command
  lifecycle; result rows normalize contradictory raw exit/duration fields away.
  Empty-query `All`/`Out` rows require real retained output;
  command scope and snapshot-evicted metadata create no synthetic hit. The two
  compact control rows gain automatic horizontal overflow so theme and font
  growth cannot make later controls unreachable by keyboard.

- **Block Search 4.0 (2026-08-26)**: the window capture controller now carries
  the opening toggle's physical keycode across the asynchronous action dispatch.
  Auto-repeat is consumed before it can close the newly presented GTK dialog,
  even if Ctrl/Shift is released mid-hold; physical release or window
  deactivation clears the guard. The title bar now keeps only Refresh/Reset,
  while matching and metadata controls occupy two compact content rows. Manual
  refresh paints and exposes its `Refreshing blocks…` status for one frame,
  then performs the same generation-gated, selection-preserving bounded rebuild.
  Its pending frame callback is explicitly cancelled on replacement, new intent,
  or close. The dialog slot now remains claimed through the close animation, so
  a toggle during that transition cannot open an instance that the old `closed`
  callback would later orphan.

- **Block Search metadata browsing (2026-08-26)**: Failed and Slow now work
  without a text query. Each eligible retained block contributes one
  representative row on the selected surface; both predicates run before the
  500-hit cap, and activating a filter-only row navigates without installing an
  empty VTE search pattern.

- **Block Search 3.9 (2026-08-26)**: the GTK search header now exposes a
  pointer-accessible refresh button with an accessible action name and `F5`
  shortcut. Clicking it, or pressing unmodified F5, synchronizes the automatic
  version probe and requests the same selection-preserving rebuild;
  Ctrl/Shift/Alt/Super/Hyper/Meta-modified F5 passes through unchanged. A
  press/release latch limits keyboard refresh to once per physical F5 press and
  prevents releasing a modifier mid-hold from turning auto-repeat into refresh;
  leaving the dialog focus domain resets the latch if GTK drops the release.

- **Block Search 3.3 (2026-08-26)**: Shift+Enter now keeps the GTK palette
  open, restores query focus, and advances after a successful live jump only.
  Snapshot results retain their dedicated view and unavailable results keep
  their row plus diagnostic, so continuous review never pretends it navigated.

- **Block Search 3.2 (2026-08-26)**: the GTK palette now wraps arrow
  navigation, supports Home/End plus ten-row PageUp/PageDown moves, scrolls the
  selected row into view without moving query focus, and exposes current/total
  position through an accessible status label.

- **Block Search 3.1 (2026-08-26)**: the cross-block palette now has `All /
  Cmd / Out` surface scopes plus a `Ctrl+O` cycle. Scope filtering runs before
  the 500-hit cap and composes with failed/slow predicates without changing
  the scan-to-VTE highlight contract.

- **Block Search 3.0 (2026-08-26)**: cross-block search now composes `Aa`
  case sensitivity, regex, and Unicode whole-word matching. One typed options
  value drives both the bounded record scan and the activated VTE/PCRE2
  highlighter, closing the old scan/jump interpretation split. `Ctrl+I`,
  `Ctrl+R`, and `Ctrl+W` expose all three controls without moving focus. The
  formerly unbounded dialog query now fails visibly above 8 KiB, and Rust-regex
  compilation has a 2 MiB heap ceiling before retained records are scanned.

- **Single-interpretation session restore (2026-08-25)**: every bounded legacy
  workspace, pane-layout, and versioned-envelope decode now runs through
  `jterm_core::bounded_json::validate_no_duplicate_members` before the existing
  allocation-budgeted seeds. This closes the last-wins behavior inherent in
  the hand-written `MapAccess` visitors for repeated `sid`, cwd, layout,
  payload, or supersession fields, and also rejects escaped-equivalent names
  and duplicates inside ignored future objects. The private serde_json RawValue
  sentinel is reserved so feature unification cannot reopen unchecked JSON.
  The shared preflight retains no decoded value tree and never reflects an
  untrusted member name.

- **Shared jsh session identity (2026-08-25)**: configured and persisted pane
  identities now use `jterm_core`'s exact 1..=128-byte ASCII
  `[A-Za-z0-9_-]` contract. Unicode, dotted, spaced, or otherwise
  grammar-invalid ids within the field budget safely degrade to no claimed id,
  while an over-budget field still rejects the snapshot at the bounded decoder;
  neither case can forward an invalid identity to `jsh`.

- **Core-owned Agent claim durability (2026-08-25)**: that round's exact core pin was
  `21437ba6f0cb85e74d4ce2a03ef1857de2c55d9d` (transitively jagent
  `a462ec81f3a4c6ad85a455780ced232172f127ea`). Core durably retires the public
  snapshot name before exposing `SessionClaim::Restored` and owns the later
  cleanup sync, so anvil removed its redundant post-restore sync failure gate
  and test-only injection seam. Cargo and Nix source identities move together.

- **Exact search cursor identity and core repin (2026-08-24)**: Block card
  surfaces carry render stamps through the backend/find contract, so a resize,
  fold, or output re-feed invalidates and rebuilds the retained query before
  navigation—including a one-hit pass whose logical cursor does not move.
  Cross-block rows carry their first surface occurrence and activation reaches
  it exactly or fails closed at the 4096-step bound. That round pinned
  `jterm_core` `0f47569`; the current exact pin is recorded above.

- **Exactly-once command lifecycle closure (2026-08-21)**: Block and Unified
  now share one observer-side `C -> finish` latch. An accepted `D` consumes it
  with `shell_reported` evidence; if `D` is lost, only a foreground-shell `A`
  consumes it with `boundary_inferred`/`degraded` evidence and `None` for exit
  status and duration. The inferred fan-out runs before that same `A` finalizes
  the backend record, preserving the normal `D -> A` ordering. Repeated `A`,
  background output, prompt-owned alternate screens, and RIS cannot mint a
  finish without an accepted `C`; RIS remains invalidation, not completion.
  The running-command display copy, engine-owned command identity, and Agent
  correlation remain available for prompt-trust rollback and later
  Block/Unified finalization.

- **Agent-validation hardening repin (2026-08-16, seventh round)**:
  `jterm_core` repinned to `cf0dd2c9cd369c1d8113eadde0ec6254d3fb81b1`.
  Core's pre-restore validation now enforces the stricter audit rules forge
  used to keep local (pending-must-be-final-turn,
  approved-requires-observation-or-note, turn-counter arithmetic,
  final-turn/state matching — verified against every reachable jagent live
  shape, including the AwaitingObservation normalization round-trip), and
  claimed Agent snapshots are read with `read_bounded_private`, so a
  group-readable snapshot quarantines as tampering. Also adds a block-view
  integration test pinning that a reset aborting a parser-owned OSC fires
  its barrier exactly once, and a comment documenting the capability
  observer's deliberate ESC-leniency divergence from the parser.

- **Strict-ST parser repin (2026-08-15, sixth round)**: `jterm_core` repinned
  to `73c1411f23ea41626187013133f1e2c27620ae94`, which adds the
  `AgentIntegrationReady`/`EraseScrollback`/`HardReset` ParserEvent barriers
  and unifies the parser on strict ST-only termination for APC/DCS/PM/SOS with
  abort-and-reprocess on ESC + non-ST (OSC keeps the BEL convention).
  `handle_event` gained the three arms: `EraseScrollback` →
  `backend.erase_scrollback()`, `HardReset` → `on_hard_reset()`,
  `AgentIntegrationReady` ignored (the raw `ShellCapabilityObserver` remains
  the trust authority because it advances in reset-splitter order ahead of
  dispatch). The splitter's `Reset` part no longer invokes those handlers
  itself — core emits each barrier exactly once when the part's exact bytes
  are fed, so the old direct call would have doubled every reset; the part
  survives purely as the ordering barrier that interleaves capability
  observation with the reset wipe. Behavioral change to note: an aborted OSC
  no longer fires its payload, so a malformed OSC 133 can no longer produce
  bogus PromptStart/CommandStart marks (fail-closed). Core also consumes OSC
  7771 now (never forwarded to the VTE); the observer still sees the raw
  bytes pre-parse, so capability trust is unchanged. Stale "pinned core
  predates / does not frame" comments were corrected; the APC capture path
  stays local pending a dedicated equivalence check.

- **text_safety removed (2026-08-15, fifth round)**: `jterm_core` repinned to
  `592d6632b7f51239c0d7ece7dc1796e708fab400`, whose `review_input` spoof table
  now covers `\u{fff0}..=\u{fff8}` and the full `\u{e0000}..=\u{e0fff}` tag
  plane — exactly anvil's old local superset — and which adds the bounded
  `safe_inline_display`/`safe_multiline_display` sanitizers. All
  `bounded_display_text` call sites moved to the core helpers (the one
  variable-multiline site in `agent.rs` branches locally), and
  `src/text_safety.rs` is deleted. Accepted cosmetic change: the truncation
  suffix is now a bare `…` (appended when `max_bytes >= 3`) instead of
  `… [truncated]`. `read_history_checked` stays for one policy difference
  (unsafe cwd kept and sanitized at display time); its spoof check now rides
  on the identical core table.

- **Inherited-environment freeze (2026-08-15, fourth round)**: anvil already
  consumed `jterm_core::child_env` but never wired the freeze; now
  `capture_inherited_environment()` is the first statement of `main` (every
  `set_var` — `ANVIL_CONFIG`, the input-method writes — runs strictly after;
  capture failure is fatal). The PTY spawn builds children from
  `envp_from_captured`, and executable resolution now reads `PATH` out of the
  frozen block instead of the live process environment, so resolution cannot
  diverge from the PATH the child will actually run with. The VTE spawn pairs
  `vte_envv_from_captured` with `VTE_SPAWN_NO_PARENT_ENVV_BITS` (OR'd in
  numerically — gtk-rs predates the named flag), stopping libvte from merging
  the live GTK-mutated environment back in; its failure path reports a launch
  failure in the terminal like the existing async errors. Adversarial review
  verified capture ordering on every path, the frozen-PATH semantics against
  `execvpe`, and the flag's bit value against the system libvte headers; it
  found only comment nits (fixed). Accepted test weakness, same as forge's:
  the spawn tests tolerate an `AlreadyExists` capture race, so the test
  binary's snapshot can contain another test's env mutation (no assertion
  depends on it).

- **Repin round (2026-08-15, third)**: `jterm_core` repinned to
  `04f63283090591d9ad88500224e848dbb69b1f61` (picks up `helper.rs`,
  `link.rs`, `bounded_json.rs`, `command_history::prepare_path`, and the
  upstreamed `read_recent_with_status`). The palette's stale
  "pinned core predates these inode checks" comment was corrected — twice:
  the first replacement cited the 256 KiB command cap, which core's reader
  actually enforces identically; the local `read_history_checked` in fact
  remains for `text_safety`'s wider spoof set and for keeping records with an
  unsafe cwd (sanitized at display time instead of dropped during the read).
  Deleting it in favor of core's `read_recent` plus a post-filter is viable
  but would change those two policies; deferred deliberately.

- **config_store test fixtures are umask-proof (2026-08-15, second half)**:
  the 11 tests that wrote `config.toml` fixtures with plain `fs::write` now go
  through a `write_fixture` helper that chmods `0600` after writing, so the
  full suite passes under the default `umask 002` as well as `077`. Test-only
  change; mode-matrix and permission-rejection tests that set explicit modes
  were deliberately left untouched.

- **Architecture unification round (2026-08-15)**: the last local duplicates
  of core modules are gone, and `jterm_core` is repinned to
  `1b7598de5530b7b8ca39582a77610b22987f66bc`. `src/snapshot_file.rs` and `src/atomic_file.rs`
  were deleted — organism memory now reads through the new
  `jterm_core::snapshot_file::read_bounded_private` (any owner-only mode
  accepted, group/other bits rejected on the open descriptor) and all atomic
  writes use `jterm_core::atomic_file`. `src/command_correction.rs`'s
  hand-rolled `waitid(WNOWAIT)`/group-signal probe was replaced by the newly
  public `jterm_core::supervised::SupervisedChild`; on probe/reap failure
  paths the worker detaches its output reader instead of joining it, because
  a disarmed (unsignalled) group can hold the pipe open forever.
  `src/text_safety.rs` stays: its spoof table deliberately extends core's
  with `\u{fff0}..=\u{fff8}` and the full `\u{e0000}..=\u{e0fff}` tag plane,
  and `bounded_display_text` has no core equivalent. Adversarial review of
  the round also caught the reader-join hang and a stale comment claiming
  `\u{ffa0}` was anvil-only (core already covers it). Still local by design:
  `src/notebook.rs`'s long-lived terminal children keep their own wait logic
  (`SupervisedChild` scopes itself to short-lived helpers).

- ASCII organism frontend parity now includes the five-part embodiment pass:
  visible juvenile/adult/seasoned phenotypes through a composable render
  context; content-free busy/waiting/resumed output rhythm; four-frame semantic
  bridges in Full motion; process-local repo territory habits; and a
  window/session attention arbiter with shared focus, cue-local cooldowns, and
  no replay queue. The implementation keeps Anvil's Relm4 ownership model while
  matching Forge's reducer and visible semantics. No memory schema or
  content-bearing perception was added; typing, alternate screen, and geometry
  failure still preempt every live expression.

- Workflow discovery is a startup-prewarmed, single-flight cache refresh; the
  Palette presents immediately and accepts only the matching asynchronous
  history snapshot for its current opening. Slow results from a closed or
  replaced dialog are discarded, and worker failures leave actions/workflows
  available with visible status instead of freezing GTK.
- Background persistence failures are surfaced during the session through a
  bounded, aggregated, per-operation rate limiter. The shutdown drain remains
  as the final catch for failures produced after the last UI tick.
- CLI cwd validation stops raw non-UTF-8 paths before the terminal's UTF-8
  launch boundary, eliminating U+FFFD path substitution. Config reads now
  reject group/other write bits, and automatic command-correction helpers use
  canonical non-writable system targets (Flatpak probes fail closed).
- Source installation, release installation, documentation, and uninstall now
  converge on `${PREFIX}/bin`; legacy `~/.cargo/bin/anvil` copies receive a
  migration warning only. Top-bar, file-tree, remote-host, AI, and Agent
  icon-only controls also expose explicit and state-aware AT-SPI labels.
- `src/session.rs` captures only budget-valid pane/tab fields and queues owned
  snapshots through `src/persistence.rs`. JSON work, atomic replace, both fsyncs,
  claim cleanup, and pruning run on a dedicated capacity-one coalescing lane;
  shutdown stops both lanes under one deadline and joins session persistence
  before ordinary history/organism work. Runtime growth stops before exceeding
  the 32-tab / 16-pane-per-tab / 64-pane restore envelope, and a 4 MiB aggregate
  overflow retries without optional AI state and then replay argv while keeping
  the workspace structure.
- Persistence failure suppression carries a monotonic attempt number. A success
  clears only failures no newer than itself, drained targets can report a later
  failure, and a completed old write cannot erase a newer enqueue rejection.
- Find-in-terminal has a backend-neutral status contract. Block search keeps a
  deterministic navigation sequence; VTE uses native PCRE2 as the search
  authority and a bounded
  10,000-row / 2 MiB Rust-regex mirror for counts. Bounded, PCRE2-only, or
  cross-engine regex totals are marked with `+`; switching the active pane
  clears the old backend and replays the open query. The bar exposes counts,
  regex errors, and accessible previous/next/close controls.
- File-tree model rows store versioned bounded hex for the original Unix path
  bytes. Non-UTF-8 names display as escaped bytes without collisions, notebook
  paths stay as `PathBuf`, and prompt insertion rejects paths that cannot cross
  the application's UTF-8 review boundary safely. A Flatpak notebook cell also
  fails explicitly when its raw cwd cannot cross the host bridge, rather than
  executing under a U+FFFD replacement path.
- The default font is `Monospace 14`; Block UI status/action PUA glyphs are GTK
  symbolic icons or text with accessible names. The Settings font list injects
  the active configured family when Pango does not enumerate it, preserving
  custom Nerd Font configurations during size changes.

- Completed block outcome now delegates to the pinned
  `jterm_core::block_contract` after `resolve_command_text` has combined OSC 133
  metadata with the bounded screen fallback. Renderer-owned `BlockStatus` and
  persisted records remain local, while cards, failure markers/navigation, and
  exact-exit filters share the core four-way result. Classification consumes the
  raw `Option<i32>` before the legacy i32-only history/notification/Agent
  adapters synthesize a value, so commandless output carrying a stray nonzero
  status cannot enter a failure or exact-exit result.

- `[[remote_hosts]]` gained `deploy` ("off" by default, "persist", or
  "incognito"). With it on, a remote tab runs `jterm_core::jsh_remote`'s vendored
  `jsh-remote.sh`, which places a verified static jsh on the destination for the
  life of the session and removes it afterwards — so blocks, cwd tracking, exit
  codes and the Commands timeline work on a machine nobody prepared, without
  anything being installed there, without root, and without touching the
  destination's `.bashrc`, `.profile`, or login shell. `remote_shell` is ignored
  in that mode. An unrecognised spelling rejects the host and is reported by
  config validation; it deliberately does not fall back to "off", because the
  difference between the modes is whether the destination's `$HOME` is written
  to. `build_remote_argv` splits into `build_deployed_argv` (pure, given a
  launcher path) and the publish step, so the argument order is asserted in
  tests without writing into the real cache directory, and a failure to publish
  degrades to plain ssh rather than refusing the tab.

- Every Agent UI action carries `AgentProposalRef { epoch, id }` instead of a
  transcript index. `src/agent.rs`, `src/agent_ops.rs`, `src/app_msg.rs`, and
  `src/main.rs` all route the pair, and `resolve_proposal` refuses an action
  raised against an earlier task generation — a stale click, a stale edit dialog,
  or a queued message after New Task or a session replacement. The edit dialog
  also forgets its target on close, so a Submit cannot approve a stale proposal.
- LLM callbacks capture `AgentSessionEpoch` and route it through `AgentLlmReply`.
  Both the reply handler and the post-`ask` handle writeback require the exact
  epoch, so a callback queued before New Task or session replacement cannot
  mutate the new transcript or install its handle there.
- Terminal execution identity is `AgentExecutionRef { epoch, generation }`, not
  a naked counter. The typed reference passes unchanged through
  `PendingAgentCommand`, `VteInput`/`VteOutput`, the block view's armed and active
  one-shot slots, `AppMsg`, and completion/start-failure handlers. Every accepting
  edge compares both fields; manual block completions remain the `None` path and
  still become untrusted context. Generations use `checked_add`, and exhaustion
  seals the session instead of reusing an identity.
- A raw model reply over 128 KiB is recorded as a provider failure before parsing
  and before any transcript mutation, so the oversized bytes never reach the
  parser or the transcript, and no model turn is consumed.
- `src/session.rs` decodes both the versioned envelope and the bare legacy
  `SavedSession` through `DeserializeSeed`/`Visitor` implementations that charge
  tab, per-tab pane, total pane, tree-depth, and per-field budgets while decoding.
  `SavedSession`, `SavedTab`, and `PaneLayout` no longer derive `Deserialize`.
  Unknown fields are still ignored so snapshots from other releases restore, and
  `session_within_restore_limits` remains as the post-decode semantic backstop.
- Agent snapshot restore uses `jterm_core::agent::try_claim_session_file` while
  holding the private parent lock. The public name is consumed once, invalid
  evidence is quarantined, typed claim-acquisition errors retain the public
  path, and core syncs the retired public namespace before a claimed session is
  accepted. Anvil deliberately adds no second post-restore durability gate.

## Remaining boundaries

The current session transaction protects cooperating anvil writers with locks,
revision checks, atomic replacement, and durable directory sync. As with most
Unix editors, a non-cooperating external process can still replace a watched
configuration path outside that protocol; conflict detection is advisory at
that boundary.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
bash scripts/clippy.sh   # cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
```

`scripts/clippy.sh` is the lint gate, and it is now exactly the command above:
CI runs the script, the release check runs the script, and the script no longer
carries a blanket `-A` allowlist. It used to allow seven lints, two of which
this document simultaneously called blocking — so a reviewer trusting the green
CI badge and a reviewer trusting this page were reading a gate nobody ran. Both
of those lints are fixed rather than silenced: `add_task_terminal_tab` takes a
`TaskTerminalIdentity` instead of a loose role plus session string, and
`agent_task_ui.rs` keeps its `#[cfg(test)] mod tests` last. Every sibling runs
bare `-D warnings`; anvil matches them. An unavoidable lint gets a local
`#[allow]` with a reason, never a repository-wide one.
