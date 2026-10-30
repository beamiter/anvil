# Shared Eva native integration

Opt in in Settings → Shared Eva Companion (restart required), or set the root TOML key `eva_companion_enabled = true`. `EVA_COMPANION_MODE=shared` also explicitly opts in. Mode is frozen for this GUI lifetime; turning the setting off requires restart, not an unsafe live fallback. The profile daemon must already be running in the same local graphical session; `EVA_COMPANION_SOCKET` can identify its Unix socket. Disabled is the release default. In shared mode legacy Block and VTE bodies are disabled **even if the daemon cannot connect**. Restart without this environment variable to explicitly choose the legacy local implementation; disconnect is never a mode switch.

One `SharedHost` belongs to the window, not to its panes, replies, tasks or history. The native overlay is non-measuring and pointer-transparent. It neither writes a PTY nor alters CanvasMotion, scrollback, document heights, sidebar tabs or focus. The keyboard-accessible **Eva** menu offers Follow me / Stay here / Hide / Recall, all eight atlas skins and Full / Calm / Static motion. The menu is text only, not a second animated preview. Appearance changes go to the coordinator; the renderer uses its authoritative appearance reply. Desktop reduced motion wins over animation choice. Anvil starts gently static until the authoritative profile arrives.

The character comes from `eva-character-atlas`, using native monospace Pango glyph colors and native accessibility labels, not emoji or a screenshot of the design. The atlas marks derived animations separately from exact original sketches. Hidden/Static frames do not schedule continuous redraws. A 50 ms IPC drain remains while the shared owner is connected; it is not a body animation clock.

Input observes capture-phase keys without consuming them, dragging, VTE selection, block selection, alternate-screen/review classes, focused text entries and the open control popover. Geometry excludes visible buttons (including approvals), editors and scrollbars, with a margin; unknown/overflow geometry hides rather than guessing. Safe viewport-edge candidates are tried before a compact exact original sketch, then Hidden; no character is cloned during this selection. A fixed safe edge is used, not an output/block anchor, so no read-history offsets change. There is no automatic terminal text access. Task observations are only the selected task's opaque ID, revision and actual lifecycle receipt. Ready for review is not success; cancelled is unknown.

## Retirement evidence

A revoke immediately removes authority and hides the label. GTK `after-paint` only records a candidate cleared frame. A later check requires GDK timing **complete and presentation_time > 0**, with a serial newer than revoke, before sending `Retired`. A tick, queue_draw, after-paint callback alone, disconnect, timeout or lost window is never proof. Unknown GDK presentation timing leaves migration blocked. Capabilities advertise `NativePresented`, never compositor scanout. Ordinary window managers therefore do not provide the stronger physical freeze-proof guarantee.

## Verification boundaries

Unit tests cover all skin/pose native markup, receipt classification and presentation evidence filtering. The explicit ignored GTK fixture requires a native display and checks overlay layout/pointer behavior. Real cross-renderer transfer, SIGSTOP pixels, Wayland/X11 compositor timing, CJK preedit/IME, HiDPI, hardware rendering and physical audio require the native matrix; compilation or a source/unit test is not their acceptance. No microphone or audio stack is created by this module.

## Dialogue and memory card

The popup has its own GUI text entry, never a shell prefix. Requests attach no terminal text. Replies are bound to the exact original host/card/revision and turn; earlier replies retain their source instead of being relabeled as current. Cancel stops the reply, not an agent task. Display receipts require the dialogue widget to be mapped.

Memory defaults to the real selected task; Global requires an explicit scope choice. Project scope uses explicitly selected registered metadata or a deliberately named local-pane scope, never a guessed directory. View/next shows provenance, consent and revision. Saving/editing/deleting/clearing first proposes a change, displays its exact returned contents and digest, and requires the separate **Approve exactly the displayed change** button. A result with another review ID cannot clear a newer review. Reconnect or task-context change invalidates pending reviews. Pause does not prevent inspecting/deleting. The card reports actual daemon results, never says a failed write was remembered.

