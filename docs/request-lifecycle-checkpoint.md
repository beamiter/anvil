# Request lifecycle integration checkpoint

Consumer callbacks migrated to jagent request-bound ingestion. Preserve explicit
approval, existing UI identities and persistent snapshot formats.

Status: reconstructed after the transient workspace was lost. This revision
has NOT been compiled or tested. Do not merge until cargo fmt, cargo check
--tests and Agent regressions pass. Core integration is already published at
790c9e4194e552be425d73def5d69d3dfa09b3fe; jsh at
fb91d2c789ad3ba0e8601c4ea338708942649737. The bundled core deliberately uses
that verified integration revision, not the subsequently merged dependency PR.

Remaining: rerun consumer tests for delayed/duplicate success and failure,
cross-session callbacks, concurrent retry, restore and stream preview isolation.
Review synchronous preflight/restore failures separately from transport callbacks.
Verify current remote changes before any master publication.
