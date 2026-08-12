# Anvil / Forge frontend parity contract

Anvil and Forge are two native frontends for the same terminal product. Anvil
keeps its Relm4 component architecture; Forge keeps its direct GTK4/libadwaita
architecture. Framework APIs and internal widget ownership are intentionally
different. User-visible behavior, state transitions, settings, action names,
and recovery semantics are shared unless this document records an explicit
platform safety boundary.

## Shared acceptance surface

| Area | Required behavior in both frontends |
| --- | --- |
| Tabs and panes | New tabs, close/duplicate/reorder/mark/pin, top/sidebar tabs, drag-to-split, zoom, focus and resize actions. A split inherits the focused leaf's backend and cwd. Spawn failure leaves the old layout intact. |
| Block mode | Structured running/finished blocks, selection, search and filters, failure markers/navigation, pinning, clear/undo-clear, re-input, Markdown/JSON export, long-output private scrolling, running Stop and jump controls, and bounded virtualization. |
| VTE mode | Conventional VTE behavior, shell integration, OSC title/cwd/bell/notification handling, search, clipboard controls, and remote/container launch. |
| AI Chats | Persistent right panel, chat library and search, new/rename/archive/delete, per-chat draft and block context, streaming, Stop/Retry, strict request-to-chat ownership, width/visibility persistence, and bounded session restore. |
| Shell Agent | Approval-gated proposals and exact-command review. Automatic execution is allowed only after a private shell capability handshake and insert-then-exact-readback verification; unsupported shells, remote/host bridges, and lost identity fail closed. |
| Command correction | Editable local/AI suggestions after narrowly classified failures; no automatic acceptance and exact verification before any explicit Run action. |
| ASCII organism | Same local reducer, memory schema, motion modes, focus ownership, command/Agent lifecycle signals, visible lifetime phenotype, content-free output rhythm, semantic bridges, repo territory habits, session attention arbitration, live/sticky/inline surfaces, and shutdown flush. No command or terminal output is persisted by this feature. |
| Settings | Same groups, row order, labels, value ranges, safe-mode sensitivity, remote-host add/edit/delete flow, AI key-file handling, AI panel controls, and organism controls. Widgets remain native to each frontend. |
| Configuration | Same public keys, defaults, validation ranges, preservation rules, and atomic/revision-checked persistence. Environment prefixes remain product-specific (`ANVIL_` and `FORGE_`). |
| Actions | Same built-in shortcuts and palette actions. Direction keys also have H/J/K/L fallbacks. Historical action spellings remain accepted as aliases. |
| CLI | Same launch, diagnostics, config, safe-mode, shell-integration, and completion-generation options; generated Bash, Zsh, Fish, and PowerShell assets are shipped. |
| Sessions | Structured argv/cwd/backend/pane layout, AI conversation state, bounded decoding, atomic checkpoints, and conservative recovery from stale or malformed state. |

## Compatibility aliases

Both keybinding parsers accept these equivalent spellings while persisting the
frontend's canonical key:

- `open_ai_panel` and `toggle_ai_panel`
- `open_history_palette` and `history_palette`
- `open_workflows` and `workflows_palette`
- `open_palette` and `toggle_command_palette`

Indexed remote actions are `connect_remote_1` through `connect_remote_9` in
both frontends.

## Safety and framework boundaries

- Safe mode always starts a fresh VTE workspace, disables external or
  persistent features, and refuses remote, AI, Agent, and config writes.
- Agent auto-run capability is intentionally Linux/direct-local-shell only.
  Bash and Zsh may advertise the private capability; Fish, PowerShell,
  Flatpak host bridges, and remote sessions remain insert/review-only.
- Relm4 messages/controllers in Anvil and direct GTK4 callbacks/models in
  Forge are implementation details, not parity exceptions.

## Change checklist

Any user-visible change in one repository must be checked against every row
above in the companion repository. Pure parsing, persistence, security,
selection, and reducer logic should move to `jterm_core` when practical;
frontend repositories should retain only thin framework adapters and native
widget construction. Run each repository's complete `make verify` gate before
declaring a parity change complete.
