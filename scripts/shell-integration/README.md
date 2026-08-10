# anvil shell integration

Source the file matching your shell from its rc file:

| Shell      | File           | Source from |
|------------|----------------|-------------|
| bash       | `anvil.bash`  | `~/.bashrc` |
| zsh        | `anvil.zsh`   | `~/.zshrc`  |
| fish       | `anvil.fish`  | `~/.config/fish/config.fish` |
| PowerShell | `anvil.ps1`   | `$PROFILE`  |

Example (bash):

```bash
[[ $TERM_PROGRAM == anvil ]] && source /path/to/anvil.bash
```

Example (PowerShell):

```powershell
if ($env:TERM_PROGRAM -eq 'anvil') { . /path/to/anvil.ps1 }
```

The PowerShell script requires PSReadLine (bundled with pwsh 7+; preinstalled
on Windows PowerShell 5+). Without it OSC 133 ;C is not emitted on Enter — the
prompt-side markers still fire, so blocks are still demarcated but exit codes
attach only at the *next* prompt.

## What it provides

Each script emits two escape sequence families that anvil parses to drive its
block view (`src/terminal/ansi.rs`):

- **OSC 133 (FTCS)** — `;A` at prompt render, `;B` when prompt finishes,
  `;C;id=<id>` when a command starts executing, and
  `;D;<exit>;id=<id>` when it returns. For direct local bash/zsh Block panes,
  anvil delivers a 128-bit token through a one-shot inherited pipe descriptor;
  the integration consumes and closes that descriptor before user commands,
  announces the matching capability as OSC 7771 inside the prompt boundary,
  and derives each C/D ID from the token plus a monotonic sequence. Fish and
  PowerShell retain ordinary per-shell correlation but do not advertise Agent
  execution capability. The matching ID prevents a stale or unrelated
  completion marker from finishing the wrong command. This lets
  anvil attribute output to discrete blocks and read the exit code exactly
  (no error-text heuristics). It also binds an approved Agent proposal to the
  exact prompt generation and command that actually started; a changed prompt,
  failed write, mismatched start, or mismatched completion fails closed.

  The private descriptor is intentionally unavailable across remote-command
  wrappers and the Flatpak host bridge. Blocks continue to work there, but
  automatic Agent execution stays disabled because Anvil cannot prove an
  equivalent descriptor/foreground-process boundary.

- **OSC 7** — reports the current working directory as a `file://` URI so the
  active prompt chip stays in sync with `cd`.

The sequences are silently ignored by terminals that don't understand them,
so it is safe to source unconditionally.
