# Smart command correction

anvil can offer a reviewable correction after a Block-mode command fails with
evidence shaped like an unknown executable or package, invalid subcommand, or
invalid option.

The feature is controlled by `command_correction_enabled` in Settings and the
Shell Agent dashboard. It defaults to `true`; the
`ANVIL_COMMAND_CORRECTION_ENABLED` environment variable can override it for one
launch. Disabling AI or correction, opening Shell Agent, closing the pane, or
finishing a newer command cancels any pending correction and rejects late
results.

## Review actions

A correction is never inserted or submitted automatically. The shared inline
review card always shows the exact editable command and Copy/Dismiss actions.

- **Insert for review** writes one validated line into an empty, idle prompt
  without pressing Enter.
- **Run verified command** is offered only when a bounded local APT/PATH probe
  verified the exact candidate and it is not recognized as dangerous.
- Editing that verified text, adding a new risk, or returning to any other text
  immediately changes the primary action to **Insert for review**. Returning
  exactly to the verified safe text restores the run action.
- Target-output hints, remote-target candidates, and AI candidates are not
  independently host-verified and therefore remain insert-only.

Prompt readiness and PTY queue admission are checked together. If the pane is
running a command, in alt-screen mode, missing shell integration, or already
contains input, anvil refuses to overwrite or submit anything.

## Resolution order

1. A suggestion printed by the failed target itself, such as Git's “most
   similar command” output.
2. For a local `apt`/`apt-get install` failure, fuzzy matching against the
   bounded `apt-cache pkgnames` result.
3. For a local command-not-found failure, fuzzy matching against bounded host
   PATH command evidence.
4. The configured AI provider, using a strict JSON `suggest`/`none` protocol.

Local probes are cancellable, output-bounded, placed in their own process
group, and share a 30-second correction deadline. Local indices are never used
as evidence for SSH, container, or other external cwd namespaces.

## AI boundary

The fallback receives byte-bounded cwd, command, exit status, failure kind,
remote flag, and head/tail terminal output as explicitly untrusted JSON fields.
It may return exactly one of:

```json
{"action":"suggest","command":"one corrected shell command","message":"brief reason"}
```

```json
{"action":"none","message":"brief reason"}
```

Unknown fields, prose, multiline/control/invisible text, unchanged commands,
oversized fields, and candidates that newly add privilege escalation, remote
execution, redirects, command substitution, pipes, or command separators are
rejected before any card is shown.
