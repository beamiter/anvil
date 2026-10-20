# A04 interaction budget: review checkpoint

Status: source/test checkpoint; no native measurements have been run.
The temporary production observer overlay, raw nonexecuting fixture and owned
application XTest collector are implemented. Normal source/release builds remain
unchanged. The collector intentionally emits raw, non-accepting evidence only:
independent pixel validation, complete GLib idle callback attribution, paired
matrix aggregation and normalized comparator evidence remain open. Root exact
review and hosted short smoke must precede any full 20-minute experiment.
Target baseline: 88c3f4c565cb5b9f8d47d22eb3ad07f47a822ae4.

## Existing evidence and reuse

- `scripts/benchmark.sh` measures build/test duration, binary size and a point-in-time
  process RSS. It cannot establish input latency or idle power.
- `pty::tests::reader_chunk_capacity_micro_benchmark` allocates fragmented 32 KiB
  buffers. `bracketed_paste_raw_observer_micro_benchmark` feeds parser bytes. Neither
  creates GTK panes or injects input. Keep both explicitly manual.
- `OwnedPty::start_reader_timed` dispatches one 32 KiB chunk at DEFAULT_IDLE, rearming eventfd.
  A timer fallback exists when eventfd is unavailable; record which transport ran.
  Timing just the consumer misses queue/watch/rearm overhead; measure the complete
  dispatch closure separately from the data-consumer callback.
- A01 `terminal::block::tests::session_export_slow_fsync_keeps_native_input_and_tabs_responsive`
  has real xdotool / VTE / kernel PTY / Notebook coverage and an owned raw PTY
  recorder. Reuse its private XDG/process cleanup pattern, not its assertion as a
  full application pane-model measurement.
- `scripts/qa-cross-selection-pointer.py` and `qa-enter-ownership.py` already use
  isolated XTest and nonexecuting input fixtures. Reuse event injection helpers.
- `scripts/test-gtk-display.sh` pins X11/cairo/software rendering and isolates each
  process/display/session bus. Preserve that isolation; do not add a 20-minute
  benchmark to every ordinary GTK regression run.
- A03 capability reasons and A05 fail-closed history/recovery tests are mandatory
  correctness guardrails. Do not disable them to obtain faster numbers.

## Native collector design for next reviewed change

Use the actual application pane tree and production Block terminal adapter, not a
Grid of standalone VTE widgets. Traditional `terminal/vte.rs::spawn_shell` uses
libvte `spawn_async` and bypasses OwnedPty entirely; it cannot qualify this item.
Start 1, 8 and 32 total panes with a fixed 1600x1000 window, fixed font family/size,
scale 1, X11, cairo, fixed theme, and empty XDG. The production limit is 16 panes
per persisted tab (session.rs); do not disable persistence or raise that limit.
The 32-pane scenario uses two real 16-pane tabs, 16 visible and 16 background,
with fixed-schedule tab alternation. The 1/8 scenarios use one visible tab.
At 16 visible panes use a 4x4 tree (roughly 400x250 pixels before chrome).
Assert every currently visible pane retains at least 16 columns by 6 rows and a
visible marker rectangle; fail setup rather than silently hiding panes. Record
allocations, tab identity and map state for all panes. Chat or VTE controls need
their own separately named scenario and workload hash. Fixture child is a nonexecuting raw PTY program:
it emits deterministic plain-text lines at a fixed rate per pane, reads input,
and echoes sequence-numbered synthetic markers. No shell commands, clipboard,
agent tokens, trust override, history restoration, or real user data are needed.

Run 5 seconds warmup then 60 seconds sustained output per pane count, at 256 KiB/s
per pane (32 panes = 8 MiB/s aggregate). Output and interaction schedules are
fixed-seed, wall-clock paced, and independent of response completion to avoid
coordinated omission. 300 distinct key events and, for 8/32 panes, 300 pointer-move events per
scenario give meaningful but noisy nearest-rank p99s. Keep all bounded samples,
missed deadlines, lost/duplicate markers and achieved byte counts. Do not silently
exclude timed-out or dropped events. Sample IDs must prove exactly-once echo.

Keys traverse XTest -> GDK -> production adapter -> PTY -> fixture echo -> parser
and renderer. Record injection monotonic time, GTK key receipt, PTY receipt,
marker observation, and first frame after marker observation. A frame-clock
`after-paint` observation is a presentation proxy, not proof of physical scanout.
Validate a subset with X11 screen captures of deterministic marker pixels; if
that validation is unavailable, label the metric `input_to_after_paint`, not
`input_to_display`, and leave display acceptance open. The comparator below requires successful pixel subset validation but still names
all frame-clock samples `input_to_after_paint`. A passing proxy gate does not
close input-to-display acceptance. Capture markers on 10 predetermined events
in a separate validation replay, excluding that replay from latency samples.
For one pane there is no divider: record validated `single_pane_no_divider` N/A
and zero drag samples; do not synthesize a divider or duplicate key samples.
Pointer workload drags a real application divider using XTest button/motion events;
verify pane allocations change, input focus stays in the intended pane and PTY
winsize follows allocation. Measure injection-to-first-rendered-geometry using the
same pixel validation; a synthetic signal emission does not qualify.

