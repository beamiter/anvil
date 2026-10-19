//! Conservative export-only text filtering. Never changes persisted history.
//! This is a heuristic, not a guarantee that all secrets can be detected.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ExportSelection {
    All,
    Ids {
        ids: Vec<u64>,
    },
    /// Inclusive authoritative command-start timestamps, in Unix milliseconds.
    TimeRange {
        start_ms: u64,
        end_ms: u64,
    },
}
impl ExportSelection {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Ids { ids } if ids.is_empty() || ids.len() > 4096 => {
                Err("Choose 1–4096 block IDs.")
            }
            Self::Ids { ids }
                if ids
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    != ids.len() =>
            {
                Err("Choose each block ID only once.")
            }
            Self::TimeRange { start_ms, end_ms } if start_ms > end_ms => {
                Err("Start must precede end.")
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn includes(&self, id: u64, start: Option<u64>) -> bool {
        match self {
            Self::All => true,
            Self::Ids { ids } => ids.contains(&id),
            Self::TimeRange { start_ms, end_ms } => {
                start.is_some_and(|t| (*start_ms..=*end_ms).contains(&t))
            }
        }
    }
}
#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportPrivacy {
    Redacted,
    Original,
}
pub(crate) const WARNING: &str = "Heuristic redaction can miss secrets or remove useful context. Review before sharing. Original exports contain unfiltered text. No upload occurs.";
/// Omit complete suspicious lines rather than risk retaining quoted/multiline
/// values. PEM bodies stay suppressed through END (or EOF). All directory
/// fields are separately replaced; home-directory lines are suppressed too.
pub(crate) fn redact(text: &mut String) -> usize {
    let mut pem = false;
    let mut count = 0;
    let mut result = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let begin = lower.contains("-----begin ") && lower.contains("private key-----");
        let sensitive = pem
            || begin
            || [
                "token",
                "secret",
                "password",
                "passwd",
                "authorization",
                "api_key",
                "api-key",
                "apikey",
                "bearer ",
                "ghp_",
                "github_pat_",
                "sk-",
                "akia",
                "/home/",
                "/users/",
                "c:\\users\\",
            ]
            .iter()
            .any(|word| lower.contains(word));
        if sensitive {
            // Never amplify a short suspicious line into a longer marker.
            let length = line.trim_end_matches('\n').len();
            if length < "[REDACTED LINE]".len() {
                result.extend(std::iter::repeat_n('*', length));
            } else {
                result.push_str("[REDACTED LINE]");
            }
            if line.ends_with('\n') {
                result.push('\n');
            }
            count += 1;
        } else {
            result.push_str(line);
        }
        if begin {
            pem = true;
        }
        if lower.contains("-----end ") && lower.contains("private key-----") {
            pem = false;
        }
    }
    *text = result;
    count
}

/// Process-wide admission bounds active preview CPU workers across panes and
/// repeated Cancel/reopen. Only the worker owns a successful permit; closing
/// its UI cannot release it while computation is still running.
pub(crate) struct PreviewPermit(());
static PREVIEW_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
impl PreviewPermit {
    pub(crate) fn try_acquire() -> Option<Self> {
        PREVIEW_RUNNING
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| Self(()))
    }
    #[cfg(test)]
    pub(crate) fn is_running() -> bool {
        PREVIEW_RUNNING.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for PreviewPermit {
    fn drop(&mut self) {
        PREVIEW_RUNNING.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures_are_removed_without_losing_safe_lines() {
        let mut s = "safe error\nTOKEN=fixture-alpha\nAuthorization: Bearer fixture-beta\n/home/private-user/log\n-----BEGIN RSA PRIVATE KEY-----\nfixture-body\n-----END RSA PRIVATE KEY-----\nsafe tail".to_string();
        assert_eq!(redact(&mut s), 6);
        for secret in [
            "fixture-alpha",
            "fixture-beta",
            "private-user",
            "fixture-body",
        ] {
            assert!(!s.contains(secret));
        }
        assert!(s.contains("safe error") && s.ends_with("safe tail"));
    }
    #[test]
    fn short_secret_lines_never_expand_the_snapshot() {
        let mut text = "sk-\n".repeat(100_000);
        let before = text.len();
        assert_eq!(redact(&mut text), 100_000);
        assert!(text.len() <= before);
        assert!(!text.contains("sk-"));
    }
    #[test]
    fn range_is_inclusive_and_unknown_is_omitted() {
        let s = ExportSelection::TimeRange {
            start_ms: 10,
            end_ms: 20,
        };
        assert!(s.includes(1, Some(10)) && s.includes(1, Some(20)));
        assert!(!s.includes(1, None) && !s.includes(1, Some(21)));
        assert!(ExportSelection::TimeRange {
            start_ms: 2,
            end_ms: 1
        }
        .validate()
        .is_err());
        assert!(ExportSelection::Ids { ids: vec![] }.validate().is_err());
        assert!(ExportSelection::Ids { ids: vec![7, 7] }.validate().is_err());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::PreviewPermit;
    #[test]
    fn worker_owns_admission_until_it_finishes() {
        let permit = PreviewPermit::try_acquire().unwrap();
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = permit;
            entered.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(PreviewPermit::is_running());
        for _ in 0..100 {
            assert!(PreviewPermit::try_acquire().is_none());
        }
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(!PreviewPermit::is_running());
        assert!(PreviewPermit::try_acquire().is_some());
    }
}
