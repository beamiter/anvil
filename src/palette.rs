//! Command palette: fuzzy-ranked search over multiple sources (actions, shell
//! history), with prefix-driven filters (`>` commands, `@` history).
//!
//! The UI lives in the `dialogs::command_palette` Relm4 component — this
//! module is the pure data + ranking layer so it can be tested independently
//! and reused by other surfaces such as the inline Ctrl-R popover.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::command_history;
use crate::keybindings::{Action, KeybindingMap};
use crate::workflows::Workflow;

/// Which sources the palette will draw from. The mode is the *default* — the
/// user can still narrow further with a prefix in the query text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteMode {
    /// Everything: actions + history.
    All,
    /// Only registered actions.
    Commands,
    /// Only shell history.
    History,
    /// `?` prefix: AI natural-language → shell command. The remaining text
    /// becomes the user prompt; gather returns a single "Ask AI" entry.
    Ai,
    /// `:` prefix: parameterised command templates ("workflows").
    Workflows,
}

/// Parsed query: a mode (possibly tightened by a prefix) and the remaining
/// text used as the fuzzy needle.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    pub mode: PaletteMode,
    pub text: String,
}

impl Query {
    /// `>foo` forces command-only, `@foo` forces history-only, `?foo` forces
    /// AI natural-language → command, `:foo` forces workflows-only. Otherwise
    /// the query inherits `default_mode`.
    pub fn parse(raw: &str, default_mode: PaletteMode) -> Self {
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix('>') {
            return Query {
                mode: PaletteMode::Commands,
                text: rest.trim_start().to_string(),
            };
        }
        if let Some(rest) = trimmed.strip_prefix('@') {
            return Query {
                mode: PaletteMode::History,
                text: rest.trim_start().to_string(),
            };
        }
        if let Some(rest) = trimmed.strip_prefix('?') {
            return Query {
                mode: PaletteMode::Ai,
                text: rest.trim_start().to_string(),
            };
        }
        if let Some(rest) = trimmed.strip_prefix(':') {
            return Query {
                mode: PaletteMode::Workflows,
                text: rest.trim_start().to_string(),
            };
        }
        Query {
            mode: default_mode,
            text: trimmed.to_string(),
        }
    }
}

/// What happens when the user activates an entry.
#[derive(Debug, Clone)]
pub(crate) enum Accept {
    /// Dispatch a built-in action.
    Action(Action),
    /// Type the command into the active pane without submitting (user can edit
    /// then press Enter). Safest default for history.
    TypeCommand(String),
    /// Forward the natural-language query to the AI bridge. The main loop
    /// fires the request, then types the returned command into the active
    /// pane (no autosubmit — same safety stance as TypeCommand).
    AskAi(String),
    /// Run the workflow whose source path is given. Index into the workflow
    /// list isn't used because the list can be reloaded between gather and
    /// accept; the source path is stable enough to re-lookup.
    RunWorkflow(std::path::PathBuf),
}

/// One row in the palette.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// Coarse priority bucket (lower = higher). Actions sit above history so
    /// "git" returns the binding for "Toggle git pane" before any past `git`
    /// invocations.
    pub tier: u8,
    /// Skim score, populated by [`gather`]. Higher = better.
    pub score: i64,
    pub label: String,
    pub sublabel: Option<String>,
    /// Right-aligned hint, e.g. the keybinding for an action or the cwd for a
    /// history entry.
    pub right: Option<String>,
    pub accept: Accept,
}

const HISTORY_SNAPSHOT_LIMIT: usize = 2_000;
const HISTORY_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HISTORY_RECORD_BYTES: usize = 1024 * 1024;
const MAX_PALETTE_COMMAND_BYTES: usize = 256 * 1024;
const MAX_PALETTE_METADATA_DISPLAY_BYTES: usize = 4 * 1024;

/// Read up to `max` newest-first records from a JSONL history file.
///
/// Non-interactive consumers such as the AI context builder can request their
/// own small bound directly. Interactive palettes should use
/// [`load_history_snapshot`] once per opening and filter that snapshot in
/// memory.
pub(crate) fn read_history(path: &Path, max: usize) -> Vec<command_history::CommandHistoryRecord> {
    read_history_checked(path, max.min(HISTORY_SNAPSHOT_LIMIT)).unwrap_or_default()
}

