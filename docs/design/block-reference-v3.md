# Developer workspace reference batch

`block-reference-v3.json` records the new Anvil/Forge logical geometry. Both
Block and Chat now use `jterm_core::developer_workspace`: a 55px toolbar,
53px rail, 191px navigation, 298px inspector and 42px local header. At
1672×941 the center starts at x244 and the inspector at x1374. Block's Live
toolbar is 36px. The two composers share a 68px minimum height, 12px horizontal
inset and 8px bottom inset; native multiline input and explicit user font
sizes retain their existing bounded growth. Inspector-first and then
navigation collapse retain the shared 480px center budget.

The Main default is `JetBrainsMono Nerd Font Mono 12px`, using absolute
logical pixels. Missing UI fields resolve independently to 14pt and scale 1,
including with an explicit Main font. Explicit env/TOML font strings and
px/pt units retain their authority. Loading does not rewrite user files. The
optional UI keys remain commented in the shipped template so adding new
active keys cannot trigger the installer's schema-replacement migration.
Older configurations that relied on missing UI keys inheriting Main therefore
change chrome appearance under the new independent default.

Chat's authored default heading/body/code roles remain 20/12/10px, normalized
against the new 12px Main nominal. Explicit older Main values consequently
have different relative authored-message extents; preserving those values
does not imply identical old rich-text pixels. Running command text is
Main-owned within its UI-owned status header, so UI 14pt cannot enlarge it.
The terminal itself remains the only Block input owner; the experimental
Block input dock stays disabled.

Default native Block and labelled Chat fixture captures at 1672×941 and
1356×704, DPR1, measured the major boundaries exactly against this contract.
Both composers measured 68px in both sizes. Real synthetic shell commands
covered exit 0, exit 7 and an interrupted foreground process (exit 130), with
exact recorded command bytes and a post-resize output burst. This is the
history-metadata-enabled path, not proof that the open native-anchor fallback
works. Opt-in acceptance diagnostics read actual GTK bounds only after resize;
they do not drive layout or input.

The small-default readonly command regression and its old/new strict evidence
are documented in `readonly-command-settling.md`. The final candidate passed
32 headless font tests and 12 necessary native regressions without weakening
the original full-command, width, font, input or selection assertions. GTK
allocation warnings remain in the evidence. Running prompt echo, a separate
running gutter, the large running blank extent, history-disabled anchor
integrity, and complete pixel/IME/frame-sequence acceptance remain open.
This batch is a local implementation checkpoint, not a 1:1 acceptance claim
or authorization to publish it.
