# Explicit native Vim IDE entry

Native IDE creation consumes a one-shot request minted only by an explicit
IDE button/menu/shortcut or an explicit `--ide` launch. Cwd, title, shell command,
process observation, bridge traffic and session restore cannot authorize entry.
Plain vim/nvim commands stay in the terminal. Actual exit revokes native session
authority; a refused quit retains the live explicit editor. Late events cannot
revive a revoked entry, and queued native paint must belong to its active entry.

## Shared dependency

Normal and test dependencies pin the public Git revision
`d3b1eb6180ea0e3bd4f6e0d1a09ef129ab3d442b` of `beamiter/jterm_core`. This published revision includes
the shared native command catalog, guarded save hooks, recent commands and
bounded workbench presentation storage.
No adjacent source checkout or local Cargo patch is required.

Use `cargo build --locked`; GTK frontends require their Nix development
environment (`nix develop --command cargo build --locked`). Their source vendor
hash is recorded beside the exact core revision in `flake.nix`.

## Vimrc integration and restored workbench state

With the matching `beamiter/vimrc` and SimpleCC runtime, the command palette
includes safe close, reopen recently closed file, and pin LSP documentation.
Close preserves Vim splits and refuses unsaved changes. Pinned documentation
opens a readonly Vim split with normal search and copy. Signature cards include
active parameter highlighting and overload position; fenced code uses syntax
highlighting in the pinned viewer.

PageUp/PageDown moves one visible result page in native quick navigation.
Successfully acknowledged palette actions are remembered; an empty palette
shows recent actions first, while text matching still takes priority.

Native Save, Save All and save-before-build use the Vimrc save hooks. A policy
explicitly enabled per project/filetype runs formatting, organize imports and
write in that order. Failed, stale or timed-out operations stop the write/build;
remote saves wait for the actual write acknowledgement. Ordinary `:w` keeps
Vim's standard behavior. See the Vimrc README for `g:vimrc_save_projects`.

Workbench presentation is restored per canonical launch project and frontend:
search query/options, supported panel visibility and dimensions, selected
output tab and available focus target. Restoration never submits a search or
build command. Vim buffers and remote reconnection remain managed by Vim's
existing session commands.

Data is stored under `$XDG_STATE_HOME/jterm/vim9`, or
`~/.local/state/jterm/vim9` when XDG_STATE_HOME is unset. State is bounded and
written atomically; corrupt files fall back to the configured defaults. Set
`JTERM_VIM9_NO_WORKSPACE_STATE=1` before launch to disable state and command
history persistence. Build command text and output are not persisted.
