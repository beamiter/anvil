# Owned GIO link-recovery boundary

This fixture exercises the actual terminal async dispatch/classification and ownership helpers against an owned portal and desktop handler. It never opens a real browser, contacts a public URL, changes real desktop defaults, or accesses a user clipboard. Passing it establishes backend behavior only; GTK prompt, accessibility, widget lifecycle and clipboard acceptance remain separate.

## Current hosted status

Native execution is disabled by a constant-false workflow step guard for both push and manual runs. Run 38983716142 on d4f84b359165638377711e726ebb3d85ceacac44 passed exact core89d locked fetch/build and all 17 Python source tests, then failed closed at UID mapping with `Operation not permitted`. No backend lane ran. The uploaded 14-file artifact SHA-256 is `71acb362b0bf9d143d28450ef046629c046964d1efd6b5dcc0f5a56289aaf9fe`.

Current CI performs source checks and the exact locked build, uploads an explicit native-SKIPPED status, and cannot establish native acceptance even when green. Re-enabling requires a separately reviewed workflow change for an explicitly authorized environment already supporting unprivileged UID mapping and network namespaces. No automatic retry, privilege escalation, security-policy change, or isolation bypass is permitted.

## Invocation and permissions

The dedicated `.github/workflows/link-recovery.yml` runs on a disposable Ubuntu 24.04 host. It uses pinned checkout/upload actions, read-only repository permission and disabled checkout credentials. Official apt packages provide GLib/GIO, PyGObject, D-Bus and util-linux; the official Rust toolchain builds the locked fixture. Dependency fetching happens before native isolation. The workflow records commit, source hashes, Rust/Cargo versions, native package versions, GLib runtime version and binary SHA-256.

For separately authorized hosted execution only:

```
python3 scripts/link-recovery/extract.py --check
python3 scripts/link-recovery/provenance.py evidence/source.json
cargo fetch --locked --manifest-path scripts/link-recovery/Cargo.toml
cargo build --offline --locked --manifest-path scripts/link-recovery/Cargo.toml
sh scripts/link-recovery/run-hosted.sh --hosted-native-approved \
  "$PWD/scripts/link-recovery/target/debug/anvil-owned-launch-boundary" \
  "$PWD/evidence/native"
```

The evidence destination must not already exist. No local bus/socket/native execution is needed to run the Python source-contract tests.

The wrapper runs `/usr/bin/env -i PATH=/usr/bin:/bin LANG=C.UTF-8 /usr/bin/unshare --user --map-root-user --net`. This requires the host to permit unprivileged user and network namespaces. Refusal is terminal: no sudo, security-setting changes, alternative route or host-network/session fallback. Every case then runs `/usr/bin/dbus-daemon --nofork --print-address=1 --config-file=<private-case>/bus.conf` with an explicit private Unix socket, no service activation directories and no inherited session configuration. Only owned portal/application services are started. The runner verifies the namespace exposes only loopback before any bus is started.

All child environments are rebuilt from an allowlist. Private HOME/XDG roots and empty system-data/config directories prevent real handler discovery; DISPLAY, WAYLAND_DISPLAY, existing D-Bus addresses, loader and Python configuration are not inherited. GIO_USE_VFS=local avoids GVfs lookup. HTTP(S) associations exist only in private recorder cases. Synthetic target bytes are retained only in private evidence artifacts and owned receiver inputs; feedback and logs use fixed descriptions.

## Source provenance

`extract.py --check` compares complete generated files byte-for-byte with the current production functions and verifies the committed source hashes. The extracted boundary comprises `launch_failure`, `launch_outcome`, and `dispatch_uri`, including the actual `launch_default_for_uri_async` call. The complete production `url_recovery.rs` is retained; the include-body copy removes only its initial inner-documentation line. `gtk::glib` is a type-identical glib re-export in the probe, without GTK initialization.

`provenance.py` additionally requires fixture and production core git/revision and resolved gio/glib/core package identities to match. A core-pin rebase must update the fixture manifest/lock and extraction explicitly, then repeat source review and relevant checks. `extract.py --write` is an explicit preparation operation; CI never refreshes stale hashes to make its own check pass. Git source remains clean before and after execution.

The probe sink adapter is not the production GTK LinkOpener. No injected Result or substitute dispatcher is used: GIO receives real private D-Bus replies/signals, and the production RecoveryOwner checks them.

## Cases

- Delayed portal Response 2: generic failure, correct original bytes, main-context timer progress.
- Response 1: silent cancellation.
- Unknown Response 77: generic failure.
- Client cancellation after actual portal receipt: wait for the late real response, remain silent.
- Replacement ownership before a late portal failure: stale callback ignored, replacement ticket survives.
- Response 0: success without recovery feedback.
- Missing handler with portal disabled: actual GIO lookup error.
- Private desktop Exec recorder: exact original URI bytes, no external handler.
- Owned D-Bus Application.Open returning delayed AccessDenied: permission-denied classification. This tests the owned application's method boundary, not failure of a daemon autostart executable.

Delayed cases require at least five 20ms main-context ticks and >=200ms elapsed. This is responsiveness evidence during pending responses, not a maximum frame-time budget.

## Deadlines, cleanup and failure artifacts

Private bus startup has a five-second monotonic deadline, nonblocking reads, child-status checks, a 4096-byte cap and exact private-address matching. Each owned service readiness wait is bounded; probe timeout is 12 seconds with an internal eight-second watchdog. Cleanup attempts every tracked service, with bounded terminate/kill/reap waits. SIGTERM/SIGINT raise a fixed interruption that reaches the active case's finally cleanup. The workflow has a 20-minute outer limit and never cancels an older run merely because a new commit arrives.

`result.json` begins failed, records each completed case and cleanup outcome, and becomes passed only after all nine cases and their cleanup finish. Exceptions record only the exception type, never arbitrary backend messages. Namespace refusal or preflight failure cannot produce success evidence. The always-run artifact step retains status, build/run logs, source/runtime/binary identities and per-case private synthetic records for seven days. Missing or failed evidence is a failure, not a skipped pass.

## Native implementation sources

Ubuntu 24.04 uses the GLib 2.80 family; record the exact installed revision rather than assuming the reference source equals the runtime. Official GLib 2.80.0 portal source (blob `083d27132fac1129b5805c50e357f05fbfb9f1e3`) was inspected for this workflow. It maps Response 1 to Cancelled and Response 2/unknown to generic Failed, retains the original URI on its non-file async branch, and may wait for a later portal response after cancellation. Its initial proxy construction remains synchronous. An async API does not guarantee zero blocking in every backend step.

GIO_USE_PORTALS enables fallback after native handler failure, not unconditional portal routing. Portal cases therefore install no handler and require real portal receipt evidence. Fresh processes avoid cached sandbox/portal choices. No Flatpak or Snap host is supported by this fixture. GLib 2.86.1 was also inspected earlier; its portal GFile transport differs, so byte-identity assertions must not be weakened if another runtime normalizes input.

- https://github.com/GNOME/glib/blob/2.80.0/gio/gopenuriportal.c
- https://github.com/GNOME/glib/blob/2.80.0/gio/gappinfo.c
- https://github.com/GNOME/glib/blob/2.80.0/gio/gportalsupport.c
- https://docs.gtk.org/gio/type_func.AppInfo.launch_default_for_uri_async.html
- https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html
