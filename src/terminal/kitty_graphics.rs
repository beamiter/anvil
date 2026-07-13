//! Kitty graphics protocol (minimal subset) — APC `\e_G<keys>;<base64>\e\\`.
//!
//! Supported:
//! - `a=T` (transmit + display, default) and `a=t` (transmit only — buffered
//!   but not auto-displayed). `a=q`/`a=d`/`a=p` are silently dropped.
//! - `f=100` (PNG, default) and `f=32` (RGBA, requires `s=<w>` + `v=<h>`).
//! - `t=d` (inline base64 payload, default). File / shared-memory transports
//!   are ignored.
//! - Chunked transmission via `m=1` (more) + final `m=0` (or absent).
//!
//! libvte does not implement this protocol, so block-mode strips APC G
//! payloads from the byte stream before VTE sees them and renders the decoded
//! image as a GTK Picture appended to the active block.
//!
//! Per-image and per-block memory caps prevent a runaway shell from ballooning
//! RSS; oversize payloads are dropped silently.

use relm4::gtk;
use std::collections::HashMap;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::Cast;

/// Per-image base64 payload cap (before decoding) — ~16 MB encoded.
const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
/// Per-block decoded image bytes cap (sum of all images attached to a block).
pub(crate) const MAX_PENDING_BYTES_PER_BLOCK: usize = 16 * 1024 * 1024;
/// Reject dimensions beyond a conservative texture/GPU-friendly ceiling even
/// when a very thin image would otherwise fit the byte budget.
const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// A decoded image may occupy at most the same budget as all pending images in
/// a block. RGB input is expanded to RGBA, so the output size is authoritative.
const MAX_DECODED_IMAGE_BYTES: usize = MAX_PENDING_BYTES_PER_BLOCK;

/// Image-data format identifier from the `f=` key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// `f=100` — PNG. Decoded via gdk::Texture::from_bytes.
    Png,
    /// `f=32` — 32-bit RGBA, raw pixels. Width/height come from `s=`/`v=`.
    Rgba,
    /// `f=24` — 24-bit RGB. Expanded to RGBA with alpha=255 before upload.
    Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageError {
    Invalid,
    TooLarge,
}

struct ImageLayout {
    width: i32,
    height: i32,
    source_bytes: usize,
    output_bytes: usize,
    stride: usize,
}

/// In-progress assembly keyed by client image id (`i=`). Multi-chunk uploads
/// share an entry; the first chunk records format/size, every chunk appends
/// its base64 fragment.
struct Pending {
    format: Format,
    width: u32,
    height: u32,
    encoded: Vec<u8>,
    /// `a=T` (display) vs `a=t` (transmit only).
    display: bool,
}

/// Parsed result of a single APC G chunk. `Complete` carries a finished image
/// ready to render; `Pending` means more chunks are expected; `Skipped` means
/// the chunk was valid but unsupported (e.g. `a=q`) — caller should drop it.
pub(crate) enum Outcome {
    Complete(gdk::Texture),
    /// Buffered but not for display (`a=t`). Future `a=p` is unsupported, so
    /// these are effectively no-ops; returned distinctly only so the caller
    /// can avoid attaching them to the current block.
    CompleteTransmitOnly,
    Pending,
    Skipped,
    Invalid,
}

/// Stateful assembler — owns one entry per in-flight image id.
#[derive(Default)]
pub(crate) struct Assembler {
    in_flight: HashMap<u32, Pending>,
    /// Single anonymous slot for chunked uploads without `i=`. Kitty's spec
    /// allows omitting the id; in practice only one such upload is active.
    anon: Option<Pending>,
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop all in-flight state — call when a block ends or the shell resets,
    /// so a half-uploaded image doesn't leak across commands.
    pub(crate) fn reset(&mut self) {
        self.in_flight.clear();
        self.anon = None;
    }

