//! Worker-only atomic export publication. No GTK objects cross this boundary.
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<AtomicU8>);
impl Cancellation {
    /// Never blocks GTK, including while a filesystem publication is slow.
    /// False means the complete file has entered its non-cancellable commit.
    pub(crate) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            || self.0.load(Ordering::Acquire) == 1
    }
    pub(crate) fn check(&self) -> io::Result<()> {
        if self.0.load(Ordering::Acquire) == 1 {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "export cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
struct CheckedWriter<'a, W>(&'a mut W, &'a Cancellation);
impl<W: Write> Write for CheckedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Write::write_all retries Interrupted forever; cancellation must not.
        self.1.check().map_err(io::Error::other)?;
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.1.check()?;
        self.0.flush()
    }
}

pub(crate) fn publish(
    directory: &Path,
    stamp: &str,
    extension: &str,
    cancel: &Cancellation,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<PathBuf> {
    publish_with_sync(directory, stamp, extension, cancel, write, |file| {
        file.sync_all()
    })
}

pub(crate) fn publish_with_sync(
    directory: &Path,
    stamp: &str,
    extension: &str,
    cancel: &Cancellation,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<PathBuf> {
    publish_with_operations(
        directory,
        stamp,
        extension,
        cancel,
        write,
        sync,
        |from, to| fs::hard_link(from, to),
    )
}

fn publish_with_operations(
    directory: &Path,
    stamp: &str,
    extension: &str,
    cancel: &Cancellation,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    sync: impl FnOnce(&File) -> io::Result<()>,
    mut link: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    cancel.check()?;
    fs::create_dir_all(directory)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temporary, file) = loop {
        cancel.check()?;
        let path = directory.join(format!(
            ".session-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => break (Temporary(path), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let mut buffered = BufWriter::new(file);
    let written = write(&mut CheckedWriter(&mut buffered, cancel));
    cancel.check()?;
    written?;
    cancel.check()?;
    buffered.flush()?;
    sync(buffered.get_ref())?;
    // Atomic commit boundary: cancellation wins before this CAS, otherwise
    // the fully written file is committing. Never hold a UI-visible lock over
    // filesystem calls. A late cancellation cannot retract a published file.
    if cancel
        .0
        .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "export cancelled",
        ));
    }
    for attempt in 0..100 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let path = directory.join(format!("session-{stamp}{suffix}.{extension}"));
        match link(&temporary.0, &path) {
            Ok(()) => {
                drop(buffered);
                drop(temporary);
                if let Ok(parent) = File::open(directory) {
                    let _ = parent.sync_all();
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many session exports share this timestamp",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "anvil-export-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn atomic_private_collision_safe_output() {
        let dir = directory();
        let token = Cancellation::default();
        let first = publish(&dir, "stamp", "json", &token, |w| w.write_all(b"original")).unwrap();
        let second = publish(&dir, "stamp", "json", &Cancellation::default(), |w| {
            w.write_all(b"second")
        })
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"original");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failure_and_cancel_leave_no_partial_file() {
        for fail_sync in [false, true] {
            let dir = directory();
            let result = publish_with_sync(
                &dir,
                "stamp",
                "md",
                &Cancellation::default(),
                |w| {
                    w.write_all(b"partial")?;
                    if fail_sync {
                        Ok(())
                    } else {
                        Err(io::Error::other("write failure"))
                    }
                },
                |_| Err(io::Error::other("sync failure")),
            );
            assert!(result.is_err());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn cancellation_during_write_all_returns_without_retry_loop() {
        let dir = directory();
        let token = Cancellation::default();
        let outcome = publish(&dir, "stamp", "md", &token, |writer| {
            assert!(token.cancel());
            writer.write_all(b"cancelled write")
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_during_slow_commit_is_nonblocking_and_reports_too_late() {
        let dir = directory();
        let worker_dir = dir.clone();
        let token = Cancellation::default();
        let worker_token = token.clone();
        let (entered, ready) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            publish_with_operations(
                &worker_dir,
                "stamp",
                "json",
                &worker_token,
                |w| w.write_all(b"complete"),
                |_| Ok(()),
                |from, to| {
                    entered.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    fs::hard_link(from, to)
                },
            )
        });
        ready.recv().unwrap();
        let now = std::time::Instant::now();
        assert!(
            !token.cancel(),
            "publication already committed; cannot retract"
        );
        assert!(now.elapsed() < std::time::Duration::from_millis(100));
        let path = worker.join().unwrap().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"complete");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancel_during_two_second_fsync_does_not_block_or_publish() {
        let dir = directory();
        let worker_dir = dir.clone();
        let token = Cancellation::default();
        let worker_token = token.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            publish_with_sync(
                &worker_dir,
                "stamp",
                "json",
                &worker_token,
                |w| w.write_all(b"complete"),
                |_| {
                    started.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    Ok(())
                },
            )
        });
        ready.recv().unwrap();
        let now = std::time::Instant::now();
        token.cancel();
        assert!(now.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
