//! Only called for the immutable target after explicit local-machine confirmation.
use super::{local_preview_file as file, local_preview_image as image};
use std::{io, path::Path};

pub enum Content {
    Text(String),
    Image(image::RgbaPreview),
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
