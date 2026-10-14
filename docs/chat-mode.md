# Chat mode

Choose **Settings → Terminal Backend → Chat**, launch `anvil --mode chat`, or
set `terminal_mode = "chat"` in config.toml. This is an opt-in mode alongside
Block, VTE and Unified; old values and defaults remain valid. The preference
applies to new/restored local panes. Existing panes keep their selected mode
and running shell. A split inherits its source pane; managed remote panes
continue to use Block.

Chat presents completed shell commands as right-aligned accent capsules, with
real output in separate, wide left-hand cards. At an integrated prompt, the
same live terminal is docked below the independently scrolling transcript. Its
cursor, completion, shell history, readline editing, IME and multiline input
belong to the existing shell. The arrow sends one native Enter; it does not
copy, clear, re-paste, or execute command text through another process.

The optional local ASCII organism occupies existing collision-checked space
inside the live input surface. It uses observed outcomes and does not need an
AI provider, credentials, or a network connection. Theme and reduced-motion
preferences still apply. It is not part of terminal output or copied text.

While `cat`, `read`, `sudo`, or a REPL is running, the native terminal owns
stdin and appears as the active result; no second composer is available. A
fixed footer shows elapsed time and a Stop button. Stop sends the existing
Ctrl+C interrupt only to the same displayed command and foreground process
group, including across a held selection or long mouse press. Both controls
capture the intended command/prompt when a mouse press or native accessibility
activation begins; delayed or repeated callbacks cannot bind to a newer one.
Both optional
buttons fail closed when ordinary input is queued/in-flight, the PTY is busy,
or the kernel cannot accept the single byte immediately. They never queue or
retry an action for a later command. They also refuse enhanced-keyboard or
pending Alt/paste protocol state; the native keyboard still handles these
protocols intact. A successful write is not a claim that the process has ended;
the shell owns completion.
Alternate-screen programs such as Vim and less take over the pane. Missing
shell integration, reset, and between-boundary states use the native terminal
until a new valid prompt arrives. Selection, copy, export, bookmarks, restore
and search retain existing command/row identities and exclude UI chrome.

The parser, PTY and shell session are shared with Block. Ordinary keyboard
input keeps the existing FIFO admission gate; optional Chat controls use a
separate guarded immediate write on that same PTY. `pwd`, `cd`, environment changes and jobs all stay
in that one session. UI changes never restart a running shell.

## Verification checklist

Native GTK interaction must be checked separately from headless unit tests:

- Open simultaneous Chat and Block panes, change the preference, and confirm
  only newly opened/restored panes change. Save and reload `chat`.
- Run `pwd`, `cd /tmp`, `pwd`, environment assignments and background jobs.
- Try Tab completion, Up/Down, middle-of-line edits, IME, bracketed paste,
  multiline syntax, continuation prompts and a heredoc; each input goes once.
- Scroll history during output. Use `cat`/REPL and `read -s`; hidden input must
  not enter capsules or new history. Click send only at a ready prompt.
- Repeatedly enter/exit Vim and less, Ctrl-C, resize and use mouse reporting.
  The same PTY must survive, and the prompt composer must return. Grow/shrink
  window height with an idle resize loop and a full-height multiline editor.
- Interrupt a long-running command. Hold output across one command ending and
  another starting, or hold the Stop mouse press across that boundary; stale
  controls must not interrupt the new command. Alt-screen hides the footer.
- Search/copy/export known output; controls and companion art must be absent.
  Jump-to-live must return to the real terminal without stealing scrollback.
- Start without shell integration, then enable valid integration; native
  passthrough must recover to Chat without replaying input.
- Repeat with AI disabled and without provider credentials.

The explicit `scripts/test-gtk-display.sh` CI gate constructs real TermViews in
presented GTK windows, drives the production reader/parser with a nonexecuting
PTY recorder, verifies native-takeover columns before any window resize,
inspects kernel PTY winsize after height-only changes, and clicks
the production Send/Stop controls. It covers Closed/full-queue/in-flight refusal,
held-boundary replay, command/foreground identity, dock selection and background
focus. Headless transport tests separately cover kernel EAGAIN, full FIFO
ordering, and real shell foreground handover. Display tests require a working
display and session bus; a headless compile
or an ignored-test listing is not native interaction acceptance.
