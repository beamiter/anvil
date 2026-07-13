from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


path = Path("src/block_view/css.rs")
text = path.read_text()
text = replace_once(
    text,
    '''/// Used by `update_input_height` to subtract this from the visible page size
/// before computing how many VTE rows fit. Must stay in sync with the
/// `.block-active` rule below; if the margin/border/padding here changes,
/// update this constant too.
pub(crate) const BLOCK_ACTIVE_VCHROME_PX: i32 = 14;
''',
    '''/// Used by the live-surface layout to subtract chrome from the visible page
/// before computing how many VTE rows fit. Keep both values in sync with the
/// normal and `.block-active.block-compact` rules below.
pub(crate) const BLOCK_ACTIVE_VCHROME_PX: i32 = 14;
/// Compact mode: 1px top/bottom margin + 1px top/bottom border, no padding.
pub(crate) const BLOCK_ACTIVE_COMPACT_VCHROME_PX: i32 = 4;
''',
    "add compact active chrome constant",
)
path.write_text(text)


path = Path("src/block_view/mod.rs")
text = path.read_text()
text = replace_once(
    text,
    '''    // .block-active wraps the VTE with margin+border+padding; subtract it from
    // page_size so the holder total fits. Running commands use this same row
    // count for their live active VTE, matching jterm1's block-mode behavior.
    let usable = (page - css::BLOCK_ACTIVE_VCHROME_PX).max(cell_h);
''',
    '''    // .block-active wraps the VTE with margin+border+padding; subtract the
    // chrome for the active density so the holder total fits exactly.
    let compact = vte
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
        .is_some_and(|holder| holder.has_css_class("block-compact"));
    let chrome = if compact {
        css::BLOCK_ACTIVE_COMPACT_VCHROME_PX
    } else {
        css::BLOCK_ACTIVE_VCHROME_PX
    };
    let usable = (page - chrome).max(cell_h);
''',
    "use compact-aware active chrome",
)
path.write_text(text)


path = Path("src/block_view/blocks.rs")
text = path.read_text()
text = replace_once(
    text,
    '''        // Mirrors create_finished_terminal's temporary capture budget. It is a
        // limit, not an eagerly allocated grid, and is removed after each feed.
''',
    '''        // Mirrors create_finished_terminal's capture budget. It is a limit,
        // not an eagerly allocated grid, and remains armed across re-feeds so an
        // older settling callback cannot invalidate a newer filtered render.
''',
    "refresh capture budget comment",
)
text = replace_once(
    text,
    '''/// other terminal semantics. The post-feed settling pass then expands the card to
/// the real buffer and removes that private scrollback.
''',
    '''/// other terminal semantics. The post-feed settling pass expands the card to
/// the real buffer while unused capture capacity remains only a safety limit.
''',
    "refresh finished render comment",
)
path.write_text(text)
