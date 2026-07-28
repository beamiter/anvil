//! Kitty graphics protocol — APC `\e_G<keys>;<base64>\e\\`.
//!
//! The structural half of the protocol lives in [`jterm_core::kitty_graphics`]:
//! control parsing, chunk assembly across `m=1` continuations, base64 decoding,
//! raw-format (`f=24`/`f=32`) length validation with RGB→RGBA expansion, the
//! PNG IHDR sniff, and the memory caps. This module keeps only the parts that
//! need a decoder or a PTY:
//!
//! - the GDK texture build (`gdk::Texture` for PNG, `gdk::MemoryTexture` for
//!   raw pixels),
//! - the `a=q` support-probe answer (`query_outcome`),
//! - the `i=`/`I=`/`p=` responder (`response_for`),
//! - the per-block image budget (`MAX_PENDING_BYTES_PER_BLOCK`).
//!
//! Supported: `a=T` (transmit + display) and `a=t` (transmit only — decoded so
//! the transfer can be acknowledged, then dropped, since nothing honours `a=p`
//! placement yet); `a=q` probes; `f=100` PNG and `f=32`/`f=24` raw pixels;
//! `t=d` inline payloads; chunked transmission. `a=d`/`a=p` are consumed and
//! answered `ENOTSUP`.
//!
//! libvte does not implement this protocol, so block mode consumes APC G
//! payloads before VTE sees them and renders the decoded image as a GTK
//! Picture appended to the finished block. Forwarding the bytes to the live
//! VTE instead (what block mode used to do) silently dropped every image.
//!
//! Per-image and per-block memory caps prevent a runaway shell from ballooning
//! RSS; oversize payloads are dropped. They come from [`Caps::BLOCK`], the
//! shared preset carrying this repository's historical values.
//!
//! Commands that carry an `i=`/`I=` identifier receive an `OK`/error reply on
//! the PTY via `response_for`, following jterm2 — the family's reference
//! responder. See that function for the deliberate divergences.

use relm4::gtk;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::Cast;

use jterm_core::kitty_graphics as protocol;
use protocol::{Action, Assembled, Caps, Command, Error, Format, Step};

/// Memory budget for one block's graphics traffic: 16 MiB of base64 per image,
/// 16 MiB decoded, 16 KiB of control data, 16384 px per side.
const CAPS: Caps = Caps::BLOCK;

/// Per-block decoded image bytes cap (sum of all textures attached to a block).
pub(crate) const MAX_PENDING_BYTES_PER_BLOCK: usize = CAPS.max_pending_bytes;

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

/// Stateful assembler — the shared chunk assembler plus jterm1's decoding.
pub(crate) struct Assembler {
    inner: protocol::Assembler,
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Self {
            inner: protocol::Assembler::new(CAPS),
        }
    }

    /// Drop all in-flight state — call when a block ends or the shell resets,
    /// so a half-uploaded image doesn't leak across commands.
    pub(crate) fn reset(&mut self) {
        self.inner.reset();
    }

    /// Parse one APC G payload. `payload` is the bytes between `\e_` and the
    /// terminating `\e\\` (i.e. starts with `G`). Returns the outcome the
    /// caller should act on.
    pub(crate) fn feed(&mut self, payload: &[u8]) -> Outcome {
        let step = match self.inner.feed(payload) {
            Ok(step) => step,
            Err(error) => return outcome_for(error),
        };
        match step {
            // Block mode only feeds APC payloads that start with `G`, so this
            // is a caller mistake rather than protocol traffic.
            Step::NotOurs => Outcome::Invalid,
            Step::NeedMore => Outcome::Pending,
            Step::Other {
                command,
                interrupted,
            } => {
                if interrupted {
                    log::debug!(
                        "kitty graphics: a non-transmit command dropped an in-flight upload"
                    );
                }
                match command.action {
                    Action::Query => query_outcome(&command),
                    // `a=d`/`a=p`/unknown actions are consumed silently so
                    // libvte never sees them as garbage, and answered ENOTSUP.
                    _ => Outcome::Skipped,
                }
            }
            Step::Ready(assembled) => {
                // A transmit-only image is decoded anyway: the reply owed to the
                // client is `OK` only if the payload was actually decodable.
                let display = assembled.display;
                match texture_for(assembled) {
                    Ok(_) if !display => Outcome::CompleteTransmitOnly,
                    Ok(texture) => Outcome::Complete(texture),
                    Err(error) => outcome_for(error),
                }
            }
        }
    }
}

/// Map the shared module's typed failure onto jterm1's wire codes: a malformed
/// command is `EINVAL`, anything unsupported or over budget is `ENOTSUP` (see
/// `response_for` for why jterm1 answers a single unsupported code).
fn outcome_for(error: Error) -> Outcome {
    match error {
        Error::Invalid(_) => {
            log::debug!("kitty graphics: {error}");
            Outcome::Invalid
        }
        Error::TooLarge => {
            log::warn!("kitty graphics: dropping oversize image ({error})");
            Outcome::Skipped
        }
        Error::NotSupported(_) => {
            log::debug!("kitty graphics: {error}");
            Outcome::Skipped
        }
    }
}