The native integration does not auto-import old local memories, automatically fetch selected text, start microphones, run shell tools or authorize model-suggested memory changes. The separate **Review selected terminal text** button reads exactly one visible local VTE selection only on that click, bounds it at 4096 UTF-8 bytes, displays source and exact text, then requires **Approve this exact text once** before it can accompany the next explicitly submitted message. Remote/ambiguous selections are refused.

## Measured native geometry and checked evidence

The body and compact Pango layouts are cached and measured with their actual native context. DPI/font context serial changes invalidate the measurement. The same measured size, including a two-pixel guard, enters safe-edge admission and a custom GTK `BodyClip` allocation/snapshot clip; the old fixed280×264 and100×72 guesses are not used in production. Any exclusion/size change publishes a new geometry revision. Incoming edits/approvals and ordinary input remain higher priority than animation.

The shared native Pango/Cairo fixture in `forge/tools/eva-native-render` actually rendered312 frames: 8 skins × 13 poses × 100/150/200% font DPI. Maximum shaped extents were200×264,320×374,440×484 pixels. All measured admission examples avoided input/approval rectangles. `forge/qa/eva-native` retains native PNGs and measurements, not reference illustrations. The shared markup unit test passed all104 skin/pose cases. These are offscreen font/draw checks, not an end-to-end window/IPC claim.

Closing a window with possible body pixels waits for withdrawal, a genuinely presented cleared frame, and the coordinator's matching durable retirement acknowledgement; existing Vim/process guards run first. No timeout is treated as proof. Missing native timing/daemon acknowledgement can therefore leave the close pending in strict mode.

## Explicit project and archived-memory management

The memory panel can discover registered task IDs, project IDs and owner-host IDs, plus persisted memory scopes (including project-only scopes), with pagination. Choose a discovered task/project or type an exact archived scope ID, request a review, then approve its displayed exact scope/digest. This is management-only access: it never adopts the archived task, changes its owner, or enables model memory retrieval. Existing View/Next, Save/Edit, Delete and Clear buttons target the approved management scope until you explicitly return to current scopes. Every mutation still needs a separate exact-change/digest approval.

For the current task, an explicitly clicked button can use its discovered registered project identity. Otherwise focus the intended **local terminal pane**, enter a named project scope ID, and explicitly assign it. Native pane identities are per-widget random UUIDs, not paths or terminal contents. No directory access or automatic memory retrieval occurs. Switching panes/tasks/projects, reconnecting, or dismissing management invalidates pending consent and memory review state. Named pane assignment is session-local and cleared on a context switch; it is not an inferred working directory. Changing an explicitly named pane project increments context revision and invalidates prior approvals. Registered task ownership/project bindings are never silently replaced. Selection approval remains separate, exact-source-bound and one-use.

Discovery metadata and offscreen tests do not establish live GUI or IPC validation. Remote panes cannot be assigned via this control.

## Reproducible dependency release

Shared coordinator/atlas source is pinned to public `beamiter/jcompanion`
commit `9aa569540091524d637a7115b64c587b9177a55b` (protocol 1).
Release manifests and Cargo.lock use immutable Git sources; Cargo acquires them
for source builds without an unversioned sibling companion checkout. Use
`cargo build --locked` and preserve existing user configuration during upgrades.
Coordinator installation and opt-in startup are documented in jcompanion's
`docs/deployment.md`; native application installation does not silently enable it.
Focused render/unit fixtures are not physical display, voice, or live API acceptance.

Shared terminal core is pinned to `beamiter/jterm_core` commit `974ccad203375032fe87706caecab39ad516c238`; normal and dev/fixture dependency identities are aligned.
Nix Git-source NAR hashes are derived from the exact published source trees; Flatpak Cargo source manifests were regenerated with the repository's checksum-pinned generator. Full Nix/Flatpak sandbox builds were not executed in this environment and are not certified by Cargo checks.
