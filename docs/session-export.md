# Reviewed session export

The JSON and Markdown session-export actions open a review dialog. Nothing is
written until **Save reviewed export**. Cancel or closing the review window
writes nothing, including while the CPU-only preview worker is running.

If cards are selected, or Unified has an explicitly chosen command, the dialog
starts with **Current selection**. That option captures the exact IDs when the
dialog opens; changing the terminal selection or switching panes cannot change
them. It never falls back to all records. A missing or duplicate ID stops the
entire export so a partly retained selection cannot become a misleading file.

You can also choose all completed records, comma-separated block IDs, or an
inclusive range of authoritative command-start timestamps in Unix milliseconds. Unknown times
are omitted from time-range selections. The snapshot is taken when **Prepare**
is pressed; later terminal activity is not added. Range and privacy controls
are locked for this preview. Cancel and reopen to change them.

Redacted is the default. **Export ORIGINAL text, including secrets and private
paths** explicitly opts into unfiltered text. Both choices show the same bounded
preview before Save. No data is uploaded. Saved files stay under Anvil's local
exports directory with the existing atomic, owner-only (0600) publication flow.

## Redaction limitations

This conservative heuristic suppresses entire lines containing common credential
labels (token, secret, password, authorization, API key), bearer and common token
prefixes, and common home-directory paths. Private-key PEM sections are suppressed
through their END marker or end of that text field. All directory fields are
masked. Block prompt, command, markup and output, and Unified command and retained
output, pass through this export-only filter. Short sensitive lines use asterisks
so filtering cannot increase retained text size.

This is not a secret scanner or a guarantee. Encoded, obfuscated, split, unlabeled
or unfamiliar secrets and other private paths may remain. False positives can
remove useful context. Review before sharing, especially when choosing Original.
Persisted history and live terminal contents are never changed.

## Format and bounds

Reviewed JSON is a versioned object with `export_metadata` and `records`, rather
than the legacy bare record array. Metadata records the selection, privacy mode,
included field categories, omitted record count, redacted line/directory counts,
truncated command/output counts and unavailable output count. Markdown includes
the same metadata in a fenced JSON preamble. Legacy internal export serializers
retain their existing formats.

A snapshot copies at most 4096 selected records and 16 MiB of retained text and
record overhead; oversized selections fail rather than silently truncate. Range
selection scans at most 65536 retained records and uses hashed ID membership.
The preview shows at most 64 KiB of serialized bytes plus a truncation notice.
It can end inside a JSON token or UTF-8 character (displayed with a replacement
character); this is a display-only prefix, not a parsed or complete JSON file.
The saved file contains the complete selected snapshot. Already-truncated or
unavailable retained output cannot be reconstructed by export and is explicitly
marked. No original text is added back after preview.

Formatting, filtering and file I/O run off the GTK main thread. A process-wide
single-worker admission guard is acquired before copying a preview snapshot and
held until that worker finishes, even after Cancel. Reopening while it is busy
shows a retry message without copying another snapshot or blocking GTK. Closing
a preview cancels work that has not begun; an already-running bounded CPU
calculation may finish and discard its result, but cannot publish a file. A save uses the existing asynchronous progress
and cancellation window. Once atomic publication has entered its non-cancellable
commit, Cancel displays “Finishing…” rather than claiming cancellation succeeded.
