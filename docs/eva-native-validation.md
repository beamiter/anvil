# GTK Eva validation snapshot — 2026-10-30

## Passed
- Forge application `cargo check --lib`:28.19s; Anvil application `cargo check`:27.35s, final source with shared settings/atlas, measured clipping, dialogue/selection consent, memory review and retirement acknowledgement.
- Tiny production-paint/native-geometry harness:2 tests passed. First covers104 skin/pose native markup cases. Second tests all312 native96/144/192DPI measurements against the actual production safe-edge selector and real protocol rectangle implementation.
- Native offscreen Pango/Cairo312-frame draw; 120 raw PNGs retained locally (reproducible, not shipped); the contact sheet and exact measurement/source metadata are shipped. All eight skin outputs visually inspected.
- `git diff --check` clean for both repositories.

## Not claimed
- This is not GTK-window end-to-end: actual Unix socket creation/display paths were unavailable in the restricted runner. No desktop/IPC workaround was attempted.
- Live cross-host pixel retirement, SIGSTOP/daemon-restart native frames, Wayland/X11 timing, CJK preedit/IME, physical GPU/input/scroll latency/audio remain unrun. A completed GDK native presentation is not compositor scanout evidence.
- The full historical GTK test suites were not completed: earlier parallel runs were explicitly interrupted for build resource contention, not counted as passing.
- Project scope is explicitly chosen from registered task metadata or deliberately named for a real local pane UUID; no directory inference/access. Archived task/project discovery and management-only exact-scope/digest approval are implemented. Live display/IPC validation of these controls remains unrun.

## Evidence
Forge `qa/eva-native`: `final-checks.log`, `native-render.log`, `measurements.json`, `native-contact-sheet.png`. Reproducible per-frame JSON/PNGs are generated locally and ignored, not shipped. The same byte-identical production paint helper is used in Anvil and Forge.

## Safety and build boundary
New shared mode defaults off and is enabled by saved native setting/restart or explicit environment opt-in. Local body paths stay disabled across shared IPC failures. Settings previews are static in shared mode. Input/review/selection/remote guards fail closed; no PTY, microphone or automatic terminal-text access is added. Existing close guards run before shared withdrawal/cleared-native-frame/durable-ack closing. Unknown proof has no timeout fallback.

Dependencies used: workspace-only official GTK4>=4.14 (provisioned4.18.6), libadwaita>=1.5 (1.7.6), VTE GTK4>=0.76 (0.80.1), Pango/Cairo/Graphene/GIO; Rust1.99.1. Commands used the shared native-build flock and one build job. No remote pushes, credentials, live cloud requests or user desktop changes.

## Scope parity validation

After adding native task/project/persisted-scope discovery and management consent,
`qa/eva-native/scope-parity-checks.log` records 4/4 fixture tests passing and both
Forge library and Anvil native application checks passing (serialized build lock,
jobs=2). Two tests use the actual production scope authority helper to reject stale
pane/project/revision/host reviews and verify archived management does not adopt
its target as the active task. The other two retain the production-paint and
312 measured 100/150/200% geometry checks. Live scope-management GTK/IPC interactions
remain unrun; these checks are not desktop end-to-end evidence.

## Final immutable-pin verification

With companion `9aa569540091524d637a7115b64c587b9177a55b` and core `974ccad203375032fe87706caecab39ad516c238`, `--locked --offline` GTK fixture tests passed **4/4** and Anvil production check passed (18.12s). Source and package metadata checks do not certify live desktop/hardware operation.
