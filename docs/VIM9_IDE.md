# Native Vim9 IDE

The Vim9 workbench gives real Vim 9 a native GTK interface. Vim owns text,
normal/insert/visual modes, registers, macros, undo, buffers, splits, and your
existing Vim configuration. A dedicated VTE renders its interactive PTY; native
widgets communicate with a bundled Vim9script bridge through a private,
authenticated local connection.

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

## Workbench layout

| Area | Behavior |
| --- | --- |
| Project | Browse one directory at a time, filter filenames, open files, refresh, and move to the parent directory. Directory reads run off the GTK thread and are bounded to 1,024 entries. |
| Quick open | Ctrl+Shift+P opens a native fuzzy picker across the project. Type a filename or path, use ↑/↓ to select, and press Enter to open. Indexing runs in the background and reports progress, errors, and truncation. Refresh rebuilds the index. |
| Navigation history | Back/forward buttons and Alt+Left/Alt+Right move through the active Vim window's real jump list, restoring the file and cursor position. Native Go to line records a jump as well. Buttons reflect whether a direction is available. |
| Open buffers | Shows real Vim buffer names, current-buffer marker and modified marker. Activate a row or use previous/next controls to switch; Close current buffer preserves modified-buffer protection. Split vertically/horizontally creates real Vim windows. |
| Editor | Real Vim in VTE; normal Vim editing, mappings, mouse and input methods remain active. |
| Search project | Press Enter or Search to run a search with the selected case mode, literal/regex, whole-word and file-glob options. Stop cancels an active search. Requires `rg` in Vim's environment. Progress, errors, cancellations, match counts and omitted-result counts are visible below the build controls. |
| Problems | Displays filename, line, column, severity/type and message, and identifies the current quickfix list as Search, Build or Vim. Vim may also supply location-list or language-server diagnostics. Activate a result to open its exact location. F8/Shift+F8 navigate Vim diagnostics. |
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

## Shortcuts

| Scope | Shortcut | Action |
| --- | --- | --- |
| Terminal workspace | Ctrl+Alt+I | Open Vim9 IDE for the local pane |
| IDE | Ctrl+Shift+P | Quick open a project file with fuzzy path matching |
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
Vim PTY, fuzzy opening of a nested file, typed line/column navigation and
jump-history back/forward, native search case/regex/word/glob options, invalid
patterns and zero matches, real editor splits, protected buffer close, explicit
save-all-and-build, native save/build controls, diagnostics, and modified-buffer
close protection. Run it under a
graphical display (for example Xvfb) with the required Vim:

```sh
cargo test native_vim9_edit_save_build_and_modified_close -- --ignored --test-threads=1
```
