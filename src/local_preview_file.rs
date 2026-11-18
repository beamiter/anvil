//! Bounded, nonexecuting local source preview. Caller must obtain local-file intent.
use std::io::{self, Read};
use std::path::Path;

pub const MAX_BYTES: u64 = 256 * 1024;

pub fn read_source(path: &Path) -> io::Result<String> {
    if !path.is_absolute() {
        return Err(io::Error::other("Choose an absolute local path"));
    }
    let file = open_without_symlink_components(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(io::Error::other(
            "Preview requires a regular file of at most 256 KiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES || bytes.contains(&0) {
        return Err(io::Error::other(
            "Preview is too large or contains binary data",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::other("Preview requires UTF-8 source text"))?;
    // Render controls explicitly, including bidi direction and invisible formatting.
    Ok(text.chars().flat_map(|c| {
        if (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') {
            c.escape_unicode().collect::<Vec<_>>()
        } else { vec![c] }
    }).collect())
}

#[cfg(target_os = "linux")]
fn open_without_symlink_components(path: &Path) -> io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    let components = path.components().collect::<Vec<_>>();
    if components.first() != Some(&Component::RootDir) || components.len() < 2 {
        return Err(io::Error::other("Expected an absolute regular file path"));
    }
    // Each open is relative to an owned directory FD; replacing path components
    // cannot redirect a later open through a newly inserted symlink.
    let root = CString::new("/").unwrap();
    let raw = unsafe {
        nix::libc::open(
            root.as_ptr(),
            nix::libc::O_RDONLY | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut directory = unsafe { OwnedFd::from_raw_fd(raw) };
    for (index, component) in components.iter().enumerate().skip(1) {
        let Component::Normal(name) = component else {
            return Err(io::Error::other(
                "Preview does not resolve parent components",
            ));
        };
        let name =
            CString::new(name.as_bytes()).map_err(|_| io::Error::other("Invalid local path"))?;
        let final_component = index + 1 == components.len();
        let flags = nix::libc::O_PATH
            | nix::libc::O_CLOEXEC
            | nix::libc::O_NOFOLLOW
            | if final_component {
                0
            } else {
                nix::libc::O_DIRECTORY
            };
        let raw = unsafe { nix::libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let opened = unsafe { OwnedFd::from_raw_fd(raw) };
        if final_component {
            let source = std::fs::File::from(opened);
            let before = source.metadata()?;
            if !before.is_file() || before.len() > MAX_BYTES {
                return Err(io::Error::other("Expected a bounded regular file"));
            }
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            // This is our still-owned descriptor, not a second lookup of the input path.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
                .open(format!("/proc/self/fd/{}", source.as_raw_fd()))?;
            let after = file.metadata()?;
            if !after.is_file() || before.dev() != after.dev() || before.ino() != after.ino() {
                return Err(io::Error::other("Local file identity changed"));
            }
            return Ok(file);
        }
        directory = opened;
    }
    Err(io::Error::other("Expected a regular file"))
}

#[cfg(not(target_os = "linux"))]
fn open_without_symlink_components(_: &Path) -> io::Result<std::fs::File> {
    Err(io::Error::other(
        "Safe local preview is only supported on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn local_source_is_bounded_and_never_interpreted() {
        let dir = std::env::temp_dir().join(format!("forge-source-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.tsx");
        std::fs::write(&path, "<script>throw 1</script>\n\u{202e}").unwrap();
        assert_eq!(
            read_source(&path).unwrap(),
            "<script>throw 1</script>\n\\u{202e}"
        );
        std::fs::write(&path, [0]).unwrap();
        assert!(read_source(&path).is_err());
        std::fs::write(&path, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert!(read_source(&path).is_err());
        assert!(read_source(&dir).is_err());
        assert!(read_source(Path::new("/dev/null")).is_err());
        let fifo = dir.join("pipe.md");
        use std::os::unix::ffi::OsStrExt;
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { nix::libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(read_source(&fifo).is_err());
        assert!(read_source(Path::new("relative.md")).is_err());
        #[cfg(unix)]
        {
            let link = dir.join("link.md");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(read_source(&link).is_err());
            let parent_link = dir.join("parent");
            std::os::unix::fs::symlink(&dir, &parent_link).unwrap();
            assert!(read_source(&parent_link.join("source.tsx")).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
