# Contributing to jterm1

jterm1 is a Rust/GTK application with two terminal backends and a deliberately
strict boundary between terminal I/O, persisted state, and Relm4 UI operations.
Small, reviewable changes are preferred over broad rewrites.

## Development environment

Nix with flakes enabled is the supported development path:

```bash
nix develop
make verify
make security
```

Useful focused commands include `make run`, `make test`, `make check`, and
`make clippy`. `make package` builds the same portable archive layout used by
the release workflow. All Cargo operations that resolve dependencies use the
committed lockfile.

## Quality gate

Before opening a pull request, run:

```bash
make verify
make security
```

This checks formatting, all Rust targets, tests, the repository Clippy policy,
and documentation. GitHub Actions additionally exercises GTK tests under Xvfb,
validates the desktop entry, smoke-tests the headless CLI, builds the portable
bundle, and builds the Nix package.

The canonical Clippy warning baseline lives in `scripts/clippy.sh`. Update that
single file only when accepting a new repository-wide lint exception; do not
add ad-hoc `allow` flags to CI or local scripts.

## Architecture guidelines

- Keep GTK work on the main thread. Filesystem, Git, process, and network work
  that can block belongs on a worker path with a bounded result returned to the
  UI.
- Preserve explicit approval before an AI agent command reaches a terminal.
- Bound terminal output retained for history, diagnostics, and AI context.
- Treat session and history files as recoverable state: write atomically and
  never let one malformed record prevent loading the remaining records.
- Add tests for parsers, shell quoting, persistence, command recall, and other
  behavior that can be checked without a visible desktop.
- Keep user configuration backward-compatible. New keys need safe defaults and
  documentation in `config.toml.example`.

## Pull requests

Explain the user-visible effect, implementation approach, and validation that
was run. Keep generated build output and local configuration out of commits.
Changes that alter packaging should exercise both `make package` and
`nix flake check`.
