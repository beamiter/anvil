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
network fetch, or external opener. PNG/JPEG files (.png/.jpg/.jpeg,
case-insensitive) are decoded on the single background worker and displayed as
an inert RGBA texture. SVG and HTML remain plain source text, never rendered.
Other image codecs are not supported.

Reads are Linux-only and require an absolute path with no parent (`..`) or
symlink components. A fixed descriptor is checked as a regular file before a
read open. Text input is limited to 256 KiB; NUL and invalid UTF-8 are rejected, and
control/directional characters are shown as escapes. One reader may run at a
time; a stalled filesystem prevents another reader rather than creating an
unbounded queue. Closing or replacing the preview retires its result. This is
not a sandbox/root restriction or a guarantee that concurrently edited content
is an immutable snapshot.

Unknown live conventional VTE mouse ownership is left to VTE. Live Block output
is eligible only while its host reports mouse mode off and no alternate screen;
read-only finished output has no application mouse receiver. Existing OSC8/web
URLs, ordinary selection, and non-Ctrl clicks retain their existing routing.

Numeric `:line[:column]` suffixes are tried as the exact literal filename first.
The confirmation panel discloses both candidates before reading; only a definite
path-lookup NotFound permits the displayed suffix-free fallback. Permission,
symlink, decoding, and post-open errors never fall back. The result names the
file actually read. Text fallback navigates to the requested one-based line if
present; columns are informational because source tools may use different units
and the viewer escapes controls. Out-of-range lines are not clamped. Non-numeric
ambiguous bare colon forms and ordinary prose remain excluded.

Not yet covered: automatic cwd inference, precise column navigation, other image codecs,
syntax highlighting, Markdown rendering, remote files, and a full
Codex GUI workflow. The initial source viewer does not imply these features are
implemented or that native pointer propagation has been validated locally.

## Image resource bounds

PNG/JPEG encoded input is limited to 4 MiB. Dimensions must be nonzero, at most
4096 pixels per edge, and at most 4,194,304 pixels before full decode. The codec
must agree on dimensions and produce exactly width × height × 4 RGBA bytes.
A 32 MiB best-effort decoder allocation limit is configured, not a strict total
memory guarantee; image/codec implementations (notably JPEG) may not enforce
all allocation limits. Conversion and GTK textures allocate additional memory.
One worker runs at a time; there is no hard CPU deadline, process RSS ceiling,
or isolated decoder sandbox. Native GTK image display remains unverified locally.

## Wrapped references and copying

Physical-row width and a trailing hyphen do not establish a soft wrap. Anvil
therefore reserves, but does not activate, parenthesized local references across
multiple extracted VTE rows. Whitespace is not trimmed into a different filename,
and the reserved region cannot fall back to a partial local/URL match. Exact
single-row paths containing spaces remain supported, after the same local-machine
confirmation. Explicit OSC 8 retains priority. Ordinary single-row web URLs retain the native matcher and URL safety policy.

Copy returns VTE's native selection verbatim; Anvil no longer trims or joins
hard-line fragments. VTE remains responsible for actual soft-wrap selection. The
current adapter exposes no reliable soft-wrap provenance, so hard-wrapped TUI
paths require an unambiguous single-row reference instead. Pure double-file tests
are not desktop acceptance; the GTK fixture separately checks blocked hard-line
hits, exact single-line hits, release/drag handling and native selection equality.

Cross-row parenthesized web destinations are now also reserved but blocked:
trimming `https://safe. ` plus ` example/path` would synthesize a different
authority. Full-looking rows are not soft-wrap evidence. No URL is opened or
rewritten from such ambiguous fragments; the reserved region suppresses native
regex fallback. Single-row URLs and explicit OSC 8 keep priority. Copy remains
verbatim VTE selection. A dedicated mapped VTE fixture checks the production URL
activation lookup, both blocked rows, exact copy, a single-row positive control,
and OSC 8 priority; it never launches a browser or claims physical-device coverage.

CRLF source line endings are displayed as ordinary line breaks. This only
normalizes the read-only presentation; source bytes and line numbers are unchanged.
A lone carriage return, ESC, and bidi controls remain visible escapes.
