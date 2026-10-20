# Explicit native Vim IDE entry

Native IDE creation consumes a one-shot request minted only by an explicit
IDE button/menu/shortcut or an explicit `--ide` launch. Cwd, title, shell command,
process observation, bridge traffic and session restore cannot authorize entry.
Plain vim/nvim commands stay in the terminal. Actual exit revokes native session
authority; a refused quit retains the live explicit editor. Late events cannot
revive a revoked entry, and queued native paint must belong to its active entry.

## Shared dependency

Normal and test dependencies pin the public Git revision
`3385b5f92198a7b476b4940fa96978fb01bda81c` of `beamiter/jterm_core`. Its source tree is
identical to the reviewed local core `46576f94295461b143a6c476e4db46cf16c297c7`.
The published commit was created through the official GitHub connector with a
non-force, expected-head update after the ordinary Git push path failed.
No adjacent source checkout or local Cargo patch is required.

Use `cargo build --locked`; GTK frontends require their Nix development
environment (`nix develop --command cargo build --locked`). Their source vendor
hash is recorded beside the exact core revision in `flake.nix`.
