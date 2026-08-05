# Security policy

## Supported versions

Security fixes are applied to the latest tagged release and the `master`
branch. Older experimental builds are not maintained separately; users should
upgrade before reporting behavior that may already be fixed.

## Reporting a vulnerability

Please report vulnerabilities privately through the repository's **Security**
tab using GitHub's private vulnerability reporting flow. Include:

- the affected anvil version or commit;
- operating system and desktop/session details;
- reproduction steps or a minimal proof of concept;
- expected impact, especially whether terminal input, files, credentials,
  remote sessions, clipboard data, or AI-bound context are involved.

Do not open a public issue for an unpatched vulnerability or include live
credentials, private keys, access tokens, or sensitive terminal output in a
report. Replace secrets with clearly marked test values. The installed
`anvil-support-bundle` command intentionally excludes configuration contents,
terminal history/output, environment values, and credentials; review its files
before attaching the archive.

## Scope notes

anvil executes commands with the permissions of the current user. Notebook
cells and approved AI-agent commands are not sandboxed. A report is especially
useful when anvil executes input without the documented user action, exposes
terminal data outside the configured workflow, bypasses an approval boundary,
or writes persistent state unsafely. Configuration backups can contain the same
sensitive paths and remote profiles as the live file; diagnostic and validation
commands intentionally report key names and metadata without values.
