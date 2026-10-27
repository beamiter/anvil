
### Chat composer hierarchy and compact hints

The focused Chat composer paints a subtle accent wash behind its action strip,
without changing terminal geometry or replacing the existing keyboard focus ring.
Compact footer hints retain their full plain text in a tooltip when ellipsized.
The hint can shrink to one character in narrow panes; action buttons remain
separate. These changes do not alter shell input, IME ownership or submit/stop
state. Forge also exposes the same sanitized, bounded running-command summary
through a tooltip, clearing it when returning to the prompt.

Source contracts and formatting were checked locally. Existing native Chat
regressions cover tooltip synchronization; GTK execution and full application
compilation still require the graphical development environment.