/// Validate an `a=q` support probe. `kitten icat` (and other well-behaved
/// clients) transmit a tiny sample image with `a=q` and block until the
/// terminal answers, so probes must be validated rather than silently skipped.
/// Nothing is buffered and no texture is built: the probe asks whether the
/// terminal *could* decode the sample, which the structural checks answer.
/// Chunking (`m=`) is ignored: known clients probe in one APC.
fn query_outcome(command: &Command<'_>) -> Outcome {
    if let Err(error) = command.require_direct_transport() {
        return outcome_for(error);
    }
    let encoded = command.payload_b64.as_bytes();
    if encoded.iter().all(|byte| byte.is_ascii_whitespace()) {
        // A probe without a sample image has nothing to validate.
        return Outcome::Invalid;
    }
    let decoded = match protocol::decode_base64(encoded, CAPS.max_decoded_bytes) {
        Ok(decoded) => decoded,
        Err(error) => return outcome_for(error),
    };
    let checked = match command.format {
        Format::Png => protocol::png_dimensions(&decoded, &CAPS).map(|_| ()),
        format => {
            let Some((width, height)) = command.declared() else {
                return Outcome::Invalid;
            };
            protocol::raw_layout(width, height, format, &CAPS).and_then(|layout| {
                if decoded.len() == layout.source_bytes {
                    Ok(())
                } else {
                    Err(Error::Invalid("raw image length does not match s= and v="))
                }
            })
        }
    };
    match checked {
        Ok(()) => Outcome::QueryOk,
        Err(error) => outcome_for(error),
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
/// - `q=` is read per-chunk, not remembered across an upload;
/// - a command the shared parser rejects outright (`o=z`, `i=` together with
///   `I=`, a control pair without `=`) is answered with silence: its
///   identifier and its `q=` level cannot be trusted, and guessing at either
///   would mean re-implementing the parsing this repository just hoisted.
pub(crate) fn response_for(payload: &[u8], outcome: &Outcome) -> Option<Vec<u8>> {
    let command = protocol::parse_command(payload, &CAPS).ok()?;
    if command.id.is_none() && command.number.is_none() {
        return None;
    }
    let body = match outcome {
        // Chunked uploads are answered once, after the final chunk.
        Outcome::Pending => return None,
        Outcome::Complete(_) | Outcome::CompleteTransmitOnly | Outcome::QueryOk => {
            if command.quiet >= 1 {
                return None;
            }
            "OK"
        }
        Outcome::Invalid => {
            if command.quiet >= 2 {
                return None;
            }
            "EINVAL:invalid graphics payload"
        }
        Outcome::Skipped => {
            if command.quiet >= 2 {
                return None;
            }
            "ENOTSUP:action, format, transport, or size not supported"
        }
    };
    // `i=` and `I=` are mutually exclusive, so at most one id plus `p=`.
    let mut fields = Vec::with_capacity(2);
    if let Some(id) = command.id {
        fields.push(format!("i={id}"));
    }
    if let Some(number) = command.number {
        fields.push(format!("I={number}"));
    }
    if let Some(placement) = command.placement.filter(|placement| *placement != 0) {
        fields.push(format!("p={placement}"));
    }
    Some(format!("\x1b_G{};{body}\x1b\\", fields.join(",")).into_bytes())
}

/// Turn a completed transfer into a GDK texture.
///
/// PNG payloads go to the bundled GdkPixbuf loader; the assembler already
/// sniffed their IHDR against [`CAPS`], so a tiny payload cannot make the
/// loader allocate a gigapixel canvas. Raw payloads are length-checked and
/// expanded to RGBA by the shared module before the upload.
fn texture_for(assembled: Assembled) -> Result<gdk::Texture, Error> {
    if assembled.format == Format::Png {
        let bytes = glib::Bytes::from_owned(assembled.bytes);
        return gdk::Texture::from_bytes(&bytes)
            .map_err(|_| Error::Invalid("PNG payload the image loader rejected"));
    }
    let (rgba, width, height) = assembled.into_rgba8()?;
    let layout = protocol::raw_layout(width, height, Format::Rgba8, &CAPS)?;
    let width = i32::try_from(layout.width).map_err(|_| Error::TooLarge)?;
    let height = i32::try_from(layout.height).map_err(|_| Error::TooLarge)?;
    let bytes = glib::Bytes::from_owned(rgba);
    Ok(gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        layout.rgba_stride,
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
    fn the_block_budget_comes_from_the_shared_caps() {
        assert_eq!(CAPS, Caps::BLOCK);
        assert_eq!(MAX_PENDING_BYTES_PER_BLOCK, 16 * 1024 * 1024);
        assert_eq!(Assembler::new().inner.caps(), &Caps::BLOCK);
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
    fn unsupported_transports_and_formats_are_skipped() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,t=f;L3RtcC9hLnBuZw=="),
            Outcome::Skipped
        ));
        assert!(matches!(
            a.feed(b"Ga=T,t=t,s=1,v=1;AQIDBA=="),
            Outcome::Skipped
        ));
        // The non-standard `f=png` alias jterm3 once accepted is not a format.
        assert!(matches!(
            a.feed(b"Ga=T,f=png,s=1,v=1;AQID"),
            Outcome::Skipped
        ));
    }

    #[test]
    fn format_defaults_to_rgba_not_png() {
        let mut a = Assembler::new();
        // Four bytes with no `f=` are one RGBA pixel; jterm1 used to read a
        // missing `f=` as PNG, but the protocol default is `f=32`.
        assert!(matches!(
            a.feed(b"Ga=T,s=1,v=1;AQIDBA=="),
            Outcome::Complete(_)
        ));
        // So PNG data now needs an explicit `f=100`: without it the command is
        // a raw transfer missing its `s=`/`v=`.
        let mut payload = b"Ga=T,i=2;".to_vec();
        payload.extend_from_slice(&encode_base64(&png_header(1, 1)));
        assert!(matches!(a.feed(&payload), Outcome::Invalid));
    }

    #[test]
    fn raw_payloads_must_match_their_declared_size_exactly() {
        let mut a = Assembler::new();
        // A 1×1 RGB image is exactly three bytes …
        assert!(matches!(
            a.feed(b"Ga=T,f=24,s=1,v=1;AQID"),
            Outcome::Complete(_)
        ));
        // … and trailing slack, which jterm1 used to accept, is now invalid.
        assert!(matches!(
            a.feed(b"Ga=T,f=24,s=1,v=1;AQIDBA=="),
            Outcome::Invalid
        ));
        assert!(matches!(
            a.feed(b"Ga=T,f=32,s=1,v=1;AQID"),
            Outcome::Invalid
        ));
    }

    #[test]
    fn chunked_assembly_accumulates_then_decodes_rgb_pixel() {
        let mut a = Assembler::new();
        // 1×1 red pixel as raw RGB (f=24): bytes 0xFF 0x00 0x00 -> "/wAA"
        // Split across two chunks via m=1 / m=0.
        let first = a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w");
        assert!(matches!(first, Outcome::Pending));
        let second = a.feed(b"Gm=0;AA");
        match second {
            Outcome::Complete(_) => {}
            _ => panic!("expected complete texture"),
        }
        // Assembler state cleared after completion.
        assert!(!a.inner.has_pending());
    }

    #[test]
    fn continuation_chunks_may_not_repeat_the_metadata() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w"),
            Outcome::Pending
        ));
        // The protocol sends metadata on the first chunk only; repeating it —
        // which jterm1 used to route by `i=` — now aborts the upload.
        assert!(matches!(a.feed(b"Ga=T,i=7,m=0;AA"), Outcome::Invalid));
        assert!(!a.inner.has_pending());
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
    fn oversized_png_ihdr_is_skipped_before_gdk_decode() {
        let encoded = encode_base64(&png_header(CAPS.max_dimension + 1, 1));
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
        assert!(!a.inner.has_pending());
    }

    #[test]
    fn query_probe_rejects_bad_or_oversize_payloads() {
        let mut a = Assembler::new();
        // Undecodable body.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=1,v=1,f=24;!!!!"),
            Outcome::Invalid
        ));
        // Payload that does not match the advertised dimensions.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=2,v=2,f=24;AAAA"),
            Outcome::Invalid
        ));
        // No sample image at all.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=1,v=1,f=24;"),
            Outcome::Invalid
        ));
        // Dimensions beyond the family cap.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,s=16385,v=1,f=32;AAAA"),
            Outcome::Skipped
        ));
        // A probe for a transport jterm1 does not implement.
        assert!(matches!(
            a.feed(b"Ga=q,i=1,t=f,f=100;L3RtcC9hLnBuZw=="),
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
    fn unparsable_commands_are_answered_with_silence() {
        let mut a = Assembler::new();
        // Zlib-compressed payloads and `i=` together with `I=` are rejected
        // while parsing, so no identifier survives to correlate a reply with.
        for payload in [
            b"Ga=T,i=1,o=z,s=1,v=1;AQIDBA==".as_slice(),
            b"Ga=T,i=1,I=2,s=1,v=1;AQIDBA==".as_slice(),
        ] {
            let outcome = a.feed(payload);
            assert!(matches!(outcome, Outcome::Skipped | Outcome::Invalid));
            assert_eq!(response_for(payload, &outcome), None);
        }
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
