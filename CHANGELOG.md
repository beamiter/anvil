# Changelog

All notable changes to jterm1 are documented here. The project follows semantic
versioning for tagged releases while it remains experimental.

## Unreleased

### Added

- Cross-block command/output search and a shared review-only input boundary for
  workflow, history, file-tree, notebook, remote, and AI insertions.
- TOML/YAML workflows with ordered directory precedence, metadata retention,
  Unicode-safe `{name}`/`{{name}}` substitution, and six installed examples.
- Multi-shell executable notebooks with Run All/Stop All, separate bounded
  stdout/stderr, and process-group cancellation that reaps descendants.
- Configurable Anthropic, OpenAI-compatible, and Ollama clients plus a strict
  proposal-ID Agent state machine with explicit per-command approval.
- Stable Flatpak host execution for terminal, notebook, Git, notification, and
  dependency probes; reverse-DNS desktop/AppStream identity and raster icons.
- Cargo-or-Nix source installation with prefix/data/bin overrides, `DESTDIR`,
  dry-run, safe legacy-launcher cleanup, and XDG-aware uninstall.
- `--check-config [--json]`, `--config-path`, and explicit rotating-backup
  recovery with preservation of the replaced live file.
- Process-safe configuration locking, exact revision conflict detection, two
  known-good backups, and durable atomic replacement.
- Isolated `--safe-mode` recovery sessions with VTE + `sh`, no restore or
  persistence, and network/state-producing integrations disabled.
- Machine-readable `--doctor --json` diagnostics and a privacy-preserving
  `jterm1-support-bundle` archive generator.
- A scheduled dependency vulnerability audit, ShellCheck gate, shared
  `make security` command, and repository Rust toolchain contract.
- Build provenance metadata and the exact Cargo lockfile in relocatable bundles.

- First-class Nix package, `nix run` app, flake check, and enriched development
  shell for the two supported Linux architectures.
- Deterministic relocatable Linux release archives with SHA-256 checksums and a
  user-local installer that preserves existing configuration.
- Tag-driven GitHub release automation and CI artifacts for every successful
  pull request build.
- Grouped weekly Dependabot updates for Cargo crates and GitHub Actions.
- Contribution, architecture, and private vulnerability reporting guidance.

### Changed

- Pane moves and remote reconnect callbacks now retain stable pane/session
  identity; reconnect cancellation affects only the dead leaf it belongs to.
- Safe mode ignores user config and behavior overrides, disables restore and
  persistence, and starts an isolated built-in VTE recovery pane.
- Support bundles now use redacted diagnostics and exclude host names, local
  paths, environment values, configuration/history/session contents, and keys.
- Default shortcuts now share the jterm ergonomic layout: directional Pane
  focus/resize layers, browser-style tab digits, symmetric zoom/opacity keys,
  and shell-owned `Ctrl+R` / `Ctrl+P` passthrough.
- In-app settings now refuse stale multi-window writes and invalid schemas;
  diagnostics and support bundles expose validation/lock state without values.
- Local and CI linting now share one canonical Clippy policy.
- Development commands use the committed Cargo lockfile and expose complete
  `make verify` and `make package` workflows.
- CI now validates the headless CLI, desktop entry, release bundle, checksum,
  and Nix package in addition to Rust formatting, tests, lints, docs, and the
  optimized build.

### Security

- AI command-palette responses containing newlines or terminal control
  characters are rejected before any bytes reach the live PTY.
- Potentially destructive AI-generated commands are highlighted for review.
- Detached palette requests no longer permanently leak their cancellation
  token.