/// Read the bounded JSONL tail through a no-follow, nonblocking descriptor.
/// The currently pinned core predates these inode checks, and calling its
/// pathname-based reader directly from GTK would let a replaced FIFO freeze
/// the main thread while opening a palette.
fn read_history_checked(
    path: &Path,
    max: usize,
) -> std::io::Result<Vec<command_history::CommandHistoryRecord>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "command history is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "command history has unsafe ownership, links, or write permissions",
            ));
        }
    }

    let file_len = metadata.len();
    if file_len == 0 {
        return Ok(Vec::new());
    }
    let start = file_len.saturating_sub(HISTORY_TAIL_BYTES);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        file.read(&mut previous)? == 1 && previous[0] == b'\n'
    };
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity((file_len - start) as usize);
    file.take(file_len - start).read_to_end(&mut tail)?;
    let first_complete = if starts_at_line_boundary {
        0
    } else {
        let Some(newline) = tail.iter().position(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        newline + 1
    };
    let Some(last_newline) = tail.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Vec::new());
    };
    if first_complete >= last_newline {
        return Ok(Vec::new());
    }

    let mut seen = HashSet::new();
    let mut records = Vec::with_capacity(max.min(256));
    for line in tail[first_complete..last_newline]
        .rsplit(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if line.len().saturating_add(1) > MAX_HISTORY_RECORD_BYTES {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<command_history::CommandHistoryRecord>(line)
        else {
            continue;
        };
        if !palette_command_is_safe(&record.command) || !seen.insert(record.command.clone()) {
            continue;
        }
        records.push(record);
        if records.len() == max {
            break;
        }
    }
    Ok(records)
}

fn palette_command_is_safe(command: &str) -> bool {
    !command.trim().is_empty()
        && command.len() <= MAX_PALETTE_COMMAND_BYTES
        && !command.chars().any(char::is_control)
        && !crate::text_safety::contains_visual_spoof(command)
}

/// Load one newest-first history snapshot for a palette opening.
///
/// UI components call this only from their `Toggle` path. Query changes then
/// pass the returned slice to [`gather`], keeping per-keystroke filtering free
/// of filesystem reads and JSON parsing. Closing and reopening creates a fresh
/// snapshot, so writes from this or another jterm1 process become visible at a
/// predictable boundary.
pub(crate) fn load_history_snapshot(
    path: Option<&Path>,
) -> Vec<command_history::CommandHistoryRecord> {
    path.map(|path| read_history(path, HISTORY_SNAPSHOT_LIMIT))
        .unwrap_or_default()
}