    /// Parse one APC G payload. `payload` is the bytes between `\e_` and the
    /// terminating `\e\\` (i.e. starts with `G`). Returns the outcome the
    /// caller should act on.
    pub(crate) fn feed(&mut self, payload: &[u8]) -> Outcome {
        if payload.is_empty() || payload[0] != b'G' {
            return Outcome::Invalid;
        }
        // Split header (key=value,key=value...) from base64 body at the first `;`.
        let rest = &payload[1..];
        let (header, body) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &b""[..]),
        };
        let keys = parse_keys(header);

        let action = keys.get("a").copied().unwrap_or("t");
        // Silently ignore unsupported actions; the caller still strips the
        // APC bytes so libvte doesn't see them as garbage.
        match action {
            "T" | "t" => {}
            _ => return Outcome::Skipped,
        }

        let id: Option<u32> = keys.get("i").and_then(|s| s.parse().ok());
        let more = keys.get("m").map(|v| *v == "1").unwrap_or(false);

        // Either fetch an existing pending entry (continuation chunk) or seed
        // a new one from the header keys on the first chunk.
        let take_existing = |this: &mut Assembler| -> Option<Pending> {
            match id {
                Some(k) => this.in_flight.remove(&k),
                None => this.anon.take(),
            }
        };

        let mut entry = match take_existing(self) {
            Some(p) => p,
            None => {
                let format = match keys.get("f").copied().unwrap_or("100") {
                    "100" => Format::Png,
                    "32" => Format::Rgba,
                    "24" => Format::Rgb,
                    _ => return Outcome::Skipped,
                };
                let transport = keys.get("t").copied().unwrap_or("d");
                if transport != "d" {
                    // File / shared-memory transports require trusting a path
                    // from the shell — out of scope for this minimal subset.
                    return Outcome::Skipped;
                }
                let width = keys.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
                let height = keys.get("v").and_then(|s| s.parse().ok()).unwrap_or(0);
                let display = action == "T";
                Pending {
                    format,
                    width,
                    height,
                    encoded: Vec::new(),
                    display,
                }
            }
        };

        // Accumulate this chunk's base64 bytes (skipping any embedded
        // whitespace some emitters add). Check the final size before reserve:
        // reserving an attacker-controlled chunk first would briefly bypass the
        // cap and can panic on capacity/allocation failure.
        let chunk_len = body.iter().filter(|b| !b.is_ascii_whitespace()).count();
        let Some(encoded_len) = entry.encoded.len().checked_add(chunk_len) else {
            return Outcome::Skipped;
        };
        if encoded_len > MAX_ENCODED_BYTES {
            log::warn!(
                "kitty graphics: dropping oversize image ({} > {} encoded bytes)",
                encoded_len,
                MAX_ENCODED_BYTES
            );
            return Outcome::Skipped;
        }
        if entry.encoded.try_reserve(chunk_len).is_err() {
            return Outcome::Skipped;
        }
        for &b in body {
            if !b.is_ascii_whitespace() {
                entry.encoded.push(b);
            }
        }

        if more {
            match id {
                Some(k) => {
                    self.in_flight.insert(k, entry);
                }
                None => {
                    self.anon = Some(entry);
                }
            }
            return Outcome::Pending;
        }

        // Reject terminal-supplied raw dimensions before decoding the base64
        // body, so an impossible layout cannot force even the bounded decoded
        // allocation. PNG dimensions live inside the encoded IHDR and are
        // checked immediately after decoding, before GDK sees the bytes.
        let raw_layout = match entry.format {
            Format::Png => Ok(()),
            Format::Rgba => checked_image_layout(entry.width, entry.height, 4).map(|_| ()),
            Format::Rgb => checked_image_layout(entry.width, entry.height, 3).map(|_| ()),
        };
        match raw_layout {
            Ok(()) => {}
            Err(ImageError::TooLarge) => return Outcome::Skipped,
            Err(ImageError::Invalid) => return Outcome::Invalid,
        }

        // Final chunk — decode and build a texture.
        let decoded = match decode_base64(&entry.encoded) {
            Some(v) => v,
            None => return Outcome::Invalid,
        };
        let texture_result = match entry.format {
            Format::Png => png_to_texture(decoded),
            Format::Rgba => rgba_to_texture(entry.width, entry.height, &decoded, true),
            Format::Rgb => rgba_to_texture(entry.width, entry.height, &decoded, false),
        };
        let texture = match texture_result {
            Ok(texture) => texture,
            Err(ImageError::TooLarge) => return Outcome::Skipped,
            Err(ImageError::Invalid) => return Outcome::Invalid,
        };
        if entry.display {
            Outcome::Complete(texture)
        } else {
            // Drop the texture — we don't currently honour `a=p` placement.
            drop(texture);
            Outcome::CompleteTransmitOnly
        }
    }
}

/// Parse `key=value,key=value` into a borrow-only map. Empty / malformed
/// entries are silently skipped — the protocol mixes optional keys freely.
fn parse_keys(bytes: &[u8]) -> HashMap<&str, &str> {
    let mut out = HashMap::new();
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return out,
    };
    for pair in s.split(',') {
        if let Some(eq) = pair.find('=') {
            let (k, v) = (pair[..eq].trim(), pair[eq + 1..].trim());
            if !k.is_empty() {
                out.insert(k, v);
            }
        }
    }
    out
}