Instrumentation is opt-in/test-only, bounded preallocated vectors, numeric data
only. Never log PTY bytes or commands. Time full PTY watch callbacks and consumer
callbacks with monotonic time; store samples outside the measured closure after
capturing its end. Track count, maximum, p50/p95/p99 and >16ms count. The 16ms
threshold is a target, reported as a separate failure from relative latency.
Retain source IDs for eventfd/timer/animation callbacks; do not add periodic GTK
sampling callbacks merely to measure idle. Verify the probe-disabled build has
identical scheduling and trust/input behavior.

For each fresh idle process, repeat the warmup and active output phase, then stop
output and wait 10 seconds to drain. Run a full 1,200-second idle window
for EACH pane count in its own fresh process. External Linux /proc sampler at
1 Hz reads VmRSS, process + every thread CPU ticks/context-switch deltas, and
probe counters. Context switches are NOT wakeups: report separately. Attribute
actual callbacks and timer rearms using probes; idle acceptance is zero recurring
PTY empty polls on eventfd transport, no permanently-ready source, and no busy
polling. Existing legitimate UI timers must be identified, counted, and reported
rather than silently ignored. Timer fallback is a separately named degraded
scenario; never claim its polling behavior is the normal eventfd path.

## Pairing and gates

Build exact baseline and candidate in identical release configuration, applying
the same reviewed test-only instrumentation overlay to both (record overlay SHA).
Partition into three hosted jobs, one per pane count. Each job runs
baseline/candidate/baseline/candidate sequentially on its own single runner
allocation and boot (four complete 20-minute idle windows plus active phases,
allow 120 minutes excluding build). Never compare across jobs/machines. No
parallel workloads within a job. Pair corresponding repetitions; retain raw
results and per-pair gates rather than combining samples across runs. A baseline
instrumented source is not an untouched baseline binary; report both SHAs.

Record machine/boot ID, CPU model/count, kernel, renderer, backend, font digest,
scale, geometry, GTK/VTE/GLib versions, build profile, compiler, dependency lock
SHA, fixture/instrumentation SHA and workload SHA. Record binary SHA and source
SHA separately. Record measured achieved bytes, expected/observed unique key
and drag events, idle duration, monotonic timestamps and all bounded samples.

Reject missing phases, mismatched environment/workload/surface/topology, zero baseline latency,
nonfinite/negative measurements, lost/duplicate events, or incomplete 20-minute
idle windows. Compare same-machine p95 input-to-after-paint <= baseline * 1.15 for
all pane counts as a provisional proxy gate; true display acceptance stays open. Report p50/p95/p99, callback >16ms, RSS and actual callback wakes.
RSS/wake relative deltas are diagnostics until a reviewed numerical budget is
established. Zero recurring idle PTY polls is a hard acceptance condition.
A fast synthetic record only validates the comparator, never native acceptance.

## Local limits and stopping point

Local GTK cannot link and AF_UNIX socket construction is denied. Do not start a
local display/session bus or route around that restriction. Stdlib Python contract tests and Rust all-target metadata checks run here;
DOCS_RS=1 bypasses native discovery/link. The shared target is used only with
exclusive coordination, jobs=1 and incremental=0. Hosted
native execution requires root exact-file review and publication approval.
A04 remains incomplete until the raw collector has passed native smoke and the
pixel oracle, complete idle attribution/windows and reviewed paired measurements
exist. No raw JSON flag is promoted into acceptance by inference.

## Output saturation and backpressure

The fixture tracks scheduled bytes, emitted bytes, blocked-write time and maximum
write lag. The collector tracks consumed bytes and queue high-water marks.
Latency acceptance requires emitted and consumed bytes within 1% of scheduled
bytes and no backlog left after drain. If 32 panes cannot sustain the fixed rate,
report saturation as its own failed scenario, retain loss/lag metrics, and do not
quietly lower the load or count only completed keys. A second explicitly named
load sweep can identify a sustainable rate but cannot replace the fixed case.

## Native integration map and unresolved proof obligations

- Build panes through `main.rs::create_pane` and
  `workspace_ops.rs::split_active` / `build_pane_layout`, awaiting each real spawn
  before the next split. Do not mount a test-only widget tree or bypass caps.
