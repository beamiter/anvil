# Smart command correction

anvil can offer a reviewable correction after a Block-mode command fails with
evidence shaped like an unknown executable or package, invalid subcommand, or
invalid option.

The engine — classification, evidence probes, the safety gate, the prompt and
the reply parser — is `jterm_core::command_correction`, shared with the rest of
the jterm family. `src/command_correction.rs` keeps only anvil's inline review
card, its per-pane session lifetime, and the policy anvil states for itself.

The feature is controlled by `command_correction_enabled` in Settings and the
Shell Agent dashboard. It defaults to `true`; the
`ANVIL_COMMAND_CORRECTION_ENABLED` environment variable can override it for one
launch. `--safe-mode` suppresses it outright. Disabling AI or correction,
opening Shell Agent, closing the pane, or finishing a newer command cancels any
pending correction and rejects late results.

A card is raised only for a command whose exit status the shell itself
reported. A block closed by boundary inference — a later prompt forced it shut
and the end mark never arrived — can attribute stale scrollback, and a previous
command's status, to the command being classified, so nothing is classified
there at all.

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

An automatic helper is resolved through the family's one trust predicate
(`jterm_core::helper`): fixed system candidates first, then the absolute
entries of `PATH`, and every path component must be system-owned — or owned by
this user and not self-writable — and not writable by group or other. A binary
owned by a third user found earlier on `PATH` is therefore never executed, and
a helper still resolves when anvil itself runs as root. Under Flatpak anvil
proves nothing locally: the failed command ran on the host while this process's
`PATH` describes the sandbox, and anvil ships no helper bridge, so only
target-output corrections remain.

## AI boundary

The fallback sends the failed command, the working directory and up to 8 KiB of
terminal output off the machine, so it runs only when
`ai_share_command_context` is on — the same consent switch Codex tasks, the AI
palette and the agent surfaces honour. With it off (the default) local verified
corrections still work and the provider is never contacted.

The fallback receives byte-bounded cwd, command, exit status, failure kind,
failure token, remote flag, and head/tail terminal output as explicitly
untrusted JSON fields. It may return exactly one of:

```json
{"action":"suggest","command":"one corrected shell command","message":"brief reason"}
```

```json
{"action":"none","message":"brief reason"}
```

Unknown fields, prose, multiline/control/invisible text, unchanged commands,
oversized fields, and candidates that newly add privilege escalation, remote
execution, redirects, command substitution, pipes, or command separators are
rejected before any card is shown. So is a candidate that hands a pipeline
stage to a shell or interpreter the original did not — appending `| sh` to a
command that already contained a pipe adds no *new* separator and would
otherwise pass every rule above.

One command line is 16 KiB, on the way in and on the way out: a longer failure
is not classified at all, and a longer draft typed or pasted into the review
field is refused rather than queued to the PTY.
