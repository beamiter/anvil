# Native Vim9 IDE

## Simple plugin workbench

With the updated [beamiter/vimrc](https://github.com/beamiter/vimrc) loaded,
**Ctrl+Shift+P**, then **`>simple`**, exposes the configuration's Simple plugin
actions in the native command picker. Choose files, buffers, grep, symbols,
rename, code actions, Git status/diff/current hunk, Markdown preview, key help,
or the Simple workbench. Availability follows the plugins and current buffer;
an unavailable action explains what it needs.

The picker also shows the Simple configuration's cached Git branch/change
counts, diagnostic counts, remote workspace label and large-file mode when
available. The same context appears in the optional native status bar. No
extra panel is enabled, and the native buffer retains all space when the
picker is closed. Context collection does not start Git, LSP, or remote jobs.

The picker now provides **Git**, **Code**, **Tree**, **Plugins**, and **Status**
filters. Each button shows the corresponding live cached state on hover;
selecting it keeps focus in the search field and displays a detailed summary.
The filters can also be typed directly:

| Filter | Available workflows |
| --- | --- |
| `>simplegit` | Status, diff, hunk preview/navigation/staging/confirmed undo, blame and history |
| `>simplecc` | Language actions, diagnostics, language-service status/restart/log |
| `>simpletree` | Toggle, reveal and refresh the project tree |
| `>simpleplug` | Plugin status, update checks, install/update/stop, change review, startup profile and health |
| `>simpleline` or `>simplestatusbar` | Statusline health and visibility |

A filter chooses a category; a command runs only after selecting a result.
Unavailable commands retain their explanation. Switching filters clears old
selection/activation state, and every editor action is resolved against the
current source buffer, window, changedtick and cursor. Plugin progress, LSP
state, tree context and statusline state refresh from the Vim configuration's
published caches. The normal minimal editor layout is unchanged when the
picker closes.

Vim and the Simple plugins continue to own the action's editing behavior and
popups. Ordinary supported completion menus use the native paint path; other
plugin popups and preview views can still use the live compatibility display.
This integration does not turn browser Markdown previews into embedded web
views. The standard Vim configuration also remains usable outside these
terminals.

With the tool and diagnostic panels hidden, **F8** and **Shift+F8** reach Vim's
SimpleTerminal toggle/new mappings. Enabling the native diagnostic panel gives
those keys to its previous/next diagnostic actions.


The Vim9 workbench gives real Vim 9 a native GTK interface. Vim owns text,
normal/insert/visual modes, registers, macros, undo, buffers, splits, and your
existing Vim configuration. The main buffer is rendered by GTK/Pango from
Vim's buffer text, display positions and **GUI RGB highlight attributes**.
It does not obtain its colors from ANSI output or VTE palette entries. VTE
continues to own the real interactive PTY and the compatibility display.
Native widgets communicate with a bundled Vim9script bridge through a private,
authenticated local connection.


## Buffer-first defaults and optional workbench

The default editor opens with a native buffer and a small title/font header.
Project browsing, open-buffer controls, tool rows, diagnostics/output and the
extra status bar are opt-in. Put this section in the terminal's `config.toml`;
the same keys apply to Anvil, Forge, Ember and Frost:

```toml
[vim9]
auto_gui = true
native_buffer = true
smooth_scroll = true
# Optional Vim IDE chrome: all default to false.
toolbar = false
buffer_tabs = false
status_bar = false
project_panel = false
tool_panel = false
```

Set individual options to `true` before opening a new editor window to restore
those controls. `toolbar` restores header actions; `buffer_tabs` shows the open
buffers and split controls; `project_panel` shows the directory browser;
`tool_panel` shows project search, build and Problems/output; `status_bar` shows
editor state and the render badge. The project directory is not scanned while
its panel stays closed. Existing configs without `[vim9]` get these minimal
defaults. Ordinary terminal settings saves preserve the section.

Workbench shortcuts only intercept keys when their corresponding optional
surface is enabled. Hidden Search/Build fields cannot capture keyboard focus,
and a hidden build panel does not take over F5/F8 from Vim. **Ctrl+S** and
**Ctrl+Shift+S** remain buffer save actions. The editor font menu remains
available, as do **Ctrl+=**, **Ctrl+-**, **Ctrl+0** and **Ctrl+wheel** for buffer
font zoom; these do not change terminal or sidebar font settings. Rendering
status and compatibility reasons are available inside the font menu even when
the status bar is hidden. Real errors and unsaved-close feedback still appear.

The GTK renderer retains one immutable text/background scene, separately from
cursor and IME overlays. Cursor-only, focus and composition changes reuse the
native render nodes; changed text, syntax, selection, viewport, font, spacing
or DPI rebuild the scene. Shared `Vim9RenderMetrics` supplies the same logical
pixel geometry for viewport clips, cursor painting and IME positioning.

For future native hover cards, Markdown preview, images and color pickers,
`jterm_core::vim9` provides source anchors tied to a Vim window, buffer,
changedtick and UTF-8 byte location. The existing GTK overlay can host native
widgets at those pixel anchors. An asynchronous result must re-resolve its
source anchor after scroll/zoom and be discarded after a source edit or window
closure. This release establishes the attachment contract; it does not claim
those advanced content renderers are already implemented.

## Native buffer rendering

Native paint rejects invisible ink before shaping it and builds diagnostic
waves only for the visible horizontal span, including cursor inversion.
Clipping keeps the original row baseline and wave phase during fractional
scrolling; a partially visible row does not move its underline to the edge.

Completion and popup render nodes are retained across unchanged cursor/focus
frames. Their own contents, GUI theme, widget clip, font settings, scale or
Pango context changes rebuild the overlay without invalidating unrelated text.
Closing the final overlay releases the retained node.


Native rendering is enabled by default for supported ordinary buffer views.
The rendering menu reports **GUI · RGB / Pango** while the native surface is drawing;
`status_bar = true` also enables the header badge.
Vim `guifg`, `guibg` and `guisp` colors are preserved as full RGB, including
when `termguicolors` is disabled. Bold, italic, underline, undercurl,
strikethrough and Visual selection are drawn by the GUI. Pango shapes complete
same-style runs, with font fallback and optional font ligatures. The mode cursor
and IME preedit are separate native layers. Text runs and preedit share a row
baseline, so CJK fallback fonts align with Latin text. Text commits still go to
the actual Vim process, so editing, undo and mappings have a single owner.

Use the header's **Editor rendering, font and line spacing** menu to choose a
monospace editor font, enable or disable ligatures, adjust line spacing, or
select the compatibility display. These controls affect this editor window;
the terminal workspace and sidebar keep their own font settings. The current
bridge retains Vim display-cell positions for mouse and cursor alignment; this
does not yet implement variable-width document layout or arbitrary inline
widgets.

The native surface covers each buffer viewport independently, including
horizontal, vertical and nested Vim splits. Vim's status lines, split dividers
and command area remain live in the compatibility display. Every pane uses its
own buffer text, syntax and viewport; the active Vim window alone owns the
native cursor and IME preedit. The bridge explicitly falls back for display
features it cannot yet reproduce faithfully, including closed folds, wrapped
continuation rows, diff, general popup windows and complex/scrolling completion menus,
complex signs or fold columns, virtual text, complex match patterns, concealment,
and complex bidirectional text. The default single sign column, ordinary
diagnostic text-property highlights and simple search matches use their GUI
styles. If any pane needs compatibility, the whole editor shows live Vim until
all panes can be sampled faithfully. The rendering menu gives the reason; native
rendering then resumes automatically. Resize transitions also
retain the compatibility view until Vim publishes matching geometry, so a stale
frame cannot cover a differently sized input grid. Selecting terminal text
with Shift-drag temporarily reveals the compatibility display so selection
feedback remains visible. No options in the user's
Vim configuration are disabled to force the native path.

## Open Vim from the command prompt

In newly opened local interactive panes, `vim`, `vim file.txt`,
`vim +37 "name with spaces.txt"` and multi-file/split arguments automatically
open the native GUI editor. The shell expands and quotes arguments normally;
the frontend receives the exact argument vector and working directory. This
uses the originating tab/pane inside the existing GUI. The original terminal
view is restored when Vim exits, and its shell receives the actual exit status,
including `:cquit`. No second GUI window is created for shell entry.
Unsaved-buffer close protection still belongs to Vim.

`auto_gui = false` keeps the ordinary command-line entry in the terminal.
For one invocation, use `JTERM_VIM9_AUTO_GUI=0 vim file.txt`, or call an explicit
Vim executable path. Redirections/pipes, Ex/script invocations, version/help
queries and headless sessions keep ordinary Vim behavior. Managed SSH panes,
validation workers and Flatpak host shells do not receive this local launcher.
An explicitly configured alias or function remains the shell's responsibility.
The integration is confined to the child shell environment; no shell startup
file or user Vim configuration is rewritten. Bash login shells retain the
launcher after a startup file resets PATH, and the owned Bash-to-jsh wrapper
restores its child-local PATH prefix after sourcing the user's rc.

## Scrolling and presentation

Native scroll events target the buffer split under the pointer, including an
inactive split, without moving editing focus. Trackpad pixel deltas accumulate
at the actual line height; three wheel lines are used per discrete notch.
Events are coalesced once per GTK frame, bounded and sent as typed viewport
requests. Vim remains the source of truth for which rows became visible.

After Vim acknowledges a viewport movement, the GTK renderer eases the
per-window text and cursor over 100 ms. Retained outgoing scenes fill exposed
edge rows, and clips preserve split dividers and status lines. Text/layout
changes, zoom, input-method composition and editing/click input stop the short
presentation tail before the next interaction. `smooth_scroll = false` or the
system reduced-motion setting disables this easing; the precise fractional
input path remains available. The font menu also has a per-window toggle.

The renderer now follows GTK's frame clock instead of a fixed 16 ms timer, so
high-refresh monitors determine presentation timing and unmapped surfaces do
no polling. Pango keeps fractional glyph positions; text shaping and shared
baselines continue to preserve Unicode, font fallback, GUI RGB colors and
buffer-only font zoom. This implementation has not established a performance
lead over VS Code, and physical high-refresh/Wayland/IME device validation is
still required.

## Start a project

```sh
anvil --ide ~/project
anvil --ide ~/project/src/main.rs
```

You can also use the editor icon in the terminal toolbar, choose **Open Vim9
IDE workbench** in the command palette, or press **Ctrl+Alt+I**. These entries
use the local pane's working directory. Each workbench launches as a separate
process: closing the original terminal window does not terminate the editor or
discard its modified buffers. Existing terminal workspace restore is not run
in the `--ide` process.

The runtime requires **Vim 9+ with `+vim9script`, `+channel`, `+job`, `+timers`, and `+quickfix`**. It uses
`vim` from PATH. Set `JTERM_VIM9_BIN` to a specific Vim executable when needed:

```sh
JTERM_VIM9_BIN=/usr/local/bin/vim anvil --ide ~/project
```

An unsupported or missing Vim produces a visible launch error. The application
does not install an editor, overwrite `.vimrc`, or install language servers.
The bridge is bundled with the shared core and extracted to its managed script
cache automatically.

`--ide` accepts a project directory, an existing file, or a new filename whose
parent directory already exists. Pass the target positionally or with
`--working-directory PATH`. A new file becomes a named Vim buffer and is written
only when saved. Normal terminal launches still require a directory. `--ide` cannot be combined with
`--execute`, `--mode`, or `--safe-mode`. Managed SSH panes are not local IDE
project roots; select a local pane or launch a local checkout explicitly.

## Workbench layout (opt-in)

| Area | Behavior |
| --- | --- |
| Project | Browse one directory at a time, filter filenames, open files, refresh, and move to the parent directory. Directory reads run off the GTK thread and are bounded to 1,024 entries. |
| Quick open | Ctrl+Shift+P opens a native fuzzy picker across the project. Type a filename or path, use ↑/↓ to select, and press Enter to open. Indexing runs in the background and reports progress, errors, and truncation. Refresh rebuilds the index. Prefix the query with `>` to find and run IDE commands. |
| Navigation history | Back/forward buttons and Alt+Left/Alt+Right move through the active Vim window's real jump list, restoring the file and cursor position. Native Go to line records a jump as well. Buttons reflect whether a direction is available. |
| Open buffers | Shows real Vim buffer names, current-buffer marker and modified marker. Activate a row or use previous/next controls to switch; Close current buffer preserves modified-buffer protection. Split vertically/horizontally creates real Vim windows. |
| Editor | GTK/Pango draws supported buffer views with Vim GUI RGB styles, native cursor and IME preedit; real Vim owns editing, mappings and undo. VTE remains the PTY/input adapter and compatibility view. With multiple splits, a subtle inner outline identifies the active Vim window and dims when the editor loses focus. |
| Search project | Press Enter or Search to run a search with the selected case mode, literal/regex, whole-word and file-glob options. Stop cancels an active search. Requires `rg` in Vim's environment. Progress, errors, cancellations, match counts and omitted-result counts are visible below the build controls. |
| Problems | Displays an E/W/I severity label, project-relative filename when valid, line, column and a wrapping message. Hover a row for the full path and message. It identifies the current quickfix list as Search, Build or Vim. Vim may also supply location-list or language-server diagnostics. Activate a result to open its exact location. F8/Shift+F8 navigate Vim diagnostics. |
| Build | Edit the visible shell command and press Build/F5. Builds run asynchronously in the project root. Cancel stops the active build. Exit status and a bounded output tail are shown in the native Build output tab; parsed diagnostics populate Problems. |
| Status | Live Vim mode, filename, modified flag, cursor location and total lines, filetype, encoding, window/buffer counts, and bridge connection state. |

The default build suggestion is `cargo check --message-format=short` for a Cargo project, `make` for a
Makefile, or `cmake --build build` for a CMake project. The command runs only
after an explicit Build/F5 action. **Save all + Build** first saves every modified buffer and runs the command only if every save succeeds; ordinary Build/F5 uses files already on disk. Review or replace the command for the project. Vim's
`errorformat` controls compiler-output parsing; select an appropriate `:compiler`
in Vim when its default format does not match your toolchain. Native snapshots
retain up to 256 buffers and diagnostics, marking truncated counts with `+`.
Project text search retains up to 256 matches in both Vim quickfix and the native
Problems view; omitted-match counts are displayed explicitly. The normal Vim
quickfix and language-server interfaces remain available, with the same search
result cap. Native Build output shows the most recent 80 retained output lines.

The New file button opens a native filename chooser and creates a named Vim
buffer; the file is written when you save it. The file picker can open any local
UTF-8 path. Sidebar navigation stays inside the project root, including symlink targets. The directory browser stays separate from Quick open: it reads one folder at a time. Quick open invokes `rg --files --hidden -0` in the project root, respects ignore rules, and does not follow directory symlinks. It retains at most 20,000 files and shows the best 100 matches. Reopening the picker reuses its index; use Refresh after adding, renaming, or removing files. Missing `rg` and indexing errors are shown in the picker.

Quick open also accepts `src/main.rs:42` and fuzzy queries such as `smrs:42:8`.
The destination is shown before opening; columns are one-based UTF-8 byte
columns, matching Vim. Type `:42` or `:42:8` to move within the current buffer,
including an unnamed or modified buffer, without opening another file or
waiting for the project index. Invalid or incomplete locations keep the picker
open and show a validation error. An exact indexed filename containing a colon
(such as `src/notes:12`) remains a literal file. Enter uses the latest query even
before the typing debounce finishes; a stale clicked row cannot open another
file after the query changes.

Prefix Quick open with `>` to search the built-in IDE command catalog by name or
alias. The list shows existing shortcuts and explains commands that are currently
unavailable. Enter runs the selected command after rechecking the current Vim
state; an unavailable command keeps the picker open with its reason. Command
results update while the picker is open and do not wait for project indexing.
Use `./>notes` to open a literal filename beginning with `>`. Ctrl+Shift+P still
opens the file picker even when toolbar chrome is hidden, and Escape returns
focus to Vim. Plain Ctrl+P remains available to Vim.

A completed project search with no matches shows a clear Problems empty state
only while that search owns the current quickfix list and no location-list or
language-server diagnostics remain. Failed or cancelled searches keep their
existing error and cancellation feedback.

## Shortcuts

| Scope | Shortcut | Action |
| --- | --- | --- |
| Terminal workspace | Ctrl+Alt+I | Open Vim9 IDE for the local pane |
| IDE | Ctrl+Shift+P | Quick open `file[:line[:column]]`, `:line[:column]` in the current buffer, or `>command` |
| IDE | Ctrl+Shift+G | Go to line or line:column |
| Vim editing grid | Alt+Left / Alt+Right | Back / forward through Vim jump history |
| IDE | Ctrl+Shift+F | Focus project text search |
| IDE | Ctrl+Shift+B | Focus the build command |
| IDE | Ctrl+Shift+O | Native file picker |
| IDE | Ctrl+S | Save current buffer |
| IDE | Ctrl+Shift+S | Save all buffers |
| IDE | Ctrl+Shift+PageUp / PageDown | Previous / next buffer |
| IDE | Ctrl+Shift+W | Close the current buffer, retaining unsaved-change protection |
| IDE | F5 | Run the visible build command |
| IDE | F8 / Shift+F8 | Next / previous diagnostic |

These IDE shortcuts apply inside the dedicated editor window, while no native text-entry field or dialog is active. Escape returns from a native field to Vim before using editor shortcuts. Plain
Ctrl+P, Ctrl+O, Ctrl+G, and Ctrl+W remain available to Vim for completion, jump navigation, file information, and window commands;
the native file actions use Shift to keep those Vim keys intact. Other keys
go to Vim or the focused native control. In terminal configuration,
`[keybindings].open_vim9_ide` overrides the launch shortcut.

Escape in a native search/build field returns focus to Vim. Escape in the quick picker or go-to dialog closes that dialog. The folder filter follows the selected row when Enter is pressed. Header buttons show/hide the project and output panels while preserving the editor session; starting a build or search reveals the corresponding output tab.

### Search options and result ownership

Open **Project search options** beside the query to choose **Smart case**
(the default), **Match case**, or **Ignore case**. Smart case ignores case for
a lowercase query and matches case when the query contains uppercase text.
Search is literal unless **Regular expression** is selected; **Match whole
words** adds ripgrep's word-boundary matching.

The file field accepts one ripgrep glob, such as `*.rs`, `src/**`, or
`!*.lock`. Spaces and commas are part of that one pattern. Empty means all
searchable files; the project ignore rules still apply. Changing a control
does not run a job. Press Enter in the query or click Search to apply the
options, and use Stop to cancel. Feedback describes the options attached to
the last search even if the controls have since changed.

A cancelled or failed search retains the preceding quickfix list. The search
feedback distinguishes cancellation, an error, and a successful search with
zero matches; the Problems header identifies the quickfix list currently
displayed. The quickfix count and current item are available in its tooltip.
Back/forward follows Vim's active-window jump list, so ordinary Vim jumps and
native navigation share history. It respects Vim's modified-buffer checks,
and Alt+Left/Alt+Right remains with native controls when the editor grid does
not own focus.

## Language services

The bridge detects commands supplied by your Vim configuration. The native
buttons enable only when their provider is present:

| Native action | Vim command |
| --- | --- |
| Definition | `LspGotoDefinition`, with native tags as a fallback |
| Hover | `LspHover` |
| References | `LspShowReferences` |
| Rename | `LspRename` with a validated symbol name |
| Format | `LspFormat` |

These are the commands exposed by the Vim9 LSP integration the bridge supports.
Install/configure language servers in Vim as you normally would. Disabled
buttons explain the missing capability. Completion, snippets, debugging,
Git workflows and other plugin features remain available through Vim; this
workbench does not claim to supply its own LSP server, debugger or Git client.

## Saving and closing

Save buttons send typed bridge requests. File navigation uses Vim's normal
editing rules, so an unsaved current buffer may prevent a switch when `hidden`
is disabled. Busy command/operator/completion states are reported instead of
injecting Ex commands into user input. Finish the operation or press Escape,
then retry the action.

The window close button asks Vim to quit gracefully. Modified buffers keep the
window open. Save them and close again, or enter `:qa!` in Vim only when you
intend to discard those changes. No forced quit is hidden behind a GUI close
button. A disconnected bridge leaves the real Vim terminal available so you
can save and quit using Vim directly.

## Validation

The GTK CI job installs Vim and ripgrep and runs the native Vim9 regression explicitly under Xvfb.

CLI tests cover directory/file/new-file launch validation, unchanged terminal
directory requirements, spaces in project paths, and incompatible mode flags.
Shared-core tests cover command encoding, authenticated transport, file path
handling and bounded snapshots. An opt-in native GTK smoke test exercises a real
Vim PTY, native GUI RGB with deliberately different terminal colors and
`termguicolors` disabled, Unicode IM-context commits and real Vim undo, native
render subscription toggling and native split painting, fuzzy opening of a nested
file, typed line/column navigation and
jump-history back/forward, native search case/regex/word/glob options, invalid
patterns and zero matches, real editor splits, protected buffer close, explicit
save-all-and-build, native save/build controls, diagnostics, and modified-buffer
close protection. Run it under a
graphical display (for example Xvfb) with the required Vim:

```sh
cargo test native_vim9_command_palette_modes_and_live_availability -- --ignored --test-threads=1
cargo test native_vim9_edit_save_build_and_modified_close -- --ignored --test-threads=1
cargo test native_vim9_split_unicode_visual_and_font_scaling -- --ignored --test-threads=1
```

The split regression opens distinct real Vim buffers, checks independent GUI
syntax colors with `termguicolors` disabled, CJK and combining-character
geometry, Visual selection, editing and native Save, nested splits and native
font changes. Font size changes must produce a resized Vim grid and a fresh
painted frame; same-size font style changes also request a fresh frame. The
test checks that unsupported display states retire the native
overlay and supported states restore all panes. Run it separately with
`GDK_SCALE=2` on a sufficiently large display to exercise GTK HiDPI rendering.
Pango layout caches are retired when the window changes monitor scale factor.

Set `JTERM_VIM9_TEST_REAL_KEYS=1` with `xdotool` on an isolated X11 display
to additionally exercise actual key delivery through the native IM filter.
Set `JTERM_VIM9_TEST_SCREENSHOT=/tmp/vim9.png` with ImageMagick `import` to
record the live RGB and Unicode editor windows. This covers GTK IM-context
commits; platform-specific input-method engines and candidate windows still
need testing on the target Wayland/X11 desktop.


### Recovery validation — 2026-10-20

Pinned shared core: `c387ab33caff52a0bd4f759fb20a35aae2586cd0`.
Passed 4 focused tests and 3 separate real GTK runs (split/Unicode/font at scale 1 and 2, plus edit/save/build/close).
These runs use Xvfb/software graphics; physical GPU, Wayland and OS IME candidate windows are not certified.


## Native plugin highlights

Single-line `matchadd()` / `:match` patterns and byte-based `matchaddpos()`
positions now use native GUI RGB paint, including the built-in `matchparen`
plugin. Match priorities, equal-priority insertion order, search precedence,
Visual selection and independent split-window ownership follow Vim. Plugin
matches update and disappear without editing the buffer or emitting a special
invalidation hook. Syntax, properties, text, undo and input remain Vim-owned.

Multiline/context-dependent/zero-width regexes and conceal matches retain an
explicit compatibility reason. Native paint is bounded to 64 matches and 2048
positions per window and 512 match/search ranges per source line; exceeding a
budget shows the real Vim compatibility view instead of partial decoration.

### Plugin validation — 2026-10-21

Pinned shared core: `feae27f7cd965332253a81723095f75850241e81`.
The locked Nix native test build and the complete GTK/VTE
`native_vim9_edit_save_build_and_modified_close` workflow pass under Xvfb with
an isolated Vim startup configuration. Both regex and positional plugin paint
reach the native Pango surface, disappear without editing, and retain the GUI
RGB values while terminal truecolor is disabled. The workflow additionally
checks IM commit, undo, native save/search/build/navigation and modified-close
protection. The optional `JTERM_VIM9_TEST_REAL_KEYS=1` variant failed at later
save/jump steps with Vim busy-state refusals on this Vim 9.2 host; that optional
variant is not reported as passing. Plugin-paint screenshots were inspected.


## Continuous wheel scrolling — 2026-10-21

Pinned shared core: `2b00c1d3d92f4f5555969d6b62a410866d4acb34`.
Supported Normal-mode buffer views now turn each three-line wheel notch into
a 100 ms continuous input transition. New notches retain their own completion
deadlines; reversing cancels the old direction's unplayed tail. The GUI frame
clock advances motion even without new Vim render messages. Trackpad pixels
continue following OS input directly. Source-checked acknowledgements rebase
paint, hit testing and caret/IME together; a captured press freezes future
wheel animation. Editing, preedit, focus changes, zoom and changed layout
retain their cancellation rules. Turning smooth scrolling off keeps the
existing compatibility behavior.

Render protocol 4 prefetches eight real source rows on each edge (protocol 3
hosts retain two). This gives ordinary notches and short bursts text to expose
while Vim handles the request. File boundaries, unsupported views and missing
margin coverage remain bounded by the actual Vim source.

The shared runtime passes 93 real-Vim regressions. The core Rust Vim9 suite
passes 77 regular checks plus its explicit real-PTY round trip; deterministic
wheel tests cover 60/120/240 Hz timelines, exact distance, acknowledgement
continuity, reversal, file edges and pointer capture.

The locked Nix test build and the real GTK/VTE
`native_vim9_fractional_scroll_and_split_isolation` test pass under Xvfb.
The test samples intermediate source/pixel positions during a nine-line wheel
burst, then checks 32 further notches without losing distance or moving the
other split. It also checks fractional trackpad input, focus ownership and
cancellation before editing. Reduced-motion settings retain their existing
behavior. These checks do not establish a physical-display frame-rate result.


## Vim-owned native cursor — 2026-10-21

Shared core: `2fa91124b7890acb97b2d628c57b84e92d9b6d38`. The native buffer
now follows Vim's `guicursor` shape, percentage, GUI RGB highlight and blink
timings. Normal/Visual, exclusive Visual, operator-pending, Insert and Replace
have independent settings. Ordered `a:` overlays and linked highlight groups
are preserved; the second highlight applies when language mappings are active
in Insert/Replace. An unspecified group reverses the actual syntax/selection
colors; an explicitly cleared group retains its configured appearance.

A solid block repaints the source glyph in the cursor foreground so Latin,
CJK and combining text remain readable. Thin bars change only cursor paint;
source hit testing and the IME anchor retain their full character rectangle.
Cursor-only updates retain the existing text-shaping cache. Unfocused cursors
use steady outlines. Blink timing follows `blinkwait`, then `blinkoff`/`blinkon`;
any zero value disables blinking. Input, source and mode changes restart the
wait, while unrelated render revisions do not.

For a steady block in Normal/Visual, a 25% insertion bar and a 20% replacement
underline, put this in the usual vimrc:

```vim
highlight Cursor guifg=#20242c guibg=#d8dee9
highlight InsertCursor guifg=#20242c guibg=#73a2e1
set guicursor=n-v:block-Cursor,i:ver25-InsertCursor,r:hor20-Cursor,a:blinkon0
```

Existing frames without `appearance` retain their compatibility cursor.
Command-line, popup and other unsupported views still use Vim's live terminal
surface. Native showmatch timing is not inferred from terminal output.

Validation: the locked Nix/Xvfb GTK test
`native_vim9_fractional_scroll_and_split_isolation` passes, including native
snapshot RGB/size checks for block, insertion, replacement and CJK cursors,
visible/hidden blink phases, and unchanged text-scene build counts while
blinking. The existing continuous-wheel/split/input checks also pass.
The local run used `--config profile.test.package.anvil.opt-level=0` for the
frontend test package; dependency profiles and shipped settings were unchanged.
The shared core passes 80 Rust Vim9 checks plus the explicit real-PTY round
trip and 99 real-Vim runtime regressions. These are headless X11 checks and
do not establish a physical-display or Wayland performance result.


## Native Insert completion — 2026-10-21

Pinned core: `20a1c694792f5737c8b92b32ef02e40f6e2c08d9` (render protocol 5).
Ordinary Insert completion menus now paint above the native GUI buffer, with
Vim's GUI `Pmenu`/`PmenuSel`, kind and extra-column colors. Word/abbreviation,
kind/menu labels, CJK and combining characters use the existing native glyph
path. The menu keeps Vim's screen geometry and padding and can cross split
separators without being clipped to one editing pane.

Vim still owns candidate generation and keyboard/mouse behavior. Ctrl-N/P
select, Ctrl-Y accepts and Ctrl-E cancels through the original PTY; the menu
never takes keyboard or IME focus. Its appearance cancels old buffer scroll
transforms, and completion mouse/wheel input bypasses semantic buffer actions.
Previously captured native gestures retain their cancelled physical tails.
Closing the menu clears the overlay and restores the same source buffer.

The list must fit fully without a scrollbar, use default column order and
contain unclipped ordinary labels. Long/filtered/fuzzy menus, separate info
popups, borders, opacity, custom `pumopt` and distinct `PmenuMatch[Sel]` styles
retain explicit compatibility reasons. Available per-item highlight metadata
is applied, but older Vim builds (including this host's 9.2.0858) omit those
fields from `complete_info()` and expose only standard menu highlight groups.
Hosts using render protocols 1–4 always retain their live compatibility menu.

The native menu now has a restrained four-pixel shadow and a one-device-pixel
inside border derived from its foreground color. The shadow clips to the editor
surface; Vim's menu rectangle, row colors, text and input geometry are retained,
including menus crossing split separators or touching the window edges.
`native_completion_elevation_preserves_rows_and_clips_to_widget` checks actual
GTK/Cairo pixels for row colors, border placement, shadow bounds and menu removal.
The native pixel workflow runs this display test explicitly under DBus/Xvfb.

Shared validation: 82 focused core Rust checks and the explicit real-PTY round
trip pass. The real-Vim runtime suite runs 106 checks: 105 pass, with one
per-item highlight test skipped because this Vim omits that optional metadata.
Menu GUI RGB, Unicode, selection, acceptance/cancellation, cross-split geometry,
row-delta closing and old-protocol/scrolled-menu compatibility are covered.

The real GTK/VTE Xvfb test `native_vim9_fractional_scroll_and_split_isolation`
passes with the final core, including Pmenu/PmenuSel snapshot RGB, real CJK
candidates, Ctrl-N/P/Y/E, unchanged focus, menu close/recovery and raw wheel
bypass, followed by the existing cursor/continuous-scroll/split checks. The
frontend test package used `profile.test.package.anvil.opt-level=0`; shipped
settings and dependency profiles were unchanged. Screenshot:
`/tmp/anvil-vim9-completion-20a1c69.png`. GTK paint and pixel-scroll paths each
compute the Pango baseline once per scene. Headless X11 validation does not
establish physical-display or Wayland performance.

## Buffer labels and pointer controls

With `[vim9] buffer_tabs = true`, buffer names use the shortest distinguishing
path suffix. For example, two `src/main.rs` files appear as
`client/src/main.rs` and `server/src/main.rs`; full paths remain in tooltips.
Unnamed or equivalent-path buffers retain distinct buffer IDs when needed.
Middle-click a buffer entry to close that exact buffer. Vim refuses unsaved
changes, just as with the existing close button.

Switching buffers reveals the current entry automatically. Refreshing its
modified marker does not pull the list away from a manually browsed position.

Ctrl + wheel zooms by the supplied wheel distance; Ctrl + touchpad preserves
fractional motion at 40 logical pixels per font point. It does not round every
small touchpad event to a whole point. Alt/Super combinations and active IME
preedit keep their existing input route.

During fractional scrolling, cursor shapes retain their complete source-cell
geometry and are clipped at the viewport edge. A partially visible row no
longer compresses or relocates its horizontal caret. Mouse/IME coordinates
continue to use the visible clip.
Warm Pango layout lookups borrow the source text and reuse measured width
and baseline. Font-context changes retire both measurements and cached scenes.

## Native column highlights

`set cursorcolumn` and `set colorcolumn=80,120` remain native in supported
buffer views. Signed color columns follow Vim's `textwidth`; each split keeps
its own guides. Syntax, search, Visual selection, tabs, wide characters and
end-of-line whitespace retain Vim's source and highlight rules. Background
decorations do not become source text or intercept source-position hits.
Native undercurls keep a continuous phase across syntax runs, with each
split and completion popup retaining its own origin and clipping bounds.

## Plugin scratch buffers and per-window palettes

Ordinary `nofile`, `nowrite` and `acwrite` buffers now support native text,
source-pointer gestures and precision scrolling without changing Vim's buffer
type, write callbacks or readonly/modifiable options. Terminal and prompt
buffers retain compatibility rendering.

A local `wincolor` keeps its own resolved Normal palette across text, empty
rows, gutters and scroll margins. Native focus/caret fallbacks use that split's
palette; changing the palette retires old scroll imagery. General
`winhighlight` remapping remains a compatibility case.

## Help and error-list windows

Help, quickfix and location lists now use native text painting for supported
plain views. QuickFixLine follows the list's current entry even when the text
cursor moves independently; global and location-list state remains separate.
These windows keep Vim's own mouse, wheel and keyboard processing, including
Enter, Ctrl-] and double-click activation. The host does not add pixel-scroll
animation to a window that explicitly delegates input to Vim. File/scratch
splits alongside it retain their normal precision scrolling.

Default help concealment and actual wrapped lines still require compatible
display. User options and readonly/nomodifiable settings are preserved.

In an embedded Vim, font zoom shortcuts are dispatched to the focused editor
before the parent terminal's content-font shortcuts. Ctrl+wheel over the
editor buffer also stays with its native font control; sidebar and sibling
terminal zoom keep their own targets. The Editor font chooser updates both
the Vim terminal grid and the Pango buffer surface.

Mapped native dialogs own their keyboard input before workspace shortcuts.
Quick Open and font chords therefore stay inside a dialog's focused field.
Clicking the Vim body focuses it and immediately confirms the caret, even at
the same position; its original `guicursor` blink wait then starts again.

## Native visible whitespace

Common `listchars` markers retain native buffer painting. For example:

```vim
set list listchars=tab:»·,trail:·,nbsp:␣,eol:↲
```

Explicit two/three-glyph Tab markers, `space`, `lead`, `trail`, `nbsp` and
`eol` support single-cell symbols. Every marker keeps the original source
bytes for clicks, selection and saving; a line-end symbol is display-only.
Each Vim split retains its own options and GUI highlight colors.
Complex marker fields, default `^I` Tab geometry, active horizontal-edge
markers and wrapped EOL continuations retain compatibility rendering.

## Native end-of-line annotations

SimpleCC CodeLens (`:SimpleCCCodeLens`) and virtual diagnostics can keep the
native buffer surface when their end-of-line text properties use the supported
`text_align: 'after'` layout. Their GUI colors, CJK and combining characters
use the same native text painter as source code, clipped to the owning split.
Property updates and removal repaint without changing the buffer or requiring
a source edit.

Annotations carry no source byte range and remain outside saved text and undo.
Vim owns their lifetime and commands; use `:SimpleCCCodeLensRun` to execute a
lens supplied by the language server. Inline inlay hints, above/below/right
alignment and unsupported wrapping or clipping keep the explicit compatibility
view. No user display option is rewritten to force native rendering.