- Production Block path is `TermView::new` -> `new_with_spawner` ->
  `OwnedPty::spawn_with_shell_token`; `ReaderCtx::install` calls
  `OwnedPty::start_reader`, then `start_reader_timed`, then
  `ReaderCtx::process_watch_input`. Require positive counters for this path and
  zero VTE spawn substitution counters. Sample actual full dispatch and consumer
  entry/exit; never substitute a test loop around parser.feed.
- A raw fixture can be supplied as the configured argv; existing
  `new_with_spawner`/from_openpty tests show raw PTY construction is possible,
  but that is NOT runtime proof of the full-app raw output path. The first hosted
  smoke must prove raw output and keystroke echo render through the actual Block
  active surface without OSC133 or any trust modification. If it does not, stop
  and review a private synthetic Bash fixture with real integration. Never mark
  a synthetic OSC133 marker trusted or relax token/foreground/input guards.
- The observer must tag callbacks by production versus instrumentation origin.
  External /proc sampling is an observer CPU/wake cost, not an application wake.
  Active phase frame probes must be detached during idle. No observer GLib timer
  may survive into the idle window. Preserve all production UI timers.
- The implemented raw collector provides owned-display XTest checks, bounded PTY
  output, input IDs, resource sampling and error artifacts. It has NOT run natively;
  a hosted smoke must establish both raw render and owned-child cleanup behavior.

The originally considered 32-simultaneously-visible-pane scenario is infeasible
under the normal persisted per-tab limit. This checkpoint preserves that coverage
gap rather than silently changing the production limit. The supported 32-total
scenario alternates tabs every 10 seconds during active measurement; injection
IDs identify target tab/pane and no key is injected during an unresolved switch.
Switch deadlines are still recorded, so a delayed switch does not erase waiting
time. Idle has no scheduled tab switching. Both paired runs use identical topology.

## Comparison CLI

Run `python3 scripts/test-interaction-budget.py` for synthetic contract checks.
Run `python3 scripts/compare-interaction-budget.py baseline.json candidate.json`
for one paired native scenario. Exit 0 means only the declared provisional proxy
gates passed, exit 1 means a budget failure, and exit 2 means invalid/incomparable
evidence. The JSON carries `presentation_proxy_only_display_acceptance_open` even
on success. Every 1/8/32 job must provide both paired repetitions; this single-pair
CLI does not claim whole-matrix completeness or assert a source is trustworthy.

The comparator environment identity fields are nonempty, non-whitespace strings,
except `scale`, which is a finite positive JSON number (integer or float, never a
boolean). Digests retain their exact hexadecimal length checks. Text identities
are compared exactly without trimming or coercion; equivalent integer/float scale
values such as `1` and `1.0` are compatible. Equal malformed identities do not
establish a comparable pair. This validation does not change measurement windows,
sample-count bounds, or any performance gate.

## Implemented source checkpoint and exact boundaries

`prepare-interaction-budget.py SOURCE NEW_TREE` applies the same five-file
`interaction-budget-overlay.patch` to a new disposable copy and copies the Rust
probe module. It never modifies SOURCE. The normal main.rs does not declare the
module, Cargo.toml is unchanged and no Cargo feature exists, so even ordinary
`--all-features` builds exclude the probe and its signal control surface. Record
the generated overlay digest for both builds. The overlay only wires observer
entry/exit and a benchmark-only signal message into existing paths; it does not
replace rendering, PTY I/O, parsing, input, trust or pane-limit logic.

The instrumented process requires an absolute owned mode-0700 directory. Its
SIGUSR1 callback reads a <=4096-byte O_NOFOLLOW JSON command with a monotonically
increasing request ID. Commands are a strict enum: snapshot, setup split/new tab,
select tab, begin active, begin idle, finish. There is no command text, PTY input,
path, token, or arbitrary execution field. Split/new tab are restricted to setup
and call the real production model methods. A ready file prevents premature
signals. Replies contain numeric identities/geometry/timing, not terminal text.

The raw collector injects lowercase a through XTest into the selected Block pane
and moves an actual divider. Its fixed schedule retains injection lateness;
subsequent samples are not rescheduled around slow responses. Per-child sequence
markers are parsed in ReaderCtx and checked in bounded visible VTE text at
after-paint. The text is never retained. This checks a stronger path than byte
receipt but is still NOT a pixel/display-latency oracle. Geometry-change timestamps
are raw observations, not yet accepted drag sample mappings.

Frame observers are explicitly disconnected at idle and do not request frames.
The probe counts PTY dispatches/consumers and AppModel updates separately; it does
not claim that these exhaust GLib/GTK timer sources. The /proc sampler records
CPU ticks, RSS and per-thread context switches, never calls those kernel wakes.
A raw run cannot set `idle_source_audit_passed` merely because these counters look
quiet. This limitation intentionally prevents comparison acceptance.

