# Anvil / Forge footer control parity

This local batch changes the native footer's visual children and CSS only. It
keeps the accepted 68px frame, the existing vertical allocation, and each mode's
input owner. Block still exposes a disabled Send affordance and uses its real
terminal above; Chat retains its existing Send/Stop authority.

Tool vectors and gaps follow Forge's actual composer factory, rather than
assuming all older shared-token suggestions were already consumed. The 36px
action slot is this batch's explicit common target, verified in Anvil:

| Component | Value |
| --- | --- |
| Outer packed gap | 6px |
| Ordinary tool vector / icon-label gap | 16px / 6px |
| Tools grid / chevron / internal gap | 14px / 12px / 4px |
| Footer label | DejaVu Sans 10px, normal weight |
| Ordinary horizontal padding / border | 6px per side / 1px |
| Model slot | 120px |
| Send and Stop slot | 36px |
| Control height | 28px |

CSS content minima are not outer allocations. Opt-in native allocation evidence
records the mapped controls. At 1672px the measured Block buttons are
37/69/68/62/79/73px for Add/Attach/Image/Code/Terminal/Tools, followed by the
120px model and 36px action; adjacent packed controls have 6px gaps. The elastic
space remains between Tools and Model.

At commit3b67209 the measured Block border remained y896 while Chat was y897.
The follow-up Inspector batch removes Block's measured1px border and uses the
same inset outline/padding model as Chat. Both footer rows now measure y897 at
941px and y660 at704px, with28px visible buttons and the same68px outer frame.
The horizontal button slots remain unchanged. Cross-app typography and full
pixel acceptance remain separate review gates.

Validation and remaining observations are recorded in the task's immutable
native evidence. The first Chat capture sampled pre-map geometry, which is
missing evidence rather than proof of a changed frame. Its original failed
menu/Escape/input sequence is retained separately; ordinary typing must be
validated independently of an Escape sent without an open popup. This batch
does not alter PTY, prompt/dirty-state, resize policy, or the known running
output projection limitations.

Release compilation and native Block/Chat captures at 1672, 1356, 1000 and700px
passed the fixed-slot/frame checks. All ordinary controls remain in packed
6px groups; Tools, Model and the action remain within the narrow window. An
independent ordinary Chat input, real Bash output and whole-block native Copy
spot check passed. The active Stop state was measured at36×28px separately.

A bounded old3a/current-binary comparison preserved the original failed event
sequence: neither run showed an open Tools popup, the harness nevertheless
sent Escape, and both then captured a command missing its first character.
Both exact output and Copy assertions failed in that sequence. This reproduces
before this footer batch and is not counted as successful menu validation.
The ordinary typing positive control explicitly excludes that menu sequence;
no application input handler or safety assertion was altered.
