# Changelog

All notable changes to jterm1 are documented here. The project follows semantic
versioning for tagged releases while it remains experimental.

## Unreleased

### Added

- First-class Nix package, `nix run` app, flake check, and enriched development
  shell for the two supported Linux architectures.
- Deterministic portable Linux release archives with SHA-256 checksums and a
  user-local installer that preserves existing configuration.
- Tag-driven GitHub release automation and CI artifacts for every successful
  pull request build.
- Grouped weekly Dependabot updates for Cargo crates and GitHub Actions.
- Contribution, architecture, and private vulnerability reporting guidance.

### Changed

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
