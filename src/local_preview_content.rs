//! Only called for the immutable target after explicit local-machine confirmation.
use super::{local_preview_file as file, local_preview_image as image};
use std::{io, path::Path};

pub enum Content {
    Text(String),
    Image(image::RgbaPreview),
}

pub struct LocatedContent {
    pub content: Content,
    pub path: std::path::PathBuf,
    pub location: Option<crate::local_reference::SourceLocationCandidate>,
}

/// Pure disclosure; never probes either candidate before user confirmation.
pub fn confirmation_target(path: &Path) -> String {
    let exact = format!("Literal local file\n{}", path.display());
    match path
        .to_str()
        .and_then(crate::local_reference::parse_candidate)
        .and_then(|candidate| candidate.source_location)
    {
        Some(location) => {
            let coordinates = match location.column {
                Some(column) => format!("line {}, column {column} (reported)", location.line),
                None => format!("line {}", location.line),
            };
            format!(
                "{exact}\n\nFallback when the literal file is missing\n{} ({coordinates})",
                location.path
            )
        }
        None => exact,
    }
}

pub fn read_reference(path: &Path) -> io::Result<LocatedContent> {
    read_reference_with(path, read_confirmed)
}

fn read_reference_with(
    path: &Path,
    mut read: impl FnMut(&Path) -> io::Result<Content>,
) -> io::Result<LocatedContent> {
    match read(path) {
        Ok(content) => Ok(LocatedContent {
            content,
            path: path.to_path_buf(),
            location: None,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let Some(location) = path
                .to_str()
                .and_then(crate::local_reference::parse_candidate)
                .and_then(|candidate| candidate.source_location)
            else {
                return Err(error);
            };
            let target = std::path::PathBuf::from(&location.path);
            let content = read(&target)?;
            Ok(LocatedContent {
                content,
                path: target,
                location: Some(location),
            })
        }
        Err(error) => Err(error),
    }
}

pub fn read_confirmed(path: &Path) -> io::Result<Content> {
    let image_extension = path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        s.eq_ignore_ascii_case("png")
            || s.eq_ignore_ascii_case("jpg")
            || s.eq_ignore_ascii_case("jpeg")
    });
    if image_extension {
        let bytes = file::read_bounded(path, image::MAX_IMAGE_BYTES)?;
        image::decode(&bytes).map(Content::Image)
    } else {
        let bytes = file::read_bounded(path, file::MAX_BYTES)?;
        file::source_text(bytes).map(Content::Text)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use ::image::ImageEncoder;
    #[test]
    fn disclosure_names_both_candidates_without_io() {
        let note = confirmation_target(Path::new("/missing/project/App.tsx:12:3"));
        assert!(note.contains("Literal local file\n/missing/project/App.tsx:12:3"));
        assert!(note.contains(
            "Fallback when the literal file is missing\n/missing/project/App.tsx (line 12"
        ));
        assert_eq!(
            confirmation_target(Path::new("/missing/plain.md")),
            "Literal local file\n/missing/plain.md"
        );
    }

    #[test]
    fn location_fallback_only_follows_lookup_absence() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Other,
        ] {
            let mut calls = 0;
            let result = read_reference_with(Path::new("/local/App.tsx:12:3"), |_| {
                calls += 1;
                Err(io::Error::from(kind))
            });
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn literal_colon_file_precedes_location_and_unsafe_files_never_fall_back() {
        let dir =
            std::env::temp_dir().join(format!("anvil-preview-location-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let base = dir.join("App.tsx");
        let exact = dir.join("App.tsx:12:3");
        std::fs::write(&base, "BASE").unwrap();
        std::fs::write(&exact, "LITERAL").unwrap();
        let result = read_reference(&exact).unwrap();
        assert_eq!(result.path, exact);
        assert!(result.location.is_none());
        assert!(matches!(result.content, Content::Text(s) if s == "LITERAL"));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exact, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = read_reference(&exact);
        std::fs::set_permissions(&exact, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Root may bypass mode bits; the injected error test covers that policy too.
        if unsafe { nix::libc::geteuid() } != 0 {
            assert_eq!(
                denied.err().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        std::fs::remove_file(&exact).unwrap();
        let result = read_reference(&exact).unwrap();
        assert_eq!(result.path, base);
        assert_eq!(result.location.unwrap().line.get(), 12);
        assert!(matches!(result.content, Content::Text(s) if s == "BASE"));
        std::os::unix::fs::symlink(&base, &exact).unwrap();
        assert!(read_reference(&exact).is_err());
        std::fs::remove_file(&exact).unwrap();
        std::fs::write(&exact, [0xff]).unwrap();
        assert!(read_reference(&exact).is_err());
        std::fs::remove_file(&exact).unwrap();
        std::fs::create_dir(&exact).unwrap();
        assert!(read_reference(&exact).is_err());
        std::fs::remove_dir(&exact).unwrap();
        std::fs::remove_file(&base).unwrap();
        assert_eq!(
            read_reference(&exact).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_dispatch_uses_bounded_regular_files_and_source_stays_inert() {
        let dir = std::env::temp_dir().join(format!("anvil-preview-image-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("sample.PNG");
        let mut bytes = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[1, 2, 3, 255], 1, 1, ::image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let Content::Image(decoded) = read_confirmed(&path).unwrap() else {
            panic!("expected image");
        };
        assert_eq!(decoded.pixels, [1, 2, 3, 255]);
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_confirmed(&link).is_err());
        assert!(read_confirmed(&dir).is_err());
        std::fs::write(&path, b"<svg onload='alert(1)'/>").unwrap();
        assert!(read_confirmed(&path).is_err());
        let source = dir.join("sample.svg");
        std::fs::write(&source, b"<svg onload='alert(1)'/>").unwrap();
        assert!(
            matches!(read_confirmed(&source).unwrap(), Content::Text(s) if s == "<svg onload='alert(1)'/>")
        );
        std::fs::write(&path, vec![0; image::MAX_IMAGE_BYTES as usize + 1]).unwrap();
        assert!(read_confirmed(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
