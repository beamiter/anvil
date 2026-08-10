//! css — extracted from block_view (mechanical split, no logic changes)
use crate::config::Config;
use gtk::gdk::RGBA;
use relm4::gtk;
use std::cell::RefCell;
use std::io::Read;
use std::path::Path;

const MAX_GIT_POINTER_BYTES: u64 = 16 * 1024;
const MAX_BRANCH_DISPLAY_CHARS: usize = 256;

/// Vertical chrome the `.block-active` holder adds around the live VTE:
/// 4px top margin + 4px bottom margin + 1px top border + 1px bottom border +
/// 2px top padding + 2px bottom padding = 14px.
///
/// Used by the live-surface layout to subtract chrome from the visible page
/// before computing how many VTE rows fit. Keep both values in sync with the
/// normal and `.block-active.block-compact` rules below.
pub(crate) const BLOCK_ACTIVE_VCHROME_PX: i32 = 14;
/// Compact mode: 1px top/bottom margin + 1px top/bottom border, no padding.
pub(crate) const BLOCK_ACTIVE_COMPACT_VCHROME_PX: i32 = 4;

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

pub(crate) fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    let parts: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        display
    } else {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    }
}

/// Cheap git-branch lookup for the context chip: walk up from `cwd` to find a
/// `.git` dir (or `.git` file for worktrees/submodules), then read `HEAD`. No
/// subprocess, no dirty-state — just the branch name (or short SHA if detached).
pub(crate) fn git_branch_for(cwd: &str) -> Option<String> {
    use std::path::PathBuf;
    let mut dir: Option<&Path> = Some(Path::new(cwd));
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        let head_path: Option<PathBuf> = if dot_git.is_dir() {
            Some(dot_git.join("HEAD"))
        } else if dot_git.is_file() {
            // "gitdir: <path>" → real git dir lives elsewhere
            read_small_git_file(&dot_git).and_then(|c| {
                c.strip_prefix("gitdir:").map(|p| {
                    let g = Path::new(p.trim());
                    if g.is_absolute() {
                        g.join("HEAD")
                    } else {
                        d.join(g).join("HEAD")
                    }
                })
            })
        } else {
            None
        };
        if let Some(hp) = head_path {
            if let Some(head) = read_small_git_file(&hp) {
                let head = head.trim();
                if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
                    return sanitize_branch(branch);
                }
                // Detached HEAD: show short SHA.
                if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(head[..7].to_string());
                }
                return None;
            }
        }
        dir = d.parent();
    }
    None
}

