from pathlib import Path

path = Path("src/block_view/alt_screen.rs")
text = path.read_text()


def replace_once(old: str, new: str, label: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    text = text.replace(old, new, 1)


replace_once(
    "pub(crate) fn expand_finished_terminal_to_buffer(terminal: &Terminal, finalize: bool) {",
    "pub(crate) fn expand_finished_terminal_to_buffer(terminal: &Terminal) {",
    "remove finalize argument",
)

replace_once(
    """    if finalize {
        // Once all rows are part of the widget, no finished block should own a
        // private vertical scroll range. The outer block ScrolledWindow is the
        // one continuous history canvas, matching Warp's block interaction model.
        terminal.set_scrollback_lines(0);
    }

""",
    """    // Keep the capture capacity armed after expansion. The configured value is
    // only a limit, so unused rows do not create an inner scroll range. More
    // importantly, an older idle-settling callback can no longer clear the
    // scrollback needed by a newer filter render before VTE has processed it.

""",
    "keep capture capacity armed",
)

replace_once(
    """/// overflow/soft-wrapped rows into the widget, and the second removes the
/// temporary private scrollback once those rows are part of the card itself.
""",
    """/// overflow/soft-wrapped rows into the widget, and the second observes any
/// adjustment changes caused by that resize. Capture capacity remains armed so
/// overlapping filter renders cannot invalidate one another.
""",
    "update settling documentation",
)

replace_once(
    "expand_finished_terminal_to_buffer(&terminal, false);",
    "expand_finished_terminal_to_buffer(&terminal);",
    "update first settling call",
)
replace_once(
    "expand_finished_terminal_to_buffer(&terminal, true);",
    "expand_finished_terminal_to_buffer(&terminal);",
    "update second settling call",
)

path.write_text(text)
print("finished snapshot settling safety patch applied")
