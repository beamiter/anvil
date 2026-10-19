//! Filesystem-only launch validation; this never starts Vim or a bridge.

use std::path::{Path, PathBuf};

pub(crate) fn resolve(path: &Path, ide: bool) -> Result<PathBuf, String> {
    let canonical = if ide && !path.is_dir() {
        let name = path
            .file_name()
            .ok_or_else(|| "choose a project directory or a file for Vim9 IDE".to_string())?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = std::fs::canonicalize(parent)
            .map_err(|err| format!("cannot open parent of {}: {err}", path.display()))?;
        if !parent.is_dir() {
            return Err(format!("{} is not a directory", parent.display()));
        }
        let target = parent.join(name);
        match std::fs::metadata(&target) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!("{} is not a regular file", target.display()));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("cannot open {}: {error}", target.display()));
            }
            _ => {}
        }
        target
    } else {
        let directory = std::fs::canonicalize(path)
            .map_err(|err| format!("cannot open directory {}: {err}", path.display()))?;
        if !directory.is_dir() {
            return Err(format!("{} is not a directory", directory.display()));
        }
        directory
    };
    if canonical.to_str().is_none() {
        return Err(format!(
            "launch path {} contains non-UTF-8 bytes and cannot be passed to the terminal safely",
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "vim9-launch-path-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_and_new_files_are_accepted_only_for_ide_launches() {
        let fixture = Fixture::new();
        let existing = fixture.0.join("existing file.txt");
        let new_file = fixture.0.join("new file.txt");
        std::fs::write(&existing, "preserved\n").unwrap();
        for ide in [false, true] {
            assert!(resolve(&fixture.0, ide).is_ok());
            assert_eq!(resolve(&existing, ide).is_ok(), ide);
            assert_eq!(resolve(&new_file, ide).is_ok(), ide);
        }
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "preserved\n");
        assert!(!new_file.exists(), "validation must not create the file");
    }

    #[test]
    fn a_new_file_requires_an_existing_directory_parent() {
        let fixture = Fixture::new();
        let file = fixture.0.join("file");
        std::fs::write(&file, "").unwrap();
        for target in [fixture.0.join("missing/new.txt"), file.join("new.txt")] {
            assert!(resolve(&target, true).is_err());
        }
    }

    #[test]
    fn a_relative_new_filename_uses_the_current_directory() {
        let name = PathBuf::from(format!(
            "vim9-relative-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!name.exists());
        assert_eq!(
            resolve(&name, true).unwrap(),
            std::fs::canonicalize(".").unwrap().join(&name)
        );
        assert!(resolve(&name, false).is_err());
        assert!(!name.exists());
    }

    #[cfg(unix)]
    #[test]
    fn new_file_targets_reject_non_utf8_names() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let target = fixture.0.join(std::ffi::OsString::from_vec(vec![0xff]));
        assert!(resolve(&target, true).unwrap_err().contains("non-UTF-8"));
    }
}