The only supported first hosted invocation is the short explicit `--smoke` mode
of `run-interaction-budget.sh INSTRUMENTED_BINARY 1 OUTPUT --smoke`. It still warms
for five seconds, injects three keys, drains ten seconds and observes a short idle
period. Full mode requires explicit `--full-budget` and separate root review after
the raw Block/pixel correctness smoke. No hosted workflow has been added here.

## Cleanup review successor

The collector owns a dedicated child-subreaper before application launch. It
records descendant PID, original parent and /proc start ticks, pins each discovered
identity with a Linux pidfd and signals only those handles. Existing collector
children are excluded. Adopted early-exit descendants remain discoverable, even
if the application dies before the first ready file or pane snapshot. No process
name, output-supplied PID alone or reusable numeric PID is a signal authority.

Finalization has independent exception-isolated lanes: harvest reports; terminate,
escalate and reap the owned subtree; independently terminate/wait the Popen app;
harvest late fixture reports; close display/log/ownership handles; then atomically
publish evidence. Output or XCloseDisplay failure cannot skip process cleanup.
All known identities must be exited and reaped, and discovery must be empty,
before raw_capture_completed can become true. Failed verification reports the
remaining identities/errors and never marks capture successful.

Keys, drags, resource samples and control snapshots are attached to the evidence
object as they accumulate. Fixture reports and the last control response are
harvested before the temporary directory is removed, including on early failure.
If the requested evidence destination fails, a unique mode-0600 sibling fallback
file is attempted; if even that fails, the complete failure report goes to stderr.
No successful JSON is published until cleanup is verified. The report records
cleanup, harvest and evidence-write failures separately.

Local successor tests include a real owned Linux PTY descendant that ignores
SIGTERM: cleanup escalates through its pidfd, reaps it and proves the identity
absent despite simultaneous display-close/output failures. These tests use no
AF_UNIX, GTK or display and establish teardown only, not native GUI performance.

## Catchable collector cancellation

The dedicated collector installs SIGTERM and SIGINT handlers before launching any
owned process. Handlers only record bounded numeric signal metadata; they do not
throw asynchronously inside process creation, cleanup or evidence publication.
Readiness and workload checkpoints raise a cancellation error, with sleeps
checking at most every 50 ms. Setup xdotool commands have a five-second timeout.
Finalization disables cancellation exceptions while retaining the record-only
handlers, so repeated signals cannot interrupt the owned-tree cleanup lanes.
Cancellation always invalidates raw_capture_completed, including cancellation
first recorded during finalization. Previous handlers are restored afterward.

Dedicated-process negative tests send SIGTERM and SIGINT to the actual collector
while it waits for readiness from a generated test application with a real owned
PTY descendant. Both application and descendant ignore termination; repeated
cancellation during cleanup still reaches pidfd escalation, verified empty
ownership and failed JSON evidence. A separate outside sentinel remains alive and
is absent from recorded ownership. These tests deliberately never open a display
or AF_UNIX socket; they prove catchable cancellation teardown, not GTK execution.

This is cooperative catchable cancellation, not an external supervisor guarantee.
An outermost collector SIGKILL, host death, unavailable filesystem or indefinitely
blocked native call can prevent Python finalization and/or evidence persistence.
No SIGKILL cleanup claim is made. CI must allow the bounded cleanup grace period
before forcibly killing the collector; hosted smoke remains subject to review.

## Historical raw-smoke checkpoint (published and passed)

The narrowly scoped path-filtered push workflow builds only disposable observed
binaries. Baseline is pinned to 88c3f4c565cb5b9f8d47d22eb3ad07f47a822ae4;
candidate is the immutable event SHA, with its parent required to equal reviewed
A04 publication head 5ccfdffc4e37ed2cb45912660152a17daeb2eceb. A later changed parent fails
closed and requires another reviewed pin. Both trees receive the byte-identical
observer overlay (digest cb8b833a01ba814fd88c8e2abda856d997a71e9abbcc8af6c90e3cdea83ff633).
All published A06, A07 and A08 production changes are retained. No normal/release build sources, release
workflow, installed artifact or published binary are replaced by this smoke job.

One ubuntu-24.04 runner builds both observed binaries with one Rust installation,
then runs baseline/candidate pairs for 1, 8 and 32-total panes. Each run uses a
fresh private display/session, three key events, two active seconds, five-second
warmup, ten-second drain and three-second idle. The smoke-only tab period is
0.5 seconds, recorded explicitly; the full workload retains ten seconds. Key
records retain the scheduled/observed tab ID, target pane PID and confirming
snapshot request/time. The smoke gate checks actual active-tab snapshots and
painted marker identities and requires both tabs to have received keys for 32.
Thirty-two panes still means 16 visible and 16 background, never 32 visible.
One-pane drag remains validated N/A; other drag injections do not prove geometry.