/// Run the query against all enabled sources, score, sort, and return up to
/// `limit` entries. This function is deliberately pure with respect to history
/// storage: callers supply the snapshot captured when their UI opened.
pub(crate) fn gather(
    query: &Query,
    kbmap: &KeybindingMap,
    history: &[command_history::CommandHistoryRecord],
    workflows: &[Workflow],
    limit: usize,
) -> Vec<Entry> {
    let matcher = SkimMatcherV2::default().smart_case();
    let mut out: Vec<Entry> = Vec::new();

    if matches!(query.mode, PaletteMode::All | PaletteMode::Commands) {
        for (action, binding) in kbmap.all_bound_actions() {
            let label = action.name().to_string();
            let entry = Entry {
                tier: 0,
                score: 0,
                label,
                sublabel: None,
                right: if binding.is_empty() {
                    None
                } else {
                    Some(binding)
                },
                accept: Accept::Action(action),
            };
            push_if_match(&matcher, &query.text, entry, &mut out);
        }
    }

    if matches!(query.mode, PaletteMode::All | PaletteMode::Workflows) {
        for wf in workflows {
            let Some(path) = wf.source_path.clone() else {
                continue;
            };
            let right = if wf.tags.is_empty() {
                Some(":".to_string())
            } else {
                Some(format!(":{}", wf.tags.join(",")))
            };
            let sublabel = if wf.description.is_empty() {
                Some(wf.command.clone())
            } else {
                Some(wf.description.clone())
            };
            let entry = Entry {
                tier: 1,
                score: 0,
                label: format!("⚙ {}", wf.name),
                sublabel,
                right,
                accept: Accept::RunWorkflow(path),
            };
            push_if_match(&matcher, &query.text, entry, &mut out);
        }
    }

    if matches!(query.mode, PaletteMode::Ai) {
        // Single synthetic entry: activating it kicks off the AI request.
        // We surface the raw user text in the label so they can see exactly
        // what's being sent. Empty query → harmless no-op entry that just
        // explains the prefix.
        let (label, sublabel, accept) = if query.text.trim().is_empty() {
            (
                "Type a natural-language request after ?".to_string(),
                Some("e.g. ? find files modified today".to_string()),
                Accept::TypeCommand(String::new()),
            )
        } else {
            let display_query = crate::text_safety::bounded_display_text(
                &query.text,
                MAX_PALETTE_METADATA_DISPLAY_BYTES,
                false,
            );
            (
                format!("Ask AI: {display_query}"),
                Some("Generates a shell command (review before running)".to_string()),
                Accept::AskAi(query.text.clone()),
            )
        };
        out.push(Entry {
            tier: 0,
            score: i64::MAX,
            label,
            sublabel,
            right: Some("?".to_string()),
            accept,
        });
        out.truncate(limit);
        return out;
    }

    if matches!(query.mode, PaletteMode::All | PaletteMode::History) {
        // Recency boost: more-recent entries (lower index in the snapshot) get
        // a small score nudge so that with an empty query, history sorts
        // newest-first, and with a query the tie-breaker still favors recent
        // matches.
        let len = history.len();
        for (idx, item) in history.iter().enumerate() {
            if !palette_command_is_safe(&item.command) {
                continue;
            }
            let recency = (len - idx) as i64; // 1..=len
            let entry = Entry {
                tier: 2,
                score: recency,
                label: crate::text_safety::bounded_display_text(
                    &item.command,
                    MAX_PALETTE_METADATA_DISPLAY_BYTES,
                    false,
                ),
                sublabel: Some(history_sublabel(item)),
                right: None,
                accept: Accept::TypeCommand(item.command.clone()),
            };
            push_if_match(&matcher, &query.text, entry, &mut out);
        }
    }

    out.sort_by(|a, b| a.tier.cmp(&b.tier).then(b.score.cmp(&a.score)));
    out.truncate(limit);
    out
}

fn push_if_match(matcher: &SkimMatcherV2, needle: &str, mut e: Entry, out: &mut Vec<Entry>) {
    if needle.is_empty() {
        out.push(e);
        return;
    }
    // Match against label first; fall back to sublabel for history entries
    // whose command is short but whose cwd narrows intent ("ls" in ~/proj/foo).
    let primary = matcher.fuzzy_match(&e.label, needle);
    let secondary = e
        .sublabel
        .as_deref()
        .and_then(|s| matcher.fuzzy_match(s, needle));
    let score = match (primary, secondary) {
        (Some(p), Some(s)) => Some(p.max(s / 2)),
        (Some(p), None) => Some(p),
        (None, Some(s)) => Some(s / 2),
        (None, None) => None,
    };
    if let Some(s) = score {
        // Preserve the recency baseline as a tiny tie-breaker beneath the
        // fuzzy score so equally-good matches keep their recency order.
        e.score += s.saturating_mul(1000);
        out.push(e);
    }
}

fn history_sublabel(item: &command_history::CommandHistoryRecord) -> String {
    let cwd = shorten_path(item.cwd.as_deref().unwrap_or_default());
    let text = if item.exit_code != 0 {
        format!("{cwd}  · exit {}", item.exit_code)
    } else {
        cwd
    };
    crate::text_safety::bounded_display_text(&text, MAX_PALETTE_METADATA_DISPLAY_BYTES, false)
}

