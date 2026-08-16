//! Safe local-image drag payloads for terminal prompts.
//!
//! Desktop drag-and-drop supplies decoded local paths. We turn those paths into
//! ordinary review-first terminal input: no newline, no implicit submission,
//! and no control or bidi-spoofing characters.

use std::fmt;
use std::path::{Path, PathBuf};

const MAX_DROPPED_IMAGES: usize = 16;
const MAX_DROP_PAYLOAD_BYTES: usize = 256 * 1024;
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageDropError(&'static str);

impl fmt::Display for ImageDropError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

pub(crate) fn prompt_payload(paths: &[PathBuf]) -> Result<String, ImageDropError> {
    if paths.is_empty() {
        return Err(ImageDropError("the drop contained no local files"));
    }
    if paths.len() > MAX_DROPPED_IMAGES {
        return Err(ImageDropError("too many images were dropped at once"));
    }

    let mut quoted = Vec::with_capacity(paths.len());
    let mut bytes = 0usize;
    for path in paths {
        if !path.is_file() {
            return Err(ImageDropError(
                "only existing local image files can be dropped",
            ));
        }
        if !is_supported_image(path) {
            return Err(ImageDropError("unsupported image type"));
        }
        let text = path
            .to_str()
            .ok_or(ImageDropError("the image path is not valid UTF-8"))?;
        if text.chars().any(char::is_control)
            || jterm_core::review_input::contains_visual_spoofing(text)
        {
            return Err(ImageDropError(
                "the image path contains hidden or control text",
            ));
        }

        let encoded = if text
            .chars()
            .all(|character| character.is_alphanumeric() || "._-/~".contains(character))
        {
            text.to_string()
        } else {
            jterm_core::process::shell_single_quote(text)
        };
        bytes = bytes
            .checked_add(encoded.len() + 1)
            .ok_or(ImageDropError("the dropped image paths are too long"))?;
        if bytes > MAX_DROP_PAYLOAD_BYTES {
            return Err(ImageDropError("the dropped image paths are too long"));
        }
        quoted.push(encoded);
    }

    let mut payload = quoted.join(" ");
    // Match desktop terminal drag behavior: leave the caret ready for more
    // prompt text without ever adding Enter.
    payload.push(' ');
    Ok(payload)
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_file(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "jterm-image-drop-{}-{nonce}-{name}",
            std::process::id()
        ));
        File::create(&path).expect("temporary image");
        path
    }

    #[test]
    fn image_paths_are_shell_quoted_and_never_submitted() {
        let image = temporary_file("screen shot's.PNG");
        let payload = prompt_payload(std::slice::from_ref(&image)).expect("valid image");
        assert_eq!(
            payload,
            format!(
                "{} ",
                jterm_core::process::shell_single_quote(image.to_str().unwrap())
            )
        );
        assert!(!payload.contains('\n'));
        assert!(!payload.contains('\r'));
        std::fs::remove_file(image).expect("cleanup");
    }

    #[test]
    fn non_image_files_are_rejected() {
        let text = temporary_file("notes.txt");
        assert_eq!(
            prompt_payload(std::slice::from_ref(&text)).unwrap_err(),
            ImageDropError("unsupported image type")
        );
        std::fs::remove_file(text).expect("cleanup");
    }
}