Six short runs establish only raw after-paint echo proxy, supported setup and
verified owned-process teardown. They cannot satisfy performance or independent
pixel acceptance. Separate pixel/display latency, complete idle-source attribution,
drag event-to-geometry and full paired-matrix gates remain explicitly open in the
summary. A failed scenario stops the smoke sequence; partial raw evidence, logs,
source/overlay/binary identities and failed summary are uploaded when available.
No full-duration invocation appears in the workflow or smoke runner. No long
measurement is authorized until the short correctness smoke and independent pixel
proof have been reviewed. CI forced-kill/host-loss limitations above still apply.

The smoke gate additionally requires ordered unique event IDs 0–2, contiguous
per-child sequences starting at one, ordered injection times and chronological
unique control snapshots. Every key must have exactly one matching paint marker
at or after its injection; duplicate, missing, unexpected and pre-injection
markers are rejected. Workload tab IDs must equal the original topology, each
key's pane must belong to that original tab, and observed key targets must cover
every topology tab. Two different panes in the same tab cannot prove two-tab
coverage. Scheduling uses int(elapsed_seconds / interval_seconds) modulo the tab
count, preserving fractional smoke time while returning an integer index.

That publication parent pin was rebased to the reviewed head above, preserving
all published A07/A08 production changes. The baseline remains the historical
88c3 commit intentionally: this short smoke checks that the same observer and
raw echo/cleanup contract work on both revisions, not that differences are caused
by A04 or that either revision meets performance budgets. The candidate remains
the exact push-event commit, with an exact-parent guard. A different master head
requires another pin review rather than replaying an obsolete tree operation.


### First hosted failure and harness repair

Run 38969356891 failed before any keys: production treats config.shell as one
executable path, so the former command string triggered normal-shell fallback.
The fixture identity guard refused that child, and the saved report verified an
empty owned tree after cleanup. No echo/performance success was inferred. The
build artifact also showed identical baseline/candidate hashes after a 0.09-second
second build; that pair is invalid and retained as failure evidence.

The corrected launcher is an atomically created private mode-0700 executable,
with a fixed Python interpreter and literal quoted argv. It execs the fixture,
preserving child PID. A temporary observer boolean checks actual AppModel
shell_argv, after the production config parser and program resolver, equals the
exact fixture executable. A false result aborts before workload signals. Local
launcher tests validate TOML path round-trip and exec PID identity; actual Rust
config/parser success remains a mandatory hosted gate, not a local test claim.

Each revision now compiles into its own absent target directory. Cargo JSON must
prove a non-fresh artifact from the expected manifest; a rustc wrapper records
and verifies the actual anvil source path/digest, compiler, argument digest and
times. Build records retain source manifests, overlay identity and output
hash/size/mtime. Source mutation, reused artifacts and identical paired binaries
fail closed. Both observed binaries and build records are kept in the smoke
artifact, separate from ordinary release artifacts. Download caches may be shared;
compiled target directories may not. All prior acceptance gaps remain open.

The build-input record starts from verified git ls-tree entries of the exact
checkout commit/tree, covering embedded assets, build scripts and Cargo files.
Only the explicit temporary overlay delta is permitted; every prepared file is
hashed before and after compilation. Compiler/flag overrides are rejected,
PYTHON* inherited variables are cleared for children, and the fixed launcher and
compiler-wrapper Python processes use isolated mode. This records source inputs;
it does not claim a hermetic toolchain or operating-system image.


## Short independent virtual-pixel and drag gate (source checkpoint)

The next workflow uses the same historical baseline and exact event-SHA candidate,
with reviewed source parent 9013f05f4ec519c940d25935772db007fdfa8f32. The separately
reviewed Bash locale production fix, its regression test and ordinary CI step
are preserved byte-for-byte. The fixed historical baseline remains intentional:
these short gates do not attribute performance changes to A04. A changed master
requires a new pin review. Identical temporary instrumentation is applied to
both fresh independent builds; production sources and normal release binaries
still contain no benchmark control surface. The current combined overlay digest
is eac561f4c56d15c6038ceee148c0bb2bba24280b95c7d8144f9d288c8dffd26b. The historical digest above describes the already-passed raw
smoke, not this successor. No hosted result is implied by this source checkpoint.

