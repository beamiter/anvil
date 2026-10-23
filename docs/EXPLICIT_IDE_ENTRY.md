# Explicit native Vim IDE entry

Native IDE creation consumes a one-shot request minted only by an explicit
IDE button/menu/shortcut or an explicit `--ide` launch. Cwd, title, shell command,
process observation, bridge traffic and session restore cannot authorize entry.
Plain vim/nvim commands stay in the terminal. Actual exit revokes native session
authority; a refused quit retains the live explicit editor. Late events cannot
revive a revoked entry, and queued native paint must belong to its active entry.

## Shared dependency

Normal and test dependencies pin the public Git revision
`d604e698dcb1ffa803857403b70a82429ec95f1a` of `beamiter/jterm_core`. This published revision includes
semantic navigation actions and cursor-bound request validation.
No adjacent source checkout or local Cargo patch is required.

Use `cargo build --locked`; GTK frontends require their Nix development
environment (`nix develop --command cargo build --locked`). Their source vendor
hash is recorded beside the exact core revision in `flake.nix`.
