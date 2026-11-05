# Mission delivery plans

Anvil's **Missions · delivery plans** entry opens a native, on-demand view over
`jterm_core::agent_workspace::MissionWorkbench`. The shared host owns the saved
plans and evidence. Anvil owns only form drafts, widget state and navigation.
There is no additional database, scheduler, task identity or provider session.

## Workflow

1. Open Missions from the workspace sidebar. Saved plans are loaded by stable
   Mission ID. Loaded unfinished plans also enter the existing **Needs you**
   rotation; opening a plan does not start, approve, validate or publish a task.
2. Enter a title, acceptance goal and explicit `owner/repository = /absolute/path`
   worktree lines with environment `local`, then choose **Create new local plan**.
   To use another acceptance environment, edit and save the selected plan. The exact five known
   terminal-family repository names select the shared five-repository preset.
3. **Inspect saved candidates** reads repository facts and linked evidence.
   **Freeze inspected commits** explicitly records the inspected clean candidate
   set and, for the five-repository preset, the required core revision. A dirty
   or missing repository remains visible and prevents candidate adoption.
4. To link an existing hosted task run, select that task in the workspace before
   opening the Mission view, then choose **Link task <identity>**. The host
   requires its exact worktree to belong to the plan. The resulting link opens
   the existing task through normal workspace navigation, never a duplicate.
5. Edit acceptance text, required environment or repository roots and explicitly
   save. Saves carry the form's tracked revision. Only a matching save acknowledgement
   or a host binding update with unchanged acceptance fields can advance it.
   A background refresh cannot
   silently overwrite edits or grant a newer revision to an old draft. Use
   **Reload selected plan (discard edits)** to intentionally load another view's
   saved version. Changing a worktree discards its former candidate/task bindings.

The detail view separates modified, committed, pushed, integrated, validated and
pin-current facts. Remote and integration receipts include the observed repository,
ref, HEAD, candidate ancestry and observation time; historical receipts do not
restore expired current gates. Checks retain passed, failed, blocked, not-run and stale
states. A saved plan, model message or historical log is not a current pass.
Closing this view detaches the widgets; it does not cancel or restart work.

## Production dependency identity

Production builds use the exact Git revision in both `Cargo.toml` declarations
and `Cargo.lock`. `.cargo/config.toml` must not install a global sibling patch:
Cargo would also apply it to nested fixtures, and a stale sibling could silently
replace the declared production core. Nix uses the same Git source with an
explicit fixed-output hash. Flatpak's offline source list is generated from the
lockfile. Historical `packaging/jterm-core-src.tar.gz` files are not Nix or Cargo
production inputs.

For deliberate local candidate work, pass a path patch with Cargo's `--config`
option for that command only. Do not publish the resulting path-resolved lock or
claim it verifies the production pin.

## Verification and limits

- `cargo fmt --all -- --check` checks Rust formatting.
- `python3 scripts/check-production-core.py` checks agreement between the
  manifest, lock, committed configuration, Nix declaration and Flatpak source.
- `python3 scripts/test-production-core.py` rejects stale pins and global path
  overrides. It does not claim a Nix or Flatpak application build.
- `python3 scripts/test-mission-adapter.py` compiles the actual pure form adapter
  against the Cargo-resolved exact core. It tests explicit paths and candidate
  binding preservation without GTK, a provider, an IME or a live host.
- The ordinary CI still runs all-target checking, tests, Clippy, Nix, the native
  Vim suite and the isolated GTK display suite. The latter includes a narrow
  Mission-window construction test that must not dispatch work.

The cloud development executor can run the pure Rust/Python checks but lacks the
GTK/VTE system development libraries. Full app compilation and display-backed
checks therefore require their unchanged CI environments. Xvfb and injected GTK
input are not evidence for physical Wayland hardware or an actual Chinese IME.
A complete five-repository delivery additionally requires exact-version CI and
publication observations for every participating repository.