Twelve short runs pair observe/control for each baseline/candidate and 1/8/32-total
scenario on one runner, backend, font and fixed 1600x1000 screen. Observe/control
order is reversed for eight panes. Both modes emit identical ANSI barcodes and
perform actual allocation checks. The raw echo, exact fixture program, per-event
identity, both-tab coverage, source/build provenance and owned-tree cleanup gates
remain prerequisites. A single pair exposes gross observer cost but is not a
statistical regression result or an unbiased estimate of population perturbation.
The full 1200-second matrix is neither invoked nor authorized here.

Each fixture emits a 31-cell single-row background-color barcode carrying its
22-bit PID, 20-bit key sequence and CRC-8/ATM, using public SGR only. It never
changes shell authorization, OSC133 trust or input routing. Fixed 4096-byte blocks
contain whole frames; padding precedes frames so trailing spaces cannot scroll
the last marker out of the bounded bottom-four-row ROI. Capacity/backpressure
counters remain in fixture reports. Unsupported PID range, small/clipped panes,
unknown cell geometry, or unrenderable colors fail closed.

A separate owned process reads X11 root-window RGB pixels using XGetImage. It
receives only expected numeric identity, cell geometry and bounded ROI, never
VTE text, paint markers or callback timings. Pane geometry and actual screen
origin define the ROI; bounds are rounded inward. The decoder checks sync cells,
all pixels, exact scale, payload/checksum and identity. It records future-marker
absence before injection, then polls at 16ms intervals for at most one second
and 64 captures. The before and final captures are exclusive-created PNGs with
SHA256; the validator decodes retained RGB independently and applies wrong-ID,
wrong-sequence and blank-image negative controls on the decoded current-marker
crop with a reported black border. This permits genuinely stale neighboring
frames in the larger ROI without mistaking them for a decoder failure. Intermediate capture intervals
are metadata only. Retained-image timing yields the conservative interval from
zero to final-readback completion minus injection start; it does not reproduce
a tighter lower bound without retaining every intermediate image.

This observes Xvfb virtual-display readback. It proves neither physical scanout
nor presentation timing on a real compositor/monitor. Each readback has monotonic
start/end timestamps; conversion/decoding/storage overhead is recorded separately
in per-request elapsed and process-CPU counts. The short gate bounds individual
readback to 250ms, each observer request to 1.5s and the active workload to 8s.
These are fail-closed observer-usability caps, not the 15-percent interaction
regression budget. Summaries report enabled/control app CPU ticks, collector CPU,
active duration and observer capture costs as one-pair descriptive measurements.
They do not subtract observer cost to manufacture latency or certify unsampled
p50/p95/p99. Future latency acceptance needs repeated calibrated observer-error
bounds and a separately reviewed fixed full workload.

Real XTest divider drags are checked against fresh snapshots: unchanged active
tab/pane identities, appropriate position movement, changed pane allocation on
the correct axis, preserved readable marker area, and an independently detected
separator-pixel shift agreeing within two pixels. The observed RGB edge must be
unambiguous and high contrast near the expected location. One-pane drag is
explicitly N/A; 32-total keys/drags must cover both actual tabs. No pane-limit
increase or unsupported 32-visible claim is made.

The observer is a descendant of the dedicated collector and must appear in its
owned identity/exit evidence. Closing pipes does not replace unconditional
pidfd-owned cleanup. Catchable cancellation and failure preserve partial JSON,
PNG and app/collector logs; outer collector SIGKILL or host loss still cannot
provide guaranteed cleanup. Final summaries always keep budget_acceptance=false.
Complete idle callback attribution, full-duration matrix, production callback
budgets, RSS/wakes and physical-display evidence remain separate open gates.


### Short-gate source review corrections

The original pixel checkpoint was rejected before native execution: separate
before/after search windows could select two stationary edges, active-loop XSync
could block the cleanup owner indefinitely, and absolute PNG paths prevented
unchanged-record replay after artifact relocation. That checkpoint remains frozen.

The successor uses one shared union region for both divider images. It requires
one unambiguous edge in each capture, correct displacement, disappearance of the
old edge, and absence of the new edge in the before image. Equal hashes are not
the criterion: stationary stripes, unrelated changed pixels and swapped edge
strengths are negative tests. Actual themes that cannot support this correspondence
fail closed rather than weakening the proof.

Pixel-mode input initialization, focus, key injection and drag injection now run
in a separate owned helper. Each injection and XSync completion fence use that
helper's SAME X connection. The parent performs no active-loop Xlib barrier and
waits at most five seconds per bounded numeric JSON request, checking cancellation
while waiting. It records the helper PID before initialization acknowledgement,
so initialization stalls also remain owned. The helper contains no shell-command,
text/token or arbitrary-key control. It is reaped by the existing unconditional
owned-tree cleanup. Real PTY tests cover a deliberately hung fake XSync, timeout,
SIGTERM/SIGINT cancellation, failed-evidence persistence and an untouched outside
sentinel. These tests do not open a display. The old raw-only input startup path
is unchanged; this correction does not claim a broad Xlib lifecycle redesign.

