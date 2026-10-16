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

## Reviewed hosted smoke checkpoint

The narrowly scoped path-filtered push workflow builds only disposable observed
binaries. Baseline is pinned to 88c3f4c565cb5b9f8d47d22eb3ad07f47a822ae4;
candidate is the immutable event SHA, with its parent required to equal reviewed
A08 head 1f89fcfa362c07326a42415d60682e7fa13a3914. A later changed parent fails
closed and requires another reviewed pin. Both trees receive the byte-identical
observer overlay (digest 7b06d789ec1d7c5ceab5027c4dbeac2841bfd382ba98763ef9b80ffa3197e6b7).
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

The publication parent pin is rebased to the reviewed A08 head above, preserving
all published A07/A08 production changes. The baseline remains the historical
88c3 commit intentionally: this short smoke checks that the same observer and
raw echo/cleanup contract work on both revisions, not that differences are caused
by A04 or that either revision meets performance budgets. The candidate remains
the exact push-event commit, with an exact-parent guard. A different master head
requires another pin review rather than replaying an obsolete tree operation.
