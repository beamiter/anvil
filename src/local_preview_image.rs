//! PNG/JPEG only, decoded off the UI thread into owned RGBA bytes.
//! Input and dimensions are capped; decoder allocation limits are best-effort.
//! They are not a hard process-memory or CPU/wall-time limit.
use std::io::{self, Cursor};

pub const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EDGE: u32 = 4096;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_DECODER_ALLOC: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct RgbaPreview {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub fn decode(bytes: &[u8]) -> io::Result<RgbaPreview> {
    let invalid = || io::Error::other("Expected a bounded PNG or JPEG image");
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(invalid());
    }
    let format = image::guess_format(bytes).map_err(|_| invalid())?;
    if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg) {
        return Err(invalid());
    }
    let reader = || {
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_EDGE);
        limits.max_image_height = Some(MAX_EDGE);
        limits.max_alloc = Some(MAX_DECODER_ALLOC);
        reader.limits(limits);
        reader
    };
    let (width, height) = reader().into_dimensions().map_err(|_| invalid())?;
    if width == 0
        || height == 0
        || width > MAX_EDGE
        || height > MAX_EDGE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(invalid());
    }
    let decoded = reader().decode().map_err(|_| invalid())?;
    if decoded.width() != width || decoded.height() != height {
        return Err(invalid());
    }
    let pixels = decoded.into_rgba8().into_raw();
    if pixels.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        return Err(invalid());
    }
    Ok(RgbaPreview {
        width,
        height,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    #[test]
    fn decodes_png_and_jpeg_into_owned_rgba() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = decode(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.pixels, [255, 0, 0, 255]);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[255, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert_eq!(decode(&jpeg).unwrap().pixels.len(), 4);
    }

    #[test]
    fn refuses_unsupported_malformed_and_oversized_bytes() {
        for bytes in [
            &b"<svg/>"[..],
            &b"GIF89a"[..],
            &b"\x89PNG\r\n\x1a\n"[..],
            &b"\xff\xd8\xff"[..],
        ] {
            assert!(decode(bytes).is_err());
        }
        assert!(decode(&vec![0; MAX_IMAGE_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn rejects_edge_and_pixel_budgets_before_full_decode() {
        for (width, height) in [(4097, 1), (2049, 2048)] {
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(
                    &vec![0; width as usize * height as usize],
                    width,
                    height,
                    image::ExtendedColorType::L8,
                )
                .unwrap();
            assert!(decode(&png).is_err());
        }
    }
}