PNG records now contain only the exact restricted relative basename and hash.
An explicitly supplied private evidence directory is pinned by dirfd; absolute
paths, traversal, separators, symlink files, nonregular files and mismatched hashes
are rejected. The same unchanged JSON and PNG hashes replay after moving the
artifact to another private directory. Timestamp values are never rewritten to
make a relocated artifact pass.


### Native coordinate and GTK narrow-handle model correction

The first hosted pixel run (38977524020) passed all four completed one-pane
observe/control cases, independently replayed from retained images. Baseline
8-pane then failed the old pointer-delta/allocation-delta oracle: requested +12,
actual allocation +6, with verified empty owned-process cleanup. This was motion,
not an unmoved divider. Full performance acceptance remains open.

Two separate issues are corrected in this source successor:

1. Widget bounds are not X11 surface coordinates. Each fresh GTK snapshot records
   actual GtkNative surface offsets, surface/widget sizes, scale, units and XID.
   GTK 4.14.5 subtracts its API offsets from incoming surface event coordinates;
   therefore the explicitly recorded surface-to-widget vector is their negative.
   Input and ROI both invert that vector and add the same owned X11 surface root
   origin. No 5px constant or missing-value zero fallback is used. A bounded input
   helper independently verifies XID/native dimensions and acknowledges each frame
   before input. Unsupported scales, nonfinite values and changed identities fail.
2. GTK 4.14.5 gtkpaned.c expands a narrow handle's measured bounds by 6px for hit
   testing, then stores int(start-local minus expanded-origin) as drag_pos.
   update_drag truncates the end-local coordinate, subtracts drag_pos and clamps
   to min/max. The pre-injection model records actual separator and paned bounds,
   stable paned identity, GTK runtime version, direction, wide-handle state and
   min/max. Only version 4.14.5, narrow, LTR, unclamped movement is supported.
   It predicts +12 pointer -> +6 allocation and -12 pointer -> -18 allocation;
   unsupported versions/wide/RTL/clamped or incompatible geometry fail closed.
   Same-connection XQueryPointer observes start/end positions in the bounded
   helper. No expectation is derived from the post-drag allocation.

The pointer schedule remains ±12. The allocation tolerance remains ±4; independent
pixels still agree with actual allocation within ±2, with one unique edge over a
shared union plus old-edge disappearance/new-edge appearance. The pixel search
union follows the precomputed allocation displacement rather than pointer delta.
This explicitly repairs the old oracle model, without a larger input or retries.
XQueryPointer and XSync remain confined to the deadline-supervised owned helper;
the collector's unconditional cleanup path is unchanged. Normal production code
is untouched, and both disposable builds receive the identical updated overlay.

Primary implementation references: GNOME GTK tag 4.14.5, gtk/gtkpaned.c
(get_handle_area, gesture_drag_begin_cb, update_drag), gtk/gtkwindow.c
(gtk_window_native_get_surface_transform), and gtk/gtkmain.c event translation.
This source checkpoint has no new native/display result. A later master rebase
requires separate parent-pin review before publication.


### Input-window versus captured-root depth

Run38983716068 on d4f84b3 compiled both instrumented binaries successfully, then
failed the first baseline1-control initialization at a combined root/depth guard.
Owned cleanup was verified empty; no new pixel or drag case completed. Because
that guard omitted rejected values, the historical artifact does not establish
whether the failing predicate was root identity or client depth.

Input-only client geometry and captured pixels have separate contracts. The helper
now queries both client and root drawables, records their actual root IDs, depths
and sizes before any validation/translation failure, and the parent preserves the
initial observation before validation. Client geometry may have depth24 or32;
it is never interpreted as pixel bytes. Client/root IDs must match the actual
owned display root, the root must be depth24 and fixed1600x1000, and the client
must reach its configured1600x1000 geometry. Other depths/roots fail closed.

The independent observer still captures the root, and its existing actual-image
checks remain unchanged: depth24, little-endian32-bit BGRX, exact stride/masks and
bounded ROI. No pixel format tolerance, XTest bounds or drag criterion is changed.
This source correction does not prove which predicate failed in the earlier run;
the next reviewed hosted gate will retain explicit numeric diagnostics either way.


### Geometry-selected header-free divider ROI

Run38985221575 retained four independently replayed one-pane PASS cases and
verified empty cleanup. Runtime metadata proves a32-depth application client on
a24-depth root. Its baseline8 pointer motions+12/−12/+12 produced the predicted
allocations+6/−18/+6. The second drag's strict old-edge disappearance test rejected
a residual edge in the pane header intersecting the old midpoint capture ROI.
The original failed images and unchanged thresholds are retained.

