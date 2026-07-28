//! Kitty graphics protocol (minimal subset) — APC `\e_G<keys>;<base64>\e\\`.
//!
//! Supported:
//! - `a=T` (transmit + display, default) and `a=t` (transmit only — buffered
//!   but not auto-displayed). `a=q` (support probe) is validated and answered
//!   but never displayed; `a=d`/`a=p` are dropped.
//! - `f=100` (PNG, default) and `f=32` (RGBA, requires `s=<w>` + `v=<h>`).
//! - `t=d` (inline base64 payload, default). File / shared-memory transports
//!   are ignored.
//! - Chunked transmission via `m=1` (more) + final `m=0` (or absent).
//!
//! libvte does not implement this protocol, so block mode consumes APC G
//! payloads before VTE sees them and renders the decoded image as a GTK
//! Picture appended to the finished block. Forwarding the bytes to the live
//! VTE instead (what block mode used to do) silently dropped every image.
//!
//! Per-image and per-block memory caps prevent a runaway shell from ballooning
//! RSS; oversize payloads are dropped.
//!
//! Commands that carry an `i=`/`I=` identifier receive an `OK`/error reply on
//! the PTY via `response_for`, following jterm2 — the family's reference
//! responder. See that function for the deliberate divergences.

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
/// the chunk was valid but unsupported (e.g. `a=d`) — caller should drop it.
pub(crate) enum Outcome {
    Complete(gdk::Texture),
    /// Buffered but not for display (`a=t`). Future `a=p` is unsupported, so
    /// these are effectively no-ops; returned distinctly only so the caller
    /// can avoid attaching them to the current block.
    CompleteTransmitOnly,
    Pending,
    Skipped,
    Invalid,
    /// `a=q` support probe passed validation. Never displayed or stored; the
    /// caller only owes the client an `OK` reply (see `response_for`).
    QueryOk,
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
        // Silently ignore unsupported actions; the caller consumed the APC
        // bytes already so libvte never sees them as garbage.
        match action {
            "T" | "t" => {}
            "q" => return query_outcome(&keys, body),
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

/// Validate an `a=q` support probe. `kitten icat` (and other well-behaved
/// clients) transmit a tiny sample image with `a=q` and block until the
/// terminal answers, so probes must be validated rather than silently skipped.
/// Chunking (`m=`) is ignored: known clients probe in one APC.
fn query_outcome(keys: &HashMap<&str, &str>, body: &[u8]) -> Outcome {
    if keys.get("t").copied().unwrap_or("d") != "d" {
        return Outcome::Skipped;
    }
    let format = match keys.get("f").copied().unwrap_or("100") {
        "100" => Format::Png,
        "32" => Format::Rgba,
        "24" => Format::Rgb,
        _ => return Outcome::Skipped,
    };
    let encoded: Vec<u8> = body
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if encoded.len() > MAX_ENCODED_BYTES {
        return Outcome::Skipped;
    }
    if encoded.is_empty() {
        return Outcome::Invalid;
    }
    let Some(decoded) = decode_base64(&encoded) else {
        return Outcome::Invalid;
    };
    let checked = match format {
        Format::Png => png_layout(&decoded).map(|_| ()),
        Format::Rgba | Format::Rgb => {
            let channels = if format == Format::Rgba { 4 } else { 3 };
            let width = keys.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
            let height = keys.get("v").and_then(|s| s.parse().ok()).unwrap_or(0);
            checked_image_layout(width, height, channels).and_then(|layout| {
                if decoded.len() < layout.source_bytes {
                    Err(ImageError::Invalid)
                } else {
                    Ok(())
                }
            })
        }
    };
    match checked {
        Ok(()) => Outcome::QueryOk,
        Err(ImageError::TooLarge) => Outcome::Skipped,
        Err(ImageError::Invalid) => Outcome::Invalid,
    }
}

/// Build the PTY reply owed for a processed APC G payload, or `None` when the
/// protocol expects silence. Reply semantics follow jterm2, the family's most
/// complete responder:
/// - only commands carrying an `i=`/`I=` identifier are answered (the id is
///   the client's correlation key; kitty itself stays silent without one);
/// - `q=1` suppresses `OK`, `q=2` also suppresses errors;
/// - a non-zero `p=` placement id is echoed back.
///
/// Deliberate divergences from jterm2, kept small because this responder sits
/// on top of a minimal assembler rather than a full placement table:
/// - every unsupported-but-well-formed command answers `ENOTSUP` instead of
///   per-cause `ENOENT`/`ENOSPC` codes;
/// - a chunked upload rejected at its first chunk answers every remaining
///   chunk too (the assembler keeps no tombstone for the aborted id);
/// - `q=` is read per-chunk, not remembered across an upload.
pub(crate) fn response_for(payload: &[u8], outcome: &Outcome) -> Option<Vec<u8>> {
    if payload.first() != Some(&b'G') {
        return None;
    }
    let rest = &payload[1..];
    let header = match rest.iter().position(|&b| b == b';') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let keys = parse_keys(header);
    let id: Option<u32> = keys.get("i").and_then(|s| s.parse().ok());
    let number: Option<u32> = keys.get("I").and_then(|s| s.parse().ok());
    if id.is_none() && number.is_none() {
        return None;
    }
    let quiet: u8 = keys
        .get("q")
        .and_then(|s| s.parse().ok())
        .filter(|q| *q <= 2)
        .unwrap_or(0);
    let body = match outcome {
        // Chunked uploads are answered once, after the final chunk.
        Outcome::Pending => return None,
        Outcome::Complete(_) | Outcome::CompleteTransmitOnly | Outcome::QueryOk => {
            if quiet >= 1 {
                return None;
            }
            "OK"
        }
        Outcome::Invalid => {
            if quiet >= 2 {
                return None;
            }
            "EINVAL:invalid graphics payload"
        }
        Outcome::Skipped => {
            if quiet >= 2 {
                return None;
            }
            "ENOTSUP:action, format, transport, or size not supported"
        }
    };
    let mut fields = Vec::with_capacity(3);
    if let Some(id) = id {
        fields.push(format!("i={id}"));
    }
    if let Some(number) = number {
        fields.push(format!("I={number}"));
    }
    if let Some(placement) = keys
        .get("p")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|p| *p != 0)
    {
        fields.push(format!("p={placement}"));
    }
    Some(format!("\x1b_G{};{body}\x1b\\", fields.join(",")).into_bytes())
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
        assert!(matches!(a.feed(b"Ga=d,i=1;"), Outcome::Skipped));
        assert!(matches!(a.feed(b"Ga=p,i=1;"), Outcome::Skipped));
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

    #[test]
    fn query_probe_validates_raw_pixels() {
        // kitten icat's support probe: 1×1 RGB sample under a=q.
        let mut a = Assembler::new();
        let payload = b"Ga=q,i=31,s=1,v=1,f=24,t=d;AAAA";
        let outcome = a.feed(payload);
        assert!(matches!(outcome, Outcome::QueryOk));
        assert_eq!(
            response_for(payload, &outcome).as_deref(),
            Some(b"\x1b_Gi=31;OK\x1b\\".as_slice())
        );
        // Probes never buffer anything.
        assert!(a.in_flight.is_empty());
        assert!(a.anon.is_none());
    }

    #[test]
    fn query_probe_rejects_bad_or_oversize_payloads() {
        let mut a = Assembler::new();
        // Undecodable body.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=1,v=1,f=24;!!!!"),
            Outcome::Invalid
        ));
        // Payload shorter than the advertised dimensions.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=2,v=2,f=24;AAAA"),
            Outcome::Invalid
        ));
        // Dimensions beyond the family cap.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=16385,v=1,f=32;AAAA"),
            Outcome::Skipped
        ));
    }

    #[test]
    fn responses_require_an_identifier() {
        assert_eq!(
            response_for(b"Ga=t,f=24,s=1,v=1;AAAA", &Outcome::Invalid),
            None
        );
        assert_eq!(
            response_for(b"Ga=T;AAAA", &Outcome::CompleteTransmitOnly),
            None
        );
    }

    #[test]
    fn responses_echo_ids_and_map_outcomes() {
        assert_eq!(
            response_for(
                b"Ga=t,i=41,s=1,v=1,f=24;AAAA",
                &Outcome::CompleteTransmitOnly
            )
            .as_deref(),
            Some(b"\x1b_Gi=41;OK\x1b\\".as_slice())
        );
        assert_eq!(
            response_for(b"GI=13,a=T;AAAA", &Outcome::Invalid).as_deref(),
            Some(b"\x1b_GI=13;EINVAL:invalid graphics payload\x1b\\".as_slice())
        );
        assert_eq!(
            response_for(b"Ga=d,i=5,p=17;", &Outcome::Skipped).as_deref(),
            Some(
                b"\x1b_Gi=5,p=17;ENOTSUP:action, format, transport, or size not supported\x1b\\"
                    .as_slice()
            )
        );
    }

    #[test]
    fn responses_wait_for_the_final_chunk() {
        assert_eq!(response_for(b"Ga=T,i=7,m=1;/w", &Outcome::Pending), None);
    }

    #[test]
    fn quiet_levels_suppress_responses() {
        assert_eq!(
            response_for(b"Ga=q,i=31,q=1,s=1,v=1,f=24;AAAA", &Outcome::QueryOk),
            None
        );
        // q=1 still reports errors …
        assert!(response_for(b"Ga=T,i=2,q=1;!!!!", &Outcome::Invalid).is_some());
        // … q=2 silences those too.
        assert_eq!(response_for(b"Ga=T,i=2,q=2;!!!!", &Outcome::Invalid), None);
        assert_eq!(response_for(b"Ga=d,i=3,q=2;", &Outcome::Skipped), None);
    }
}