fn shorten_path(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    if let Ok(home) = std::env::var("HOME") {
        if p == home {
            return "~".to_string();
        }
        if let Some(rest) = p.strip_prefix(&home).filter(|rest| rest.starts_with('/')) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_overrides_default_mode() {
        let q = Query::parse(">tab", PaletteMode::History);
        assert_eq!(q.mode, PaletteMode::Commands);
        assert_eq!(q.text, "tab");

        let q = Query::parse("@git", PaletteMode::Commands);
        assert_eq!(q.mode, PaletteMode::History);
        assert_eq!(q.text, "git");

        let q = Query::parse("foo", PaletteMode::All);
        assert_eq!(q.mode, PaletteMode::All);
        assert_eq!(q.text, "foo");
    }

    #[test]
    fn empty_query_keeps_all_entries() {
        let kbmap = KeybindingMap::from_defaults();
        let entries = gather(
            &Query {
                mode: PaletteMode::Commands,
                text: String::new(),
            },
            &kbmap,
            &[],
            &[],
            100,
        );
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e.tier == 0));
    }

    #[test]
    fn workflows_appear_under_colon_prefix() {
        let kbmap = KeybindingMap::from_defaults();
        let wf = Workflow {
            name: "Git rebase".to_string(),
            description: "rebase onto target".to_string(),
            command: "git rebase {{t}}".to_string(),
            tags: vec!["git".to_string()],
            shell: None,
            args: vec![],
            source_path: Some(std::path::PathBuf::from("/tmp/wf.yaml")),
        };
        let q = Query::parse(":rebase", PaletteMode::All);
        assert_eq!(q.mode, PaletteMode::Workflows);
        let entries = gather(&q, &kbmap, &[], std::slice::from_ref(&wf), 50);
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert!(matches!(entries[0].accept, Accept::RunWorkflow(_)));
    }

    #[test]
    fn query_filtering_uses_snapshot_until_the_next_load() {
        let path = std::env::temp_dir().join(format!(
            "jterm1-palette-history-snapshot-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, "{\"command\":\"before\",\"exit_code\":0}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let snapshot = load_history_snapshot(Some(&path));

        // Simulate another process replacing/appending history while the
        // palette remains open. Filtering the existing snapshot must not touch
        // disk, while the next opening must observe the new contents.
        std::fs::write(&path, "{\"command\":\"after\",\"exit_code\":0}\n").unwrap();
        let kbmap = KeybindingMap::from_defaults();
        let query = Query {
            mode: PaletteMode::History,
            text: String::new(),
        };
        let cached = gather(&query, &kbmap, &snapshot, &[], 10);
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].label, "before");

        let refreshed = load_history_snapshot(Some(&path));
        let reopened = gather(&query, &kbmap, &refreshed, &[], 10);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].label, "after");

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn history_palette_rejects_visual_spoof_commands_and_sanitizes_metadata() {
        let history = vec![
            command_history::CommandHistoryRecord {
                command: "echo safe\u{00ad}hidden".into(),
                cwd: Some("/tmp/hidden\u{e0020}cwd".into()),
                exit_code: 0,
                end_time_ms: None,
            },
            command_history::CommandHistoryRecord {
                command: "echo visible".into(),
                cwd: Some("/tmp/safe\u{202e}cwd".into()),
                exit_code: 1,
                end_time_ms: None,
            },
        ];
        let entries = gather(
            &Query {
                mode: PaletteMode::History,
                text: String::new(),
            },
            &KeybindingMap::from_defaults(),
            &history,
            &[],
            10,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "echo visible");
        let sublabel = entries[0].sublabel.as_deref().unwrap();
        assert!(!sublabel.contains('\u{202e}'));
        assert!(sublabel.contains('\u{fffd}'));
    }

    #[cfg(unix)]
    #[test]
    fn history_palette_file_open_is_nonblocking_no_follow_and_regular_only() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = std::env::temp_dir().join(format!(
            "jterm1-palette-safe-open-{}-{}",
            std::process::id(),
            relm4::gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir(&root).unwrap();
        let fifo = root.join("history.fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { nix::libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(read_history_checked(&fifo, 10).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        let target = root.join("target.jsonl");
        std::fs::write(&target, "{\"command\":\"safe\",\"exit_code\":0}\n").unwrap();
        let link = root.join("history.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_history_checked(&link, 10).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