fn read_small_git_file(path: &Path) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_GIT_POINTER_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GIT_POINTER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_GIT_POINTER_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn sanitize_branch(branch: &str) -> Option<String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut chars = branch.chars();
    for ch in chars.by_ref().take(MAX_BRANCH_DISPLAY_CHARS) {
        if ch.is_control() || crate::text_safety::is_visual_spoof(ch) {
            output.push('\u{fffd}');
        } else {
            output.push(ch);
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    Some(output)
}

pub(crate) fn chrono_local_offset_secs() -> i64 {
    use nix::libc;
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

// ─── CSS ──────────────────────────────────────────────────────────────────────

pub(crate) fn install_block_css(config: &Config) {
    let fg = &config.foreground;
    let bg = &config.background;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    // Accent color for active chevron (use palette color 2 = green-ish)
    let accent = rgba_to_hex(&config.palette[2]);
    // Error color for bad exit codes — use the theme's red (palette 1) so it
    // matches what VTE would render, instead of a hard-coded swatch.
    let err = &config.palette[1];
    let err_hex = rgba_to_hex(err);
    let err_bg = format!(
        "rgba({},{},{},0.18)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );

    // Status-stripe colors derived from the theme palette: green (palette 2) for
    // success, red (palette 1) for failure. Kept semi-transparent so the stripe
    // reads as an accent rather than a hard bar.
    let ok = &config.palette[2];
    let ok_stripe = format!(
        "rgba({},{},{},0.55)",
        (ok.red() * 255.0) as u8,
        (ok.green() * 255.0) as u8,
        (ok.blue() * 255.0) as u8,
    );
    let ok_hex = rgba_to_hex(ok);
    let err_stripe = format!(
        "rgba({},{},{},0.70)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );
    // Cyan distinguishes asynchronous/background output from command success.
    let async_color = &config.palette[6];
    let async_hex = rgba_to_hex(async_color);
    let async_r = (async_color.red() * 255.0) as u8;
    let async_g = (async_color.green() * 255.0) as u8;
    let async_b = (async_color.blue() * 255.0) as u8;
    let async_stripe = format!("rgba({async_r},{async_g},{async_b},0.65)");

    // Per-channel components for the success/error/accent colors, used to build
    // tinted backgrounds and focus glows directly in the CSS template.
    let ok_r = (ok.red() * 255.0) as u8;
    let ok_g = (ok.green() * 255.0) as u8;
    let ok_b = (ok.blue() * 255.0) as u8;
    let err_r = (err.red() * 255.0) as u8;
    let err_g = (err.green() * 255.0) as u8;
    let err_b = (err.blue() * 255.0) as u8;
    // Accent == palette[2] (same green as success); reused for the active-card
    // focus ring and prompt chevron.
    let acc = &config.palette[2];
    let acc_r = (acc.red() * 255.0) as u8;
    let acc_g = (acc.green() * 255.0) as u8;
    let acc_b = (acc.blue() * 255.0) as u8;

    // Unknown outcomes and the organism's caution state use theme yellow.
    let warn = &config.palette[3];
    let warn_hex = rgba_to_hex(warn);
    let warn_r = (warn.red() * 255.0) as u8;
    let warn_g = (warn.green() * 255.0) as u8;
    let warn_b = (warn.blue() * 255.0) as u8;
    let warn_stripe = format!("rgba({warn_r},{warn_g},{warn_b},0.62)");

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    // Shell Agent cards use the theme's blue so they remain distinct from
    // success/correction accents (palette 2).
    let agent = &config.palette[4];
    let agent_hex = rgba_to_hex(agent);
    let agent_r = (agent.red() * 255.0) as u8;
    let agent_g = (agent.green() * 255.0) as u8;
    let agent_b = (agent.blue() * 255.0) as u8;

    // Slightly different background for finished blocks (3% toward fg)
    let bg_r = (bg.red() * 255.0) as u8;
    let bg_g = (bg.green() * 255.0) as u8;
    let bg_b = (bg.blue() * 255.0) as u8;
    let block_bg_hex = format!(
        "#{:02x}{:02x}{:02x}",
        (bg_r as f32 + (fg_r as f32 - bg_r as f32) * 0.03) as u8,
        (bg_g as f32 + (fg_g as f32 - bg_g as f32) * 0.03) as u8,
        (bg_b as f32 + (fg_b as f32 - bg_b as f32) * 0.03) as u8,
    );

    // Parse font description to extract font family and size
    // Format: "FontName Style Size" e.g. "SauceCodePro Nerd Font Mono 14"
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (font_family, base_size) = if parts.len() >= 2 {
        // Last part is usually the size. Pango allows float sizes ("Fira Code 12.5"),
        // so parse as f64 and round rather than rejecting non-integer sizes.
        if let Ok(size) = parts[parts.len() - 1].parse::<f64>() {
            let family = parts[..parts.len() - 1].join(" ");
            (family, size.round().max(1.0) as i32)
        } else {
            (config.font_desc.clone(), 14)
        }
    } else {
        (config.font_desc.clone(), 14)
    };
    // Escape the family name so a quote/backslash in the font name can't break the
    // surrounding CSS string and silently disable the whole stylesheet.
    let font_family = font_family.replace('\\', "\\\\").replace('"', "\\\"");

    // Apply font scale to the base size
    let scaled_size = (base_size as f64 * config.default_font_scale)
        .round()
        .max(1.0) as i32;
    let font_size = format!("{}pt", scaled_size);

    let css = format!(
        r#"
        .block-scroll {{
            background-color: {bg_hex};
        }}
        .block-failure-markers {{
            color: rgba({err_r},{err_g},{err_b},0.90);
        }}
        .block-list {{
            background-color: {bg_hex};
        }}
        .block-finished {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.08);
            border-left: 3px solid transparent;
            border-radius: 10px;
            background-color: {block_bg_hex};
            min-height: 40px;
            transition: background-color 140ms ease, border-color 140ms ease, box-shadow 140ms ease;
        }}
        .block-finished.block-compact {{
            border-radius: 6px;
            min-height: 32px;
            box-shadow: none;
        }}
        .block-success {{
            border-left-color: {ok_stripe};
        }}
        .block-failed {{
            border-left-color: {err_stripe};
            background-color: rgba({err_r},{err_g},{err_b},0.11);
            box-shadow: inset 2px 0 0 0 {err_stripe};
        }}
        .block-background {{
            border-left-color: {async_stripe};
            background-color: rgba({async_r},{async_g},{async_b},0.07);
            box-shadow: inset 2px 0 0 0 {async_stripe};
        }}
        .block-correction, .command-suggestion, .command-review-standalone {{
            border-left-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            background-color: rgba({acc_r},{acc_g},{acc_b},0.05);
        }}
        .block-agent {{
            border-left-color: rgba({agent_r},{agent_g},{agent_b},0.85);
            background-color: rgba({agent_r},{agent_g},{agent_b},0.05);
        }}
        .block-organism {{
            border-left-color: rgba({agent_r},{agent_g},{agent_b},0.70);
            background-color: rgba({agent_r},{agent_g},{agent_b},0.035);
        }}
        .block-organism.organism-active {{
            border-left-color: rgba({acc_r},{acc_g},{acc_b},0.90);
            background-color: rgba({acc_r},{acc_g},{acc_b},0.07);
        }}
        .block-organism.organism-success {{
            border-left-color: {ok_stripe};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.08);
        }}
        .block-organism.organism-error {{
            border-left-color: {err_stripe};
            background-color: rgba({err_r},{err_g},{err_b},0.10);
        }}
        .block-organism.organism-warning {{
            border-left-color: {warn_stripe};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.07);
        }}
        .organism-sprite {{
            color: {agent_hex};
            font-family: "{font_family}";
            font-weight: bold;
        }}
        .organism-live-body {{
            color: {agent_hex};
            background-color: rgba({bg_r},{bg_g},{bg_b},0.80);
            border: 1px solid rgba({agent_r},{agent_g},{agent_b},0.32);
            border-radius: 6px;
            padding: 3px 6px;
            font-family: "{font_family}";
            font-size: {font_size};
            font-weight: bold;
        }}
        .organism-live-body.organism-active {{
            color: {accent};
            border-color: rgba({acc_r},{acc_g},{acc_b},0.50);
        }}
        .organism-live-body.organism-success {{
            color: {ok_hex};
            border-color: rgba({ok_r},{ok_g},{ok_b},0.50);
        }}
        .organism-live-body.organism-error {{
            color: {err_hex};
            border-color: rgba({err_r},{err_g},{err_b},0.55);
        }}
        .organism-live-body.organism-warning {{
            color: {warn_hex};
            border-color: rgba({warn_r},{warn_g},{warn_b},0.50);
        }}
        .organism-sticky-avatar {{
            color: {agent_hex};
            font-family: "{font_family}";
            font-weight: bold;
            margin-right: 6px;
        }}
        .organism-sticky-avatar.organism-error {{ color: {err_hex}; }}
        .organism-sticky-avatar.organism-success {{ color: {ok_hex}; }}
        .organism-sticky-avatar.organism-warning {{ color: {warn_hex}; }}
        .organism-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .organism-badge, .organism-state {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.82em;
        }}
        .organism-status {{ color: {fg_hex}; }}
        .organism-error .organism-status {{ color: {err_hex}; }}
        .organism-success .organism-status {{ color: {ok_hex}; }}
        .agent-card-icon {{
            color: {agent_hex};
            font-family: "{font_family}";
        }}
        .agent-card-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .agent-card-binding {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .agent-context-card {{
            padding: 8px 10px;
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.045);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 9px;
        }}
        .agent-dashboard {{
            color: {fg_hex};
            background-color: {bg_hex};
        }}
        .agent-overview, .agent-setting-card, .agent-status-card,
        .agent-composer, .agent-transcript-card {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.055);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.14);
            border-radius: 10px;
        }}
        .agent-overview {{
            padding: 12px;
        }}
        .agent-chip {{
            color: rgba({fg_r},{fg_g},{fg_b},0.78);
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 999px;
            padding: 4px 9px;
            font-size: 0.82em;
        }}
        .agent-safety-chip {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.14);
        }}
        .agent-setting-card {{
            padding: 10px 12px;
        }}
        .agent-status-card {{
            padding: 9px 11px;
        }}
        .agent-status {{
            color: rgba({fg_r},{fg_g},{fg_b},0.78);
        }}
        .agent-status-card progressbar trough {{
            min-height: 4px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.10);
            border-radius: 999px;
        }}
        .agent-status-card progressbar progress {{
            min-height: 4px;
            background-color: {agent_hex};
            border-radius: 999px;
        }}
        .agent-prompt-status {{
            border-radius: 999px;
            padding: 2px 7px;
            font-size: 0.82em;
        }}
        .agent-prompt-status.agent-prompt-ready {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.10);
        }}
        .agent-prompt-status.agent-prompt-blocked {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.10);
        }}
        .assistant-card-icon {{
            color: {accent};
            font-family: "{font_family}";
        }}
        .assistant-card-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .assistant-card-badge {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .assistant-context-chip {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.055);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 8px;
            padding: 5px 8px;
            font-size: 0.88em;
        }}
        .assistant-status-row {{
            padding: 5px 0;
        }}
        .assistant-status {{
            color: {dim_fg};
        }}
        .command-review-embedded {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.045);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.34);
            border-radius: 9px;
        }}
        .command-review-description {{
            color: {fg_hex};
        }}
        .command-review-risk {{
            color: {dim_fg};
            font-size: 0.9em;
        }}
        .command-review-risk.error, .command-review-feedback.error {{
            color: {err_hex};
        }}
        .command-review-feedback {{
            color: {dim_fg};
            font-size: 0.9em;
        }}
        .command-review-entry {{
            font-family: "{font_family}";
            font-size: {font_size};
            color: {fg_hex};
            background-color: {bg_hex};
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.18);
            border-radius: 6px;
            padding: 4px 8px;
        }}
        .command-review-entry:focus {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.75);
        }}
        .command-review-actions {{
            margin-top: 2px;
        }}
        .command-review-secondary {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-color: rgba({fg_r},{fg_g},{fg_b},0.16);
        }}
        .command-review-secondary:hover {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.16);
        }}
        .agent-msg-body {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
        }}
        .agent-msg-error {{
            color: {err_hex};
        }}
        .agent-proposal-card {{
            padding: 10px;
            color: {fg_hex};
            background-color: {bg_hex};
            border: 1px solid rgba({agent_r},{agent_g},{agent_b},0.48);
            border-radius: 10px;
        }}
        .agent-danger-command {{
            padding: 8px;
            font-family: "{font_family}";
            background-color: rgba({err_r},{err_g},{err_b},0.16);
            border-radius: 7px;
        }}
        .agent-composer {{
            padding: 9px;
        }}
        .agent-input {{
            min-height: 34px;
            color: {fg_hex};
            caret-color: {fg_hex};
            background-color: rgba({bg_r},{bg_g},{bg_b},0.62);
            border-color: rgba({fg_r},{fg_g},{fg_b},0.20);
        }}
        .agent-input text {{
            color: {fg_hex};
            caret-color: {fg_hex};
        }}
        .agent-turn-label {{
            color: rgba({fg_r},{fg_g},{fg_b},0.68);
        }}
        .agent-send {{
            min-width: 72px;
            min-height: 34px;
        }}
        .agent-input-hint {{
            color: rgba({fg_r},{fg_g},{fg_b},0.58);
            font-size: 0.82em;
        }}
        .agent-section-label {{
            color: rgba({fg_r},{fg_g},{fg_b},0.62);
            font-size: 0.80em;
            font-weight: bold;
            padding: 7px 9px 5px 9px;
        }}
        /* The shell reported no status. Deliberately not tinted like a failure
           and not striped like a success: nothing is known about the outcome. */
        .block-unknown {{
            border-left-color: rgba({fg_r},{fg_g},{fg_b},0.35);
        }}
        .block-hovered {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.05);
            border-top-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            border-right-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            border-bottom-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            box-shadow: 0 4px 14px rgba(0,0,0,0.22);
        }}
        .block-selected {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.08);
            border-color: rgba({acc_r},{acc_g},{acc_b},0.48);
            box-shadow: inset 0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.65);
        }}
        .block-selected.block-selection-active {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.14);
            border-color: rgba({acc_r},{acc_g},{acc_b},0.92);
            box-shadow: inset 0 0 0 2px {accent}, 0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.55);
        }}
        .block-active {{
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.32);
            border-left: 3px solid rgba({acc_r},{acc_g},{acc_b},0.85);
            border-radius: 10px;
            margin: 4px 8px;
            padding: 2px 0;
            background-color: {bg_hex};
            box-shadow: 0 2px 8px rgba(0,0,0,0.18);
        }}
        .block-active.block-compact {{
            border-radius: 6px;
            margin: 1px 4px;
            padding: 0;
            box-shadow: none;
        }}
        .block-output-scrollbar {{
            min-width: 10px;
            margin: 1px 3px 1px 1px;
            padding: 0;
            background-color: transparent;
        }}
        .block-output-scrollbar trough {{
            min-width: 8px;
            border-radius: 4px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.06);
        }}
        .block-output-scrollbar slider {{
            min-width: 6px;
            border-radius: 4px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.38);
        }}
        .block-output-scrollbar slider:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.78);
        }}
        .block-prompt-chevron {{
            color: {accent};
            font-family: "{font_family}";
            font-size: {font_size};
            font-weight: bold;
            margin-left: 10px;
            margin-right: 6px;
        }}
        .block-chip {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.07);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.10);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-bookmark-star {{
            color: #e5c07b;
            font-family: "{font_family}";
            font-size: 0.82em;
            margin-right: 2px;
        }}
        .block-bookmarked {{
            box-shadow: inset 3px 0 0 0 #e5c07b;
        }}
        .block-chip-git {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.22);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-status-ok {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.16);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-status-bad {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.18);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-status-background {{
            color: {async_hex};
            background-color: rgba({async_r},{async_g},{async_b},0.16);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-status-unknown {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-background-chip {{
            color: {async_hex};
            background-color: rgba({async_r},{async_g},{async_b},0.12);
            border: 1px solid rgba({async_r},{async_g},{async_b},0.28);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-action-btn {{
            color: {dim_fg};
            min-width: 24px;
            min-height: 24px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.9em;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-action-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-action-active {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.18);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.34);
        }}
        .block-filter-row {{
            padding: 2px 0;
        }}
        .block-filter-toggle {{
            color: {dim_fg};
            min-width: 26px;
            min-height: 24px;
            padding: 0 4px;
            border-radius: 6px;
            font-family: "{font_family}";
            font-size: 0.8em;
        }}
        .block-filter-toggle:checked {{
            color: {fg_hex};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.35);
        }}
        .block-filter-status {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 0 6px;
        }}
        .block-filter-empty {{
            color: {err_hex};
        }}
        .block-header {{
            border-radius: 6px 6px 0 0;
        }}
        .block-header-label {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .block-collapse-btn {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.8em;
            min-width: 24px;
            min-height: 24px;
            padding: 0;
            border-radius: 999px;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-collapse-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-output-summary {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.82em;
            padding: 2px 4px;
            border-radius: 5px;
        }}
        .block-output-summary:hover {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.12);
        }}
        .block-prompt {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: {font_size};
            line-height: 1.0;
            margin: 0;
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        .block-cmd-active text {{
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        .block-cmd-active text selection {{
            background-color: transparent;
        }}
        .block-cmd-finished {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
        }}
        .block-cmd-finished text {{
            background-color: {bg_hex};
        }}
        .block-exit-bad {{
            color: {err_hex};
            background-color: {err_bg};
            border: 1px solid rgba({err_r},{err_g},{err_b},0.35);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            font-weight: bold;
            padding: 1px 8px;
        }}
        .block-meta-badge {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 8px;
        }}
        .block-running-label {{
            color: {dim_fg};
            font-size: 0.85em;
            padding-right: 8px;
        }}
        .block-output {{
            background-color: {bg_hex};
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            min-height: 0;
            line-height: 1.0;
            padding: 0;
            margin: 0;
        }}
        .block-output-static {{
            padding-left: 0;
            padding-right: 0;
        }}
        .block-show-more {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.25);
            border-radius: 999px;
            margin-left: 12px;
            margin-top: 6px;
            margin-bottom: 4px;
            font-size: 0.82em;
            padding: 2px 12px;
            transition: background-color 120ms ease;
        }}
        .block-show-more:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.18);
        }}
        .jump-bottom-fab {{
            color: {bg_hex};
            background-color: {accent};
            background-image: none;
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.55);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
            min-width: 18px;
            min-height: 18px;
            padding: 6px 12px;
            box-shadow: 0 4px 14px rgba(0,0,0,0.35);
            transition: background-color 120ms ease, box-shadow 120ms ease;
        }}
        .jump-bottom-fab:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            box-shadow: 0 6px 18px rgba(0,0,0,0.45);
        }}
        .sticky-running-header {{
            background-color: {block_bg_hex};
            border-bottom: 1px solid rgba({acc_r},{acc_g},{acc_b},0.45);
            box-shadow: 0 3px 10px rgba(0,0,0,0.30);
            padding: 6px 14px;
        }}
        .sticky-running-label {{
            color: {accent};
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
        }}
        .sticky-header-control {{
            color: {dim_fg};
            min-width: 22px;
            min-height: 22px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.82em;
        }}
        .sticky-header-control:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .sticky-running-header.sticky-minimized {{
            padding: 2px 8px;
            background-color: rgba({bg_r},{bg_g},{bg_b},0.92);
            box-shadow: 0 1px 4px rgba(0,0,0,0.24);
        }}
        .feed-hold-badge {{
            color: {bg_hex};
            background-color: {accent};
            background-image: none;
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.55);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.85em;
            font-weight: bold;
            padding: 4px 12px;
            box-shadow: 0 4px 14px rgba(0,0,0,0.35);
        }}
        .command-palette > contents {{
            background-color: {block_bg_hex};
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.45);
            border-radius: 10px;
            padding: 10px;
            box-shadow: 0 10px 30px rgba(0,0,0,0.45);
        }}
        .command-palette-list {{
            background-color: transparent;
        }}
        .command-palette-list row {{
            padding: 0;
            border-radius: 6px;
        }}
        .command-palette-list row:selected {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.28);
        }}
        .command-palette-row {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: 0.92em;
            padding: 6px 10px;
        }}
        "#,
    );

    thread_local! {
        static BLOCK_CSS_PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    }

    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    let Some(display) = gtk::gdk::Display::default() else {
        // No display (headless / CI). Nothing to style.
        return;
    };

    BLOCK_CSS_PROVIDER.with(|cell| {
        let mut prev = cell.borrow_mut();
        if let Some(old) = prev.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *prev = Some(provider);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("anvil-git-chip-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn git_branch_is_bounded_and_visual_controls_are_made_visible() {
        let root = test_root("branch");
        let repo = root.join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let branch = format!(
            "safe\u{202e}\u{200b}{}",
            "界".repeat(MAX_BRANCH_DISPLAY_CHARS + 10)
        );
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();

        let display = git_branch_for(repo.to_str().unwrap()).unwrap();
        assert!(!display.contains('\u{202e}'));
        assert!(!display.contains('\u{200b}'));
        assert!(display.contains("��"));
        assert!(display.ends_with('…'));
        assert_eq!(display.chars().count(), MAX_BRANCH_DISPLAY_CHARS + 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_worktree_pointer_is_bounded_and_resolves_relative_path() {
        let root = test_root("worktree");
        let repo = root.join("repo");
        let real = root.join("real-git");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&real).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: ../real-git\n").unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/worktree\n").unwrap();
        assert_eq!(
            git_branch_for(repo.to_str().unwrap()).as_deref(),
            Some("worktree")
        );

        let oversized = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(repo.join(".git"))
            .unwrap();
        oversized.set_len(MAX_GIT_POINTER_BYTES + 1).unwrap();
        assert!(git_branch_for(repo.to_str().unwrap()).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn git_head_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = test_root("fifo");
        let repo = root.join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let head = git.join("HEAD");
        let head_c = CString::new(head.as_os_str().as_bytes()).unwrap();
        // SAFETY: head_c is a live NUL-terminated pathname for this call.
        assert_eq!(unsafe { nix::libc::mkfifo(head_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(git_branch_for(repo.to_str().unwrap()).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(root).unwrap();
    }
}