/// Standard base64 decoder. Returns None on invalid alphabet or padding.
/// Whitespace is already stripped by the caller.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn idx(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    // Trim trailing `=` padding (0–2) and any extra `=` we tolerate.
    let mut end = input.len();
    while end > 0 && input[end - 1] == b'=' {
        end -= 1;
    }
    let data = &input[..end];
    let capacity = data.len().checked_mul(3)?.checked_div(4)?;
    let mut out = Vec::new();
    out.try_reserve_exact(capacity).ok()?;
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in data {
        let v = idx(b)?;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

fn checked_image_layout(
    width: u32,
    height: u32,
    source_channels: usize,
) -> Result<ImageLayout, ImageError> {
    if width == 0 || height == 0 || !matches!(source_channels, 3 | 4) {
        return Err(ImageError::Invalid);
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(ImageError::TooLarge);
    }

    let width_usize = usize::try_from(width).map_err(|_| ImageError::TooLarge)?;
    let height_usize = usize::try_from(height).map_err(|_| ImageError::TooLarge)?;
    let width_i32 = i32::try_from(width).map_err(|_| ImageError::TooLarge)?;
    let height_i32 = i32::try_from(height).map_err(|_| ImageError::TooLarge)?;
    let pixels = width_usize
        .checked_mul(height_usize)
        .ok_or(ImageError::TooLarge)?;
    let source_bytes = pixels
        .checked_mul(source_channels)
        .ok_or(ImageError::TooLarge)?;
    let output_bytes = pixels.checked_mul(4).ok_or(ImageError::TooLarge)?;
    let stride = width_usize.checked_mul(4).ok_or(ImageError::TooLarge)?;
    if source_bytes > MAX_DECODED_IMAGE_BYTES || output_bytes > MAX_DECODED_IMAGE_BYTES {
        return Err(ImageError::TooLarge);
    }

    Ok(ImageLayout {
        width: width_i32,
        height: height_i32,
        source_bytes,
        output_bytes,
        stride,
    })
}

/// Read PNG dimensions before invoking GDK. A tiny compressed payload can
/// otherwise advertise a huge canvas and make the image loader allocate it.
fn png_layout(bytes: &[u8]) -> Result<ImageLayout, ImageError> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    const IHDR_LEN: u32 = 13;

    if bytes.len() < 24
        || &bytes[..8] != PNG_SIGNATURE
        || u32::from_be_bytes(bytes[8..12].try_into().map_err(|_| ImageError::Invalid)?) != IHDR_LEN
        || &bytes[12..16] != b"IHDR"
    {
        return Err(ImageError::Invalid);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| ImageError::Invalid)?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| ImageError::Invalid)?);
    // GDK exposes a four-byte-per-pixel texture regardless of the PNG's source
    // color type, so use RGBA for the decoded allocation budget.
    checked_image_layout(width, height, 4)
}

/// Decode a PNG payload into a gdk::Texture via the bundled GdkPixbuf loader.
fn png_to_texture(bytes: Vec<u8>) -> Result<gdk::Texture, ImageError> {
    if bytes.len() > MAX_ENCODED_BYTES {
        return Err(ImageError::TooLarge);
    }
    let _layout = png_layout(&bytes)?;
    let gbytes = glib::Bytes::from_owned(bytes);
    gdk::Texture::from_bytes(&gbytes).map_err(|_| ImageError::Invalid)
}