Before input, the new plan uses only measured allocations: identify leaf panes
adjacent to each side of the chosen vertical separator, clip terminal content to
its pane, intersect the two content intervals with the measured handle span, and
choose the longest eligible interval (ties by top coordinate and pane identity).
The fixed64×48 ROI must fit those adjacent panes and the1600×1000 root. Its48-pixel
cross span must be entirely within both terminal-content intervals. Non-content
intervals conservatively exclude measured headers/chrome, with no fixed header
height or pixel-dependent selection. All candidates, exclusions, chosen pane IDs,
ROI and choice timestamp are retained before input, including control mode.

After movement, the same preselected adjacent pane identities must still cover
the fixed ROI span; the validator recomputes the original choice from the linked
pre-input snapshot. It never changes the ROI based on after pixels. Missing spans,
insufficient height/width, unknown scales, identity or bounds failures stop the
gate. Horizontal separators are explicitly unsupported by this narrow sampling
contract because the required strip can intersect their full-width pane header.
Current1/8/32-total scenarios use the root vertical separator; no32-visible claim.

Input schedule, GTK drag model, allocation/pixel tolerances, unique-edge union and
old-edge disappearance/new-edge appearance tests are unchanged. Observer pixels,
cleanup, native-coordinate composition and disposable Rust overlay are unchanged.
Historical geometry would select y266 rather than y484 for baseline8, but those
new pixels were not captured: this is not retrospective pixel acceptance. Source
review and a final current-head rebase are required before another short hosted
gate. The full1200-second performance matrix remains open.


### Shape-aware disappearance alternative (source-only successor)

The strict low-contrast old-edge disappearance/new-edge appearance proof remains
first. If residual content edges prevent it, an alternative derives a visual
signature solely from the dominant pre-input edge: a complete cross-axis-constant
solid strip, one or two pixels wide (the existing narrow-edge cluster bound),
with opposite signed boundaries and at least 24 RGB contrast on EACH row at both
boundaries. Fill RGB and boundary signs must match exactly across the full height
and at the destination; exterior background RGB is not the strip signature.
The pre-input dominant edge must identify one such structure, its signature must
be unique throughout the shared search union, and even one matching residual row
at the old site or preexisting destination rejects the alternative. Wide bodies,
partial/occluded strips, duplicate signatures and changed colors fail closed.

The existing aggregate 24 contrast, 80% ambiguity, expected-location and measured
displacement bounds remain unchanged. The alternative does not select an ROI or
learn colors/width from the after image. It proves transported visual structure
alongside the existing widget allocation/identity proof, not semantic identity
from RGB alone. Validator output records the proof method and learned pre-input
signature. Styles without a stable supported signature remain unsupported.
Retained failed runs stay failed; offline successor replay is diagnostic only.
No full performance budget or physical scanout is accepted by this source change.

### Native Vim buffer successor source review (2026-10-20)

The next single-parent successor is reviewed against publication head
`2fb6d7edb8310c47e47535acd496b619562b41f1`. The workflow's exact-parent pin
advances to that immutable head after independent source review and root review
of the resulting workflow and dependency changes. It is not derived from the
candidate at runtime. Another parent change still fails closed and requires a
new review; the historical baseline remains
`88c3f4c565cb5b9f8d47d22eb3ad07f47a822ae4`.

The reviewed range from `3ff8221fed5faaa84398ad1dff1dec4f63761f3e` contains
12 commits and 58 changed files. It includes Block selection/navigation and
shared-core updates as well as the native Vim workbench, so the range is not
treated as behavior-neutral. All 33 interaction and associated QA files have
identical Git blobs at the two endpoints. The observer patch applies cleanly
to the reviewed target. The combined observer identity remains
`eac561f4c56d15c6038ceee148c0bb2bba24280b95c7d8144f9d288c8dffd26b`.
The PTY, watch reader/batch, session implementation and existing budget document
were also unchanged across those endpoints.

Candidate/event identity, the single-parent equality guard, byte-identical
overlays, complete source hashes, separate fresh locked builds, actual rustc
and binary identities, rejection of identical binaries, clean original
checkouts and owned-process cleanup checks remain required. The accompanying
core-pin update also synchronizes the link-recovery fixture; its extraction
and provenance checks remain strict and its native execution remains disabled.

The Block collector still starts ordinary Block mode without `--ide`, so it
does not construct the native Vim buffer, its IM context or its frame timer.
The added terminal-toolbar entry changes chrome, however, and actual pane and
separator geometry still requires the hosted short gate. This review permits
the existing short 1/8/32-total paired pixel/control scenarios only. Source
review is not a successful native result, a Vim GUI acceptance result, a
physical-scanout measurement or permission for the full performance matrix.
