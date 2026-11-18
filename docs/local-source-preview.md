# Local source preview (initial slice)

Ctrl+left-click an OSC8 local `file:///` link or an unambiguous absolute Unix
path (or a relative path such as `src/App.tsx` or a common source filename
such as `README.md` / `package.json`) in host-owned Block output to open the right inspector's **Local source
preview** page. No file is read by the click. Confirm **Read this machine's
local file** only when the displayed path belongs to this machine: terminal
output, including a `file:` URI, does not authenticate a local filesystem.

Relative paths require an explicitly typed absolute local directory in the sidebar.
Click **Prepare this local target** to freeze the complete path, then separately
confirm **Read this machine's local file**. No terminal-reported cwd,
remote URI, home expansion, or process cwd is used; parent components are rejected.
The directory is locked after preparation. Close and reopen the preview to select
a different directory.

The viewer shows bounded UTF-8 source text, including Markdown, TSX, HTML, and
SVG, without evaluating or rendering those formats. There is no shell, editor,
network fetch, or external opener. Binary images are not decoded in this slice.

Reads are Linux-only and require an absolute path with no parent (`..`) or
symlink components. A fixed descriptor is checked as a regular file before a
read open. Input is limited to 256 KiB, NUL and invalid UTF-8 are rejected, and
control/directional characters are shown as escapes. One reader may run at a
time; a stalled filesystem prevents another reader rather than creating an
unbounded queue. Closing or replacing the preview retires its result. This is
not a sandbox/root restriction or a guarantee that concurrently edited content
is an immutable snapshot.

Unknown live conventional VTE mouse ownership is left to VTE. Live Block output
is eligible only while its host reports mouse mode off and no alternate screen;
read-only finished output has no application mouse receiver. Existing OSC8/web
URLs, ordinary selection, and non-Ctrl clicks retain their existing routing.

Numeric `:line[:column]` suffixes remain part of the exact filename in this slice;
they are not stripped and do not imply source navigation. Non-numeric ambiguous
bare colon forms and ordinary prose are not treated as source filenames.

Not yet covered: automatic cwd inference, `:line` navigation, image
rendering, syntax highlighting, Markdown rendering, remote files, and a full
Codex GUI workflow. The initial source viewer does not imply these features are
implemented or that native pointer propagation has been validated locally.