/// Build a texture from raw RGB(A) pixels. `has_alpha=false` expands each
/// 3-byte pixel to 4 bytes with full opacity, matching the kitty protocol's
/// `f=24` semantics.
fn rgba_to_texture(
    width: u32,
    height: u32,
    data: &[u8],
    has_alpha: bool,
) -> Result<gdk::Texture, ImageError> {
    let source_channels = if has_alpha { 4 } else { 3 };
    let layout = checked_image_layout(width, height, source_channels)?;
    if data.len() < layout.source_bytes {
        return Err(ImageError::Invalid);
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(layout.output_bytes)
        .map_err(|_| ImageError::TooLarge)?;
    if has_alpha {
        bytes.extend_from_slice(&data[..layout.source_bytes]);
    } else {
        for px in data[..layout.source_bytes].chunks_exact(3) {
            bytes.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
        }
    }
    debug_assert_eq!(bytes.len(), layout.output_bytes);

    let gbytes = glib::Bytes::from_owned(bytes);
    Ok(gdk::MemoryTexture::new(
        layout.width,
        layout.height,
        gdk::MemoryFormat::R8g8b8a8,
        &gbytes,
        layout.stride,
    )
    .upcast())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_base64(input: &[u8]) -> Vec<u8> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            out.push(TABLE[(a >> 2) as usize]);
            out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize]);
            out.push(if chunk.len() > 1 {
                TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize]
            } else {
                b'='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(c & 0x3f) as usize]
            } else {
                b'='
            });
        }
        out
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn base64_round_trip() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"f", b"Zg=="),
            (b"fo", b"Zm8="),
            (b"foo", b"Zm9v"),
            (b"foob", b"Zm9vYg=="),
            (b"hello world", b"aGVsbG8gd29ybGQ="),
        ];
        for (plain, encoded) in cases {
            let got = decode_base64(encoded).expect("valid base64");
            assert_eq!(&got, plain);
        }
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(decode_base64(b"!!!!").is_none());
    }

    #[test]
    fn key_parser_handles_whitespace_and_empty() {
        let m = parse_keys(b"a=T,f=100,i=42,m=1");
        assert_eq!(m.get("a").copied(), Some("T"));
        assert_eq!(m.get("f").copied(), Some("100"));
        assert_eq!(m.get("i").copied(), Some("42"));
        assert_eq!(m.get("m").copied(), Some("1"));
        assert_eq!(m.get("q").copied(), None);
    }

    #[test]
    fn rejects_non_g_payload() {
        let mut a = Assembler::new();
        assert!(matches!(a.feed(b""), Outcome::Invalid));
        assert!(matches!(a.feed(b"X"), Outcome::Invalid));
    }

    #[test]
    fn unsupported_action_is_skipped() {
        let mut a = Assembler::new();
        assert!(matches!(a.feed(b"Ga=q,i=1;Zm9v"), Outcome::Skipped));
        assert!(matches!(a.feed(b"Ga=d,i=1;"), Outcome::Skipped));
    }

    #[test]
    fn file_transport_is_skipped() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,t=f;L3RtcC9hLnBuZw=="),
            Outcome::Skipped
        ));
    }

    #[test]
    fn chunked_assembly_accumulates_then_decodes_rgb_pixel() {
        let mut a = Assembler::new();
        // 1×1 red pixel as raw RGB (f=24): bytes 0xFF 0x00 0x00 -> "/wAA"
        // Split across two chunks via m=1 / m=0.
        let first = a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w");
        assert!(matches!(first, Outcome::Pending));
        let second = a.feed(b"Ga=T,i=7,m=0;AA");
        match second {
            Outcome::Complete(_) => {}
            _ => panic!("expected complete texture"),
        }
        // Assembler state cleared after completion.
        assert!(a.in_flight.is_empty());
        assert!(a.anon.is_none());
    }

    #[test]
    fn oversized_raw_dimensions_are_skipped_without_overflow() {
        let cases: &[&[u8]] = &[
            b"Ga=T,f=32,s=4294967295,v=4294967295;AAAA",
            b"Ga=T,f=24,s=16384,v=16384;AAAA",
            b"Ga=T,f=32,s=16385,v=1;AAAA",
        ];
        for payload in cases {
            let mut assembler = Assembler::new();
            assert!(matches!(assembler.feed(payload), Outcome::Skipped));
        }
    }

    #[test]
    fn checked_layout_enforces_decoded_byte_budget() {
        let at_limit = checked_image_layout(2048, 2048, 4).expect("16 MiB RGBA fits");
        assert_eq!(at_limit.output_bytes, MAX_DECODED_IMAGE_BYTES);
        assert!(matches!(
            checked_image_layout(2049, 2048, 4),
            Err(ImageError::TooLarge)
        ));
        assert!(matches!(
            checked_image_layout(u32::MAX, 1, 4),
            Err(ImageError::TooLarge)
        ));
    }

    #[test]
    fn oversized_png_ihdr_is_skipped_before_gdk_decode() {
        let encoded = encode_base64(&png_header(MAX_IMAGE_DIMENSION + 1, 1));
        let mut payload = b"Ga=T,f=100;".to_vec();
        payload.extend_from_slice(&encoded);

        let mut assembler = Assembler::new();
        assert!(matches!(assembler.feed(&payload), Outcome::Skipped));
    }
}
