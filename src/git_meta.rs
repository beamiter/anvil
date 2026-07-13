//! git_meta — bounded shell-out for the active-pane git status strip.
//!
//! A refresh uses one `git status --porcelain=v2 --branch` process to read the
//! branch, dirty flag, and ahead/behind counts. Older code launched several git
//! processes per refresh and documented a timeout without actually enforcing it.
//! The single-process parser keeps the GTK/Relm4 main-thread pause small, while a
//! real deadline kills a wedged git process instead of allowing it to hang the UI.
//!
//! Failures are silent — non-repo directories just return `None` and the strip
//! hides itself. Git-status flakiness should never surface as a terminal error.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GIT_STATUS_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_WAIT_POLL: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMeta {
    /// Short branch name, or detached-HEAD short sha.
    pub branch: String,
    /// True if there are any uncommitted changes (tracked or untracked).
    pub dirty: bool,
    /// Commits on local branch not yet on upstream. None if no upstream.
    pub ahead: Option<u32>,
    /// Commits on upstream not yet locally. None if no upstream.
    pub behind: Option<u32>,
}

/// Resolve repo metadata for `cwd`. Returns `None` if `cwd` is not inside a git
/// repository, the directory does not exist, git is unavailable, or the probe
/// exceeds the hard timeout.
pub fn read(cwd: &Path) -> Option<RepoMeta> {
    if !cwd.is_dir() {
        return None;
    }

    let output = run_git_status(cwd)?;
    parse_porcelain_v2(&output)
}

/// Parse the stable headers and records emitted by porcelain v2.
///
/// Relevant headers:
/// - `# branch.oid <sha>`
/// - `# branch.head <name>`
/// - `# branch.ab +<ahead> -<behind>`
///
/// Any ordinary/unmerged/untracked record marks the worktree dirty. Ignored
/// records are not requested and therefore do not affect the strip.
fn parse_porcelain_v2(output: &str) -> Option<RepoMeta> {
    let mut oid: Option<&str> = None;
    let mut head: Option<&str> = None;
    let mut ahead_behind: Option<(u32, u32)> = None;
    let mut dirty = false;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.ab ") {
            ahead_behind = parse_ahead_behind(value);
            continue;
        }

        if matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u' | b'?')) {
            dirty = true;
        }
    }

    let head = head?;
    let branch = if head == "(detached)" {
        let oid = oid.filter(|value| *value != "(initial)")?;
        let short_len = oid.len().min(7);
        format!("({})", &oid[..short_len])
    } else {
        head.to_string()
    };

    let (ahead, behind) = match ahead_behind {
        Some((ahead, behind)) => (Some(ahead), Some(behind)),
        None => (None, None),
    };

    Some(RepoMeta {
        branch,
        dirty,
        ahead,
        behind,
    })
}

fn parse_ahead_behind(value: &str) -> Option<(u32, u32)> {
    let mut fields = value.split_whitespace();
    let ahead = fields.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = fields.next()?.strip_prefix('-')?.parse().ok()?;
    Some((ahead, behind))
}

/// Run one bounded git-status process. Stdout is drained on a worker thread so a
/// repository with many changed files cannot fill the pipe and deadlock before
/// the child exits. The GTK/Relm4 caller still waits synchronously, but never for
/// longer than the configured deadline.
fn run_git_status(cwd: &Path) -> Option<String> {
    let mut child = Command::new("git")
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = match thread::Builder::new()
        .name("jterm1-git-status-reader".to_string())
        .spawn(move || {
            let mut output = String::new();
            stdout.read_to_string(&mut output).ok()?;
            Some(output)
        }) {
        Ok(reader) => reader,
        Err(_) => {
            terminate_child(&mut child);
            return None;
        }
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GIT_STATUS_TIMEOUT => thread::sleep(GIT_WAIT_POLL),
            Ok(None) | Err(_) => {
                terminate_child(&mut child);
                let _ = reader.join();
                return None;
            }
        }
    };

    let output = reader.join().ok().flatten()?;
    status.success().then_some(output)
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Format a RepoMeta into the compact strip text. Designed to read at a glance:
/// `main ●  ↑2 ↓1` — branch, dirty dot, then upstream divergence.
pub fn format_strip(meta: &RepoMeta) -> String {
    let mut s = String::new();
    s.push_str(&meta.branch);
    if meta.dirty {
        s.push_str(" ●");
    }
    match (meta.ahead, meta.behind) {
        (Some(a), Some(b)) if a > 0 || b > 0 => {
            s.push_str("  ");
            if a > 0 {
                s.push_str(&format!("↑{a} "));
            }
            if b > 0 {
                s.push_str(&format!("↓{b}"));
            }
        }
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_branch_with_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head feature/cache\n",
            "# branch.upstream origin/feature/cache\n",
            "# branch.ab +2 -1\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "feature/cache".into(),
                dirty: false,
                ahead: Some(2),
                behind: Some(1),
            })
        );
    }

    #[test]
    fn parses_dirty_records_and_no_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head main\n",
            "1 .M N... 100644 100644 100644 abc abc src/main.rs\n",
            "? scratch.txt\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "main".into(),
                dirty: true,
                ahead: None,
                behind: None,
            })
        );
    }

    #[test]
    fn parses_detached_head_as_short_oid() {
        let output = concat!(
            "# branch.oid 89abcdef01234567\n",
            "# branch.head (detached)\n",
        );
        assert_eq!(
            parse_porcelain_v2(output).map(|meta| meta.branch),
            Some("(89abcde)".into())
        );
    }

    #[test]
    fn rejects_missing_or_initial_detached_oid() {
        assert_eq!(parse_porcelain_v2("# branch.oid abc\n"), None);
        assert_eq!(
            parse_porcelain_v2("# branch.oid (initial)\n# branch.head (detached)\n"),
            None
        );
    }

    #[test]
    fn format_strip_clean_no_upstream() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&m), "main");
    }

    #[test]
    fn format_strip_dirty_marker() {
        let m = RepoMeta {
            branch: "feature/x".into(),
            dirty: true,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&m), "feature/x ●");
    }

    #[test]
    fn format_strip_ahead_behind() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(2),
            behind: Some(1),
        };
        assert_eq!(format_strip(&m), "main  ↑2 ↓1");
    }

    #[test]
    fn format_strip_ahead_only() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: true,
            ahead: Some(3),
            behind: Some(0),
        };
        assert_eq!(format_strip(&m), "main ●  ↑3 ");
    }

    #[test]
    fn format_strip_zero_zero_hidden() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(0),
            behind: Some(0),
        };
        assert_eq!(format_strip(&m), "main");
    }
}
