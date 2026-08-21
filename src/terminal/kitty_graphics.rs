//! Kitty graphics protocol — APC `\e_G<keys>;<base64>\e\\`.
//!
//! The structural half of the protocol lives in [`jterm_core::kitty_graphics`]
//! and is shared with the rest of the family: control-data parsing, `m=1` chunk
//! assembly, base64 decoding, raw-format length validation and the PNG IHDR
//! sniff. This module keeps only what needs GTK or a reply on the wire:
//!
//! - the GDK decode path ([`gdk::Texture::from_bytes`] for `f=100`, a
//!   [`gdk::MemoryTexture`] for `f=24`/`f=32`),
//! - the `a=q` support probe ([`query_outcome`]),
//! - the PTY responder ([`response_for`]),
//! - the per-block image budget ([`MAX_PENDING_BYTES_PER_BLOCK`], enforced by
//!   the caller in `block_view/mod.rs`).
//!
//! Supported display subset: direct `a=T` with static `c=`/`r=`/`C=` geometry,
//! `i=` identity, and `f=100`/`f=32`/`f=24`; `a=q` probes are validated.
//! Transmit-only `a=t`, `I=` allocation, placement/crop/z controls, `a=d` and
//! `a=p` are answered `ENOTSUP` because this renderer has no persistent image
//! store or replacement table. `f=100`
//! (PNG), `f=32` (RGBA, the protocol default) and `f=24` (RGB) with the inline
//! `t=d` transport only. Chunked transmission via `m=1` + final `m=0`, where a
//! continuation may carry only `m=` and an optional `q=`; any other action
//! arriving mid-upload drops the in-flight chunks.
//!
//! libvte does not implement this protocol, so block mode consumes APC G
//! payloads before VTE sees them and renders the decoded image as a GTK
//! Picture appended to the finished block.
//!
//! Memory caps come from [`Caps::BLOCK`], the family's block-terminal budget:
//! 16 MiB encoded per image, 16 MiB decoded, 16 MiB across all in-flight
//! uploads, and a 16384-pixel edge. Oversize payloads are dropped.
//!
//! Commands that carry an `i=`/`I=` identifier receive an `OK`/error reply on
//! the PTY via [`response_for`], following the family responder contract. See
//! that function for the deliberate divergences.

use relm4::gtk;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::Cast;

use std::collections::{HashMap, HashSet};

use jterm_core::kitty_graphics as core;
use jterm_core::kitty_graphics::{Action, Assembled, Caps, Error, Format, Step};

/// Memory budget for one block's graphics traffic — the family's block preset.
const CAPS: Caps = Caps::BLOCK;

/// Per-block decoded image bytes cap (sum of all images attached to a block).
/// A decoded image may occupy at most the same budget, so one image can fill a
/// block on its own but two oversize ones cannot.
pub(crate) const MAX_PENDING_BYTES_PER_BLOCK: usize = CAPS.max_decoded_bytes;
/// Bound GTK/GDK object fan-out independently from decoded pixels. Tiny images
/// still allocate a Texture, Picture, signal state and widget-tree nodes.
pub(crate) const MAX_IMAGES_PER_BLOCK: usize = 64;
/// Conservative fixed retained charge for one Texture + Picture pair and its
/// surrounding GTK/GDK bookkeeping. Pixel backing is charged separately.
pub(crate) const RETAINED_BYTES_PER_IMAGE_OBJECT: usize = 64 * 1024;
const MAX_SEEN_DISPLAY_IDS: usize = 256;

pub(crate) fn pending_image_bytes_after_admission(
    retained_bytes: usize,
    image_count: usize,
    pixel_bytes: usize,
    encoded_source_backing_bytes: usize,
) -> Option<usize> {
    if image_count >= MAX_IMAGES_PER_BLOCK {
        return None;
    }
    let next = retained_bytes
        .checked_add(pixel_bytes)?
        .checked_add(encoded_source_backing_bytes)?
        .checked_add(RETAINED_BYTES_PER_IMAGE_OBJECT)?;
    (next <= MAX_PENDING_BYTES_PER_BLOCK).then_some(next)
}

/// Parsed result of a single APC G chunk. `Complete` carries a finished image
/// ready to render; `Pending` means more chunks are expected; `Skipped` means
/// the chunk was valid but unsupported (e.g. `a=d`) — caller should drop it.
pub(crate) enum Outcome {
    Complete {
        texture: gdk::Texture,
        /// PNG loaders may retain the encoded GBytes as well as decoded pixel
        /// backing. Raw formats transfer/replace their source Vec, so only PNG
        /// needs this additional retained charge.
        encoded_source_backing_bytes: usize,
        /// Display geometry and cursor identity captured from the FIRST
        /// transfer chunk. A legal continuation carries only `m=`/`q=`, so
        /// looking at the chunk which produced `Ready` silently loses the
        /// placement used by real clients such as chafa.
        placement: Option<DisplayPlacement>,
    },
    Pending,
    Skipped,
    Invalid,
    /// `a=q` support probe passed validation. Never displayed or stored; the
    /// caller only owes the client an `OK` reply (see [`response_for`]).
    QueryOk,
}

/// Placement controls needed by Unified's probe-addressed image layer.
///
/// The shared assembler deliberately ignores placement. Keep this sidecar
/// small and lossless: validation which depends on the live grid happens in
/// the backend, while malformed/missing values remain `None` and fail closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayPlacement {
    pub(crate) columns: Option<u32>,
    pub(crate) rows: Option<u32>,
    /// False when a present `c=`/`r=`/`C=` could not be parsed. Absence is
    /// distinct: only absent `c=` may use pixel-width inference.
    pub(crate) geometry_valid: bool,
    /// `Some(true)` for the protocol default / `C=0`, `Some(false)` for
    /// `C=1`, and `None` for an unsupported value.
    pub(crate) cursor_moves: Option<bool>,
    /// Cursor cell captured when the transfer's final chunk arrived.
    pub(crate) cursor_col: i64,
    pub(crate) cursor_row: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TransferSlot {
    Anonymous,
    Id(u32),
}

fn transfer_slot(id: Option<u32>) -> TransferSlot {
    id.filter(|id| *id != 0)
        .map_or(TransferSlot::Anonymous, TransferSlot::Id)
}

fn has_unsupported_static_placement_control(payload: &[u8]) -> bool {
    let Some(rest) = payload.strip_prefix(b"G") else {
        return false;
    };
    let control = memchr::memchr(b';', rest).map_or(rest, |index| &rest[..index]);
    let Ok(control) = std::str::from_utf8(control) else {
        return false;
    };
    control
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| match key {
            "x" | "y" | "w" | "h" | "X" | "Y" | "p" => value.parse::<u32>() != Ok(0),
            "z" => value.parse::<i32>() != Ok(0),
            "U" | "P" | "Q" | "H" | "V" => true,
            _ => false,
        })
}

fn display_placement(
    command: &core::Command<'_>,
    (cursor_col, cursor_row): (i64, i64),
) -> Option<DisplayPlacement> {
    let parsed_optional = |key| match command.u32_value(key) {
        Ok(value) => (value, true),
        Err(_) => (None, false),
    };
    let (columns, columns_valid) = parsed_optional("c");
    let (rows, rows_valid) = parsed_optional("r");
    let (cursor_moves, cursor_policy_valid) = match command.get("C") {
        None | Some("0") => (Some(true), true),
        Some("1") => (Some(false), true),
        Some(_) => (None, false),
    };
    let static_controls_valid = ["x", "y", "w", "h", "X", "Y", "p"].into_iter().all(|key| {
        command
            .u32_value(key)
            .is_ok_and(|value| value.unwrap_or(0) == 0)
    }) && command
        .i32_value("z")
        .is_ok_and(|value| value.unwrap_or(0) == 0)
        && ["U", "P", "Q", "H", "V"]
            .into_iter()
            .all(|key| command.get(key).is_none())
        && command.number.is_none();
    (command.action == Action::Display).then_some(DisplayPlacement {
        columns,
        rows,
        geometry_valid: columns_valid && rows_valid && cursor_policy_valid && static_controls_valid,
        cursor_moves,
        cursor_col,
        cursor_row,
    })
}

/// Texture-free mirror of [`Outcome`]: what crosses the `RenderBackend` trait
/// and what [`response_for`] answers from. `Complete` here means "a decoded
/// texture is parked backend-side awaiting admission" — the texture itself
/// never leaves the backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FeedStatus {
    Complete,
    Pending,
    Skipped,
    Invalid,
    QueryOk,
}

impl Outcome {
    /// Drop the texture payload, keeping the variant identity the protocol
    /// responder and the admission decision need.
    pub(crate) fn status(&self) -> FeedStatus {
        match self {
            Outcome::Complete { .. } => FeedStatus::Complete,
            Outcome::Pending => FeedStatus::Pending,
            Outcome::Skipped => FeedStatus::Skipped,
            Outcome::Invalid => FeedStatus::Invalid,
            Outcome::QueryOk => FeedStatus::QueryOk,
        }
    }
}

/// Stateful assembler — a thin GTK-side wrapper over the shared one.
pub(crate) struct Assembler {
    inner: core::Assembler,
    /// Geometry is keyed exactly like the shared assembler's in-flight table.
    /// `current` identifies the id-less continuation stream without guessing
    /// from the final chunk.
    placements: HashMap<TransferSlot, DisplayPlacement>,
    current: Option<TransferSlot>,
    /// This subset has no replacement table. Reject reuse within one command
    /// instead of acknowledging and accumulating ambiguous placements.
    seen_display_ids: HashSet<u32>,
    pending_display_id: Option<u32>,
    reply_contexts: HashMap<TransferSlot, ReplyKeys>,
    last_reply_context: Option<ReplyKeys>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Self {
            inner: core::Assembler::new(CAPS),
            placements: HashMap::new(),
            current: None,
            seen_display_ids: HashSet::new(),
            pending_display_id: None,
            reply_contexts: HashMap::new(),
            last_reply_context: None,
        }
    }

    /// Drop all in-flight state — call when a block ends or the shell resets,
    /// so a half-uploaded image doesn't leak across commands.
    pub(crate) fn reset(&mut self) {
        self.reset_in_flight();
        self.seen_display_ids.clear();
    }

    pub(crate) fn reset_in_flight(&mut self) {
        self.inner.reset();
        self.placements.clear();
        self.reply_contexts.clear();
        self.last_reply_context = None;
        self.pending_display_id = None;
        self.current = None;
    }

    pub(crate) fn response_for(&mut self, payload: &[u8], status: &FeedStatus) -> Option<Vec<u8>> {
        let keys = self
            .last_reply_context
            .take()
            .or_else(|| ReplyKeys::scan(payload))?;
        response_for_keys(keys, status)
    }

    pub(crate) fn commit_display_id(&mut self) {
        if let Some(id) = self.pending_display_id.take() {
            self.seen_display_ids.insert(id);
        }
    }

    /// Parse one APC G payload. `payload` is the bytes between `\e_` and the
    /// terminating `\e\\` (i.e. starts with `G`). Returns the outcome the
    /// caller should act on.
    pub(crate) fn feed(&mut self, payload: &[u8]) -> Outcome {
        self.feed_at(payload, (0, 0))
    }

    /// Parse one APC payload while preserving the cursor at completion. The
    /// cursor is deliberately an argument rather than read
    /// from GTK here so the protocol state remains display-independent and
    /// directly testable.
    pub(crate) fn feed_at(&mut self, payload: &[u8], cursor: (i64, i64)) -> Outcome {
        self.last_reply_context = None;
        self.pending_display_id = None;
        if has_unsupported_static_placement_control(payload) {
            self.reset_in_flight();
            return Outcome::Skipped;
        }
        let parsed = core::parse_command(payload, &CAPS).ok();
        let starts_transfer = parsed
            .as_ref()
            .filter(|command| command.action.is_transmit() && !command.is_continuation())
            .map(|command| {
                (
                    transfer_slot(command.id),
                    display_placement(command, cursor),
                )
            });
        let continuation = parsed.as_ref().is_some_and(core::Command::is_continuation);

        let continuation_slot = self.current;
        let step = match self.inner.feed(payload) {
            Ok(step) => step,
            Err(error) => {
                let current_keys = ReplyKeys::scan(payload);
                let pending_keys =
                    continuation_slot.and_then(|slot| self.reply_contexts.get(&slot).copied());
                // Fail closed as one stream: otherwise core and the sidecars
                // can disagree after an interleaved bad transfer. The current
                // payload's explicit identity wins; an id-less bad final falls
                // back to the in-flight first chunk.
                self.reset_in_flight();
                self.last_reply_context = match (current_keys, pending_keys) {
                    (Some(current), _) if current.id.is_some() || current.number.is_some() => {
                        Some(current)
                    }
                    (current, Some(pending)) => Some(pending.merge_continuation(current)),
                    (current, None) => current,
                };
                return outcome_for(error);
            }
        };
        match step {
            Step::NotOurs => Outcome::Invalid,
            Step::NeedMore => {
                if let Some((slot, placement)) = starts_transfer {
                    self.current = Some(slot);
                    if let Some(keys) = ReplyKeys::scan(payload) {
                        self.reply_contexts.insert(slot, keys);
                    }
                    if let Some(placement) = placement {
                        self.placements.insert(slot, placement);
                    } else {
                        self.placements.remove(&slot);
                    }
                }
                Outcome::Pending
            }
            Step::Ready(assembled) => {
                let slot = transfer_slot(assembled.id);
                let mut placement = if continuation {
                    self.placements.remove(&slot)
                } else {
                    starts_transfer.and_then(|(_, placement)| placement)
                };
                if let Some(placement) = placement.as_mut() {
                    // c/r/C belong to the first chunk, but Kitty anchors the
                    // placement where the final chunk completes.
                    placement.cursor_col = cursor.0;
                    placement.cursor_row = cursor.1;
                }
                let display_id = assembled
                    .display
                    .then_some(assembled.id)
                    .flatten()
                    .filter(|id| *id != 0);
                if display_id.is_some_and(|id| {
                    self.seen_display_ids.contains(&id)
                        || self.seen_display_ids.len() >= MAX_SEEN_DISPLAY_IDS
                }) {
                    if let Some(placement) = placement.as_mut() {
                        placement.geometry_valid = false;
                    }
                }
                if self.current == Some(slot) {
                    self.current = None;
                }
                self.reply_contexts.remove(&slot);
                self.last_reply_context = Some(ReplyKeys::from_assembled(&assembled));
                let outcome = complete(assembled, placement);
                self.pending_display_id = matches!(outcome, Outcome::Complete { .. })
                    .then_some(display_id)
                    .flatten();
                if matches!(outcome, Outcome::Invalid | Outcome::Skipped) {
                    let reply = self.last_reply_context;
                    self.reset_in_flight();
                    self.last_reply_context = reply;
                }
                outcome
            }
            Step::Other {
                command,
                interrupted,
            } => {
                if interrupted {
                    log::debug!(
                        "kitty graphics: a={} dropped an in-flight chunked upload",
                        command.get("a").unwrap_or("?")
                    );
                }
                // Every non-transmit action resets the shared assembler.
                self.placements.clear();
                self.reply_contexts.clear();
                self.current = None;
                self.last_reply_context = ReplyKeys::scan(payload);
                if command.action == Action::Query {
                    query_outcome(&command)
                } else {
                    // Silently drop unsupported actions; the caller consumed
                    // the APC bytes already so libvte never sees them as
                    // garbage. The client still gets an ENOTSUP reply.
                    Outcome::Skipped
                }
            }
        }
    }
}

/// Map the shared parser's typed failure onto this app's wire codes:
/// [`Outcome::Invalid`] answers `EINVAL`, [`Outcome::Skipped`] answers
/// `ENOTSUP` — including size failures, which this responder reports as
/// "not supported" rather than a per-cause `ENOSPC`.
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

/// Decode a display transfer; transmit-only is refused because no store exists.
fn complete(assembled: Assembled, placement: Option<DisplayPlacement>) -> Outcome {
    let display = assembled.display;
    if !display {
        // This renderer has no persistent image store and cannot honour a
        // later a=p. Acknowledging transmit-only would promise state we drop.
        return Outcome::Skipped;
    }
    if display && placement.is_none_or(|placement| !placement.geometry_valid) {
        return Outcome::Skipped;
    }
    let encoded_source_backing_bytes = if assembled.format == Format::Png {
        assembled.bytes.len()
    } else {
        0
    };
    let texture = match texture_for(assembled) {
        Ok(texture) => texture,
        Err(error) => return outcome_for(error),
    };
    Outcome::Complete {
        texture,
        encoded_source_backing_bytes,
        placement,
    }
}

fn texture_for(assembled: Assembled) -> Result<gdk::Texture, Error> {
    if assembled.format == Format::Png {
        // The shared assembler already checked the IHDR geometry against CAPS,
        // so a tiny payload cannot make the GdkPixbuf loader allocate a huge
        // canvas.
        let bytes = glib::Bytes::from_owned(assembled.bytes);
        return gdk::Texture::from_bytes(&bytes).map_err(|_| Error::Invalid("PNG data"));
    }
    let (rgba, width, height) = assembled.into_rgba8()?;
    memory_texture(rgba, width, height)
}

/// Upload RGBA pixels as a GDK texture. `f=24` was already expanded to RGBA
/// with full opacity by the shared decoder.
fn memory_texture(rgba: Vec<u8>, width: u32, height: u32) -> Result<gdk::Texture, Error> {
    let stride = (width as usize).checked_mul(4).ok_or(Error::TooLarge)?;
    let width = i32::try_from(width).map_err(|_| Error::TooLarge)?;
    let height = i32::try_from(height).map_err(|_| Error::TooLarge)?;
    let bytes = glib::Bytes::from_owned(rgba);
    Ok(
        gdk::MemoryTexture::new(width, height, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
            .upcast(),
    )
}

/// Validate an `a=q` support probe. `kitten icat` (and other well-behaved
/// clients) transmit a tiny sample image with `a=q` and block until the
/// terminal answers, so probes must be validated rather than treated as an
/// unsupported display. Nothing is buffered or displayed; chunking (`m=`) is
/// ignored
/// because known clients probe in one APC.
fn query_outcome(command: &core::Command<'_>) -> Outcome {
    if let Err(error) = command.require_direct_transport() {
        return outcome_for(error);
    }
    let encoded = command.payload_b64.as_bytes();
    let significant = encoded.iter().filter(|b| !b.is_ascii_whitespace()).count();
    if significant > CAPS.max_encoded_bytes {
        return outcome_for(Error::TooLarge);
    }
    if significant == 0 {
        return Outcome::Invalid;
    }
    let decoded = match core::decode_base64(encoded, CAPS.max_decoded_bytes) {
        Ok(decoded) => decoded,
        Err(error) => return outcome_for(error),
    };
    let checked = match command.format {
        Format::Png => core::png_dimensions(&decoded, &CAPS).map(|_| ()),
        format => {
            let Some((width, height)) = command.declared() else {
                return Outcome::Invalid;
            };
            core::raw_layout(width, height, format, &CAPS).and_then(|layout| {
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

/// The identifiers a reply echoes back, scanned leniently.
///
/// Deliberately not read through [`core::parse_command`]: the commands that
/// most need an answer are the ones the shared parser *rejected*, and a client
/// blocked on its `i=` correlation key still owes to hear `EINVAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplyKeys {
    id: Option<u32>,
    number: Option<u32>,
    placement: Option<u32>,
    quiet: u8,
    quiet_specified: bool,
}

impl ReplyKeys {
    fn from_assembled(assembled: &Assembled) -> Self {
        Self {
            id: assembled.id,
            number: assembled.number,
            placement: assembled.placement,
            quiet: assembled.quiet,
            quiet_specified: true,
        }
    }

    /// `None` when the payload is not a graphics command, is not UTF-8, or
    /// carries more control data than the shared parser would have read.
    fn scan(payload: &[u8]) -> Option<Self> {
        let rest = payload.strip_prefix(b"G")?;
        // Split once at `;`: base64 padding belongs to the data section and
        // must never be read back as another control pair.
        let control = match memchr::memchr(b';', rest) {
            Some(index) => &rest[..index],
            None => rest,
        };
        if control.len() > CAPS.max_control_bytes {
            return None;
        }
        let control = std::str::from_utf8(control).ok()?;
        let mut keys = Self {
            id: None,
            number: None,
            placement: None,
            quiet: 0,
            quiet_specified: false,
        };
        for (key, value) in control.split(',').filter_map(|pair| pair.split_once('=')) {
            // Last write wins, matching how the shared parser resolves
            // duplicate keys.
            match key {
                "i" => keys.id = value.parse().ok(),
                "I" => keys.number = value.parse().ok(),
                "p" => keys.placement = value.parse().ok(),
                "q" => {
                    if let Some(quiet) = value.parse().ok().filter(|q| *q <= 2) {
                        keys.quiet = quiet;
                        keys.quiet_specified = true;
                    }
                }
                _ => {}
            }
        }
        Some(keys)
    }

    fn merge_continuation(self, continuation: Option<Self>) -> Self {
        continuation.map_or(self, |continuation| Self {
            id: continuation.id.or(self.id),
            number: continuation.number.or(self.number),
            placement: continuation.placement.or(self.placement),
            quiet: if continuation.quiet_specified {
                continuation.quiet
            } else {
                self.quiet
            },
            quiet_specified: self.quiet_specified || continuation.quiet_specified,
        })
    }
}

/// Build the PTY reply owed for a processed APC G payload, or `None` when the
/// protocol expects silence. Reply semantics follow ember, the family's most
/// complete responder:
/// - only commands carrying an `i=`/`I=` identifier are answered (the id is
///   the client's correlation key; kitty itself stays silent without one);
/// - `q=1` suppresses `OK`, `q=2` also suppresses errors;
/// - a non-zero `p=` placement id is echoed back.
///
/// Deliberate divergences from ember, kept small because this responder sits
/// on top of the shared structural assembler rather than a full placement
/// table:
/// - every unsupported-but-well-formed command answers `ENOTSUP` instead of
///   per-cause `ENOENT`/`ENOSPC` codes;
/// - a chunked upload rejected at any chunk keeps no tombstone for the aborted
///   id, so its remaining chunks are each rejected as "continuation without a
///   transfer in progress" — silently, since a legal continuation carries no
///   `i=` to answer;
/// - `q=` is read per-chunk from the payload being answered, not remembered
///   across an upload the way the shared assembler remembers it.
pub(crate) fn response_for(payload: &[u8], outcome: &FeedStatus) -> Option<Vec<u8>> {
    let keys = ReplyKeys::scan(payload)?;
    response_for_keys(keys, outcome)
}

fn response_for_keys(keys: ReplyKeys, outcome: &FeedStatus) -> Option<Vec<u8>> {
    if keys.id.is_none() && keys.number.is_none() {
        return None;
    }
    let body = match outcome {
        // Chunked uploads are answered once, after the final chunk.
        FeedStatus::Pending => return None,
        FeedStatus::Complete | FeedStatus::QueryOk => {
            if keys.quiet >= 1 {
                return None;
            }
            "OK"
        }
        FeedStatus::Invalid => {
            if keys.quiet >= 2 {
                return None;
            }
            "EINVAL:invalid graphics payload"
        }
        FeedStatus::Skipped => {
            if keys.quiet >= 2 {
                return None;
            }
            "ENOTSUP:action, format, transport, or size not supported"
        }
    };
    let mut fields = Vec::with_capacity(3);
    if let Some(id) = keys.id {
        fields.push(format!("i={id}"));
    }
    if let Some(number) = keys.number {
        fields.push(format!("I={number}"));
    }
    if let Some(placement) = keys.placement.filter(|p| *p != 0) {
        fields.push(format!("p={placement}"));
    }
    Some(format!("\x1b_G{};{body}\x1b\\", fields.join(",")).into_bytes())
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
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn feed(payload: &[u8]) -> Outcome {
        Assembler::new().feed(payload)
    }

    // ---- shared caps -----------------------------------------------------

    #[test]
    fn the_block_budget_is_the_shared_block_preset() {
        assert_eq!(MAX_PENDING_BYTES_PER_BLOCK, 16 * 1024 * 1024);
        assert_eq!(CAPS, Caps::BLOCK);
        assert_eq!(CAPS.max_dimension, 16_384);
    }

    #[test]
    fn tiny_images_are_bounded_by_object_count_and_fixed_cost() {
        let one = pending_image_bytes_after_admission(0, 0, 4, 0).unwrap();
        assert_eq!(one, RETAINED_BYTES_PER_IMAGE_OBJECT + 4);
        assert!(pending_image_bytes_after_admission(one, MAX_IMAGES_PER_BLOCK, 4, 0).is_none());
    }

    #[test]
    fn image_admission_rejects_byte_overflow_and_budget_overrun() {
        assert!(pending_image_bytes_after_admission(usize::MAX, 0, 1, 0).is_none());
        assert!(pending_image_bytes_after_admission(
            MAX_PENDING_BYTES_PER_BLOCK - RETAINED_BYTES_PER_IMAGE_OBJECT,
            0,
            1,
            0
        )
        .is_none());
    }

    #[test]
    fn png_encoded_backing_is_part_of_the_retained_budget() {
        let exact_backing = MAX_PENDING_BYTES_PER_BLOCK - RETAINED_BYTES_PER_IMAGE_OBJECT - 4;
        assert_eq!(
            pending_image_bytes_after_admission(0, 0, 4, exact_backing),
            Some(MAX_PENDING_BYTES_PER_BLOCK)
        );
        assert!(pending_image_bytes_after_admission(0, 0, 4, exact_backing + 1).is_none());
    }

    // ---- outcomes --------------------------------------------------------

    #[test]
    fn rejects_non_g_payload() {
        assert!(matches!(feed(b""), Outcome::Invalid));
        assert!(matches!(feed(b"X"), Outcome::Invalid));
    }

    #[test]
    fn unsupported_action_is_skipped() {
        assert!(matches!(feed(b"Ga=d,i=1;"), Outcome::Skipped));
        assert!(matches!(feed(b"Ga=p,i=1;"), Outcome::Skipped));
    }

    #[test]
    fn file_transport_is_skipped() {
        assert!(matches!(
            feed(b"Ga=T,f=100,t=f;L3RtcC9hLnBuZw=="),
            Outcome::Skipped
        ));
        // t=t and t=s are recognised and refused the same way.
        assert!(matches!(feed(b"Ga=T,f=100,t=t,i=1;AAAA"), Outcome::Skipped));
        assert!(matches!(feed(b"Ga=T,f=100,t=s,i=1;AAAA"), Outcome::Skipped));
    }

    #[test]
    fn chunked_assembly_accumulates_then_decodes_rgb_pixel() {
        let mut a = Assembler::new();
        // 1×1 red pixel as raw RGB (f=24): bytes 0xFF 0x00 0x00 -> "/wAA"
        // Split across two chunks via m=1 / m=0.
        let first = a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w");
        assert!(matches!(first, Outcome::Pending));
        // Continuations carry only m= (and optionally q=): repeating the
        // metadata would start a second upload for i=7.
        let final_payload = b"Gm=0;AA";
        let second = a.feed(final_payload);
        let status = second.status();
        match second {
            Outcome::Complete { .. } => {}
            _ => panic!("expected complete texture"),
        }
        assert_eq!(
            a.response_for(final_payload, &status).as_deref(),
            Some(b"\x1b_Gi=7;OK\x1b\\".as_slice())
        );
    }

    #[test]
    fn invalid_final_chunk_replies_with_first_chunk_identity_and_quiet_policy() {
        for (quiet, expects_reply) in [(0, true), (2, false)] {
            let mut assembler = Assembler::new();
            let first = format!("Ga=T,f=24,s=1,v=1,i=88,c=1,r=1,q={quiet},m=1;/w");
            assert!(matches!(assembler.feed(first.as_bytes()), Outcome::Pending));
            let final_payload = b"Gm=0;%%%";
            let outcome = assembler.feed(final_payload);
            assert!(matches!(outcome, Outcome::Invalid));
            let reply = assembler.response_for(final_payload, &outcome.status());
            if expects_reply {
                assert_eq!(
                    reply.as_deref(),
                    Some(b"\x1b_Gi=88;EINVAL:invalid graphics payload\x1b\\".as_slice())
                );
            } else {
                assert_eq!(reply, None);
            }
        }
    }

    #[test]
    fn invalid_continuation_quiet_value_overrides_first_chunk() {
        for (first_quiet, final_quiet, expects_reply) in [(0, 2, false), (2, 0, true)] {
            let mut assembler = Assembler::new();
            let first = format!("Ga=T,f=24,s=1,v=1,i=89,c=1,r=1,q={first_quiet},m=1;/w");
            assert!(matches!(assembler.feed(first.as_bytes()), Outcome::Pending));
            let final_payload = format!("Gm=0,q={final_quiet};%%%");
            let outcome = assembler.feed(final_payload.as_bytes());
            assert!(matches!(outcome, Outcome::Invalid));
            let reply = assembler.response_for(final_payload.as_bytes(), &outcome.status());
            assert_eq!(reply.is_some(), expects_reply);
            if let Some(reply) = reply {
                assert!(reply.starts_with(b"\x1b_Gi=89;EINVAL"));
            }
        }
    }

    #[test]
    fn interleaved_bad_identity_resets_stream_without_misattributing_reply() {
        let mut assembler = Assembler::new();
        assert!(matches!(
            assembler.feed(b"Ga=T,f=24,s=1,v=1,i=81,c=1,r=1,m=1;/w"),
            Outcome::Pending
        ));
        let bad = b"Ga=T,f=24,s=1,v=1,i=82,c=1,r=1;AA";
        let outcome = assembler.feed(bad);
        assert!(matches!(outcome, Outcome::Invalid));
        assert!(assembler
            .response_for(bad, &outcome.status())
            .is_some_and(|reply| reply.starts_with(b"\x1b_Gi=82;EINVAL")));

        let orphan = b"Gm=0;AA";
        let outcome = assembler.feed(orphan);
        assert!(matches!(outcome, Outcome::Invalid));
        assert_eq!(assembler.response_for(orphan, &outcome.status()), None);
    }

    #[test]
    fn chunked_display_keeps_first_chunk_geometry_and_final_chunk_cursor() {
        let mut assembler = Assembler::new();
        assert!(matches!(
            assembler.feed_at(b"Ga=T,f=24,s=1,v=1,i=71,c=5,r=3,C=1,m=1;/w", (17, 42)),
            Outcome::Pending
        ));
        let Outcome::Complete { placement, .. } = assembler.feed_at(b"Gm=0;AA", (1, 2)) else {
            panic!("expected completed display transfer");
        };
        assert_eq!(
            placement,
            Some(DisplayPlacement {
                columns: Some(5),
                rows: Some(3),
                geometry_valid: true,
                cursor_moves: Some(false),
                cursor_col: 1,
                cursor_row: 2,
            })
        );
    }

    #[test]
    fn unrelated_single_transfer_does_not_steal_chunked_geometry() {
        let mut assembler = Assembler::new();
        assert!(matches!(
            assembler.feed_at(b"Ga=T,f=24,s=1,v=1,i=81,c=6,r=4,m=1;/w", (14, 21)),
            Outcome::Pending
        ));

        let Outcome::Complete {
            placement: single, ..
        } = assembler.feed_at(b"Ga=T,f=32,s=1,v=1,i=82,c=1,r=1;AQIDBA==", (2, 3))
        else {
            panic!("expected unrelated single transfer");
        };
        assert_eq!(single.unwrap().cursor_col, 2);

        let Outcome::Complete {
            placement: chunked, ..
        } = assembler.feed_at(b"Gm=0;AA", (99, 100))
        else {
            panic!("expected original chunked transfer");
        };
        let chunked = chunked.unwrap();
        assert_eq!((chunked.columns, chunked.rows), (Some(6), Some(4)));
        assert_eq!((chunked.cursor_col, chunked.cursor_row), (99, 100));
    }

    #[test]
    fn single_chunk_display_uses_protocol_defaults_without_inventing_extent() {
        let Outcome::Complete { placement, .. } =
            Assembler::new().feed_at(b"Ga=T,f=32,s=1,v=1,i=72;AQIDBA==", (4, 9))
        else {
            panic!("expected completed display transfer");
        };
        assert_eq!(
            placement,
            Some(DisplayPlacement {
                columns: None,
                rows: None,
                geometry_valid: true,
                cursor_moves: Some(true),
                cursor_col: 4,
                cursor_row: 9,
            })
        );
    }

    #[test]
    fn malformed_or_overflowing_display_geometry_never_becomes_absent() {
        for control in [
            "c=bogus,r=1",
            "c=4294967296,r=1",
            "c=1,r=bogus",
            "c=1,r=4294967296",
            "c=1,r=1,C=2",
        ] {
            let payload = format!("Ga=T,f=32,s=1,v=1,i=74,{control};AQIDBA==");
            assert!(
                matches!(
                    Assembler::new().feed_at(payload.as_bytes(), (0, 0)),
                    Outcome::Skipped
                ),
                "{control}"
            );
        }
    }

    #[test]
    fn unsupported_static_placement_controls_fail_honestly() {
        for control in [
            "x=1", "y=1", "w=1", "h=1", "X=1", "Y=1", "z=-1", "p=9", "p=bogus", "U=0",
        ] {
            let payload = format!("Ga=T,f=32,s=1,v=1,i=75,c=1,r=1,{control};AQIDBA==");
            assert!(
                matches!(
                    Assembler::new().feed_at(payload.as_bytes(), (0, 0)),
                    Outcome::Skipped
                ),
                "{control}"
            );
        }
        // Explicit protocol defaults remain inside the static subset.
        assert!(matches!(
            feed(b"Ga=T,f=32,s=1,v=1,i=75,c=1,r=1,x=0,y=0,z=0,p=0;AQIDBA=="),
            Outcome::Complete { .. }
        ));
    }

    #[test]
    fn repeated_nonzero_image_id_is_refused_until_command_reset() {
        let payload = b"Ga=T,f=32,s=1,v=1,i=76,c=1,r=1;AQIDBA==";
        let mut assembler = Assembler::new();
        assert!(matches!(assembler.feed(payload), Outcome::Complete { .. }));
        assembler.commit_display_id();
        assert!(matches!(assembler.feed(payload), Outcome::Skipped));
        assembler.reset();
        assert!(matches!(assembler.feed(payload), Outcome::Complete { .. }));
    }

    #[test]
    fn backend_rejection_does_not_poison_image_id_retry() {
        let payload = b"Ga=T,f=32,s=1,v=1,i=77,c=1,r=1;AQIDBA==";
        let mut assembler = Assembler::new();
        assert!(matches!(assembler.feed(payload), Outcome::Complete { .. }));
        // No commit models Unified geometry/budget rejection.
        assert!(matches!(assembler.feed(payload), Outcome::Complete { .. }));
    }

    #[test]
    fn reset_and_failed_continuation_drop_geometry_sidecars() {
        let mut assembler = Assembler::new();
        assert!(matches!(
            assembler.feed_at(b"Ga=T,f=24,s=1,v=1,i=73,c=2,r=4,m=1;/w", (6, 8)),
            Outcome::Pending
        ));
        assert!(matches!(assembler.feed(b"Gm=0;%%%"), Outcome::Invalid));
        assert!(assembler.placements.is_empty());
        assert_eq!(assembler.current, None);

        assert!(matches!(
            assembler.feed_at(b"Ga=T,f=24,s=1,v=1,i=73,c=7,r=2,m=1;/w", (11, 13)),
            Outcome::Pending
        ));
        assembler.reset();
        assert!(assembler.placements.is_empty());
        assert_eq!(assembler.current, None);
    }

    #[test]
    fn a_continuation_that_repeats_metadata_is_invalid() {
        // Standardized by the shared parser: the protocol sends metadata on the
        // first chunk only, so a second one is a client that lost track of its
        // own transfer.
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w"),
            Outcome::Pending
        ));
        assert!(matches!(a.feed(b"Ga=T,i=7,m=0;AA"), Outcome::Invalid));
        // The abort clears the slot, so an honest retry still works.
        assert!(matches!(
            a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w"),
            Outcome::Pending
        ));
        assert!(matches!(a.feed(b"Gm=0;AA"), Outcome::Complete { .. }));
    }

    #[test]
    fn transmit_only_uploads_are_refused_without_a_persistent_store() {
        assert!(matches!(
            feed(b"Ga=t,f=32,s=1,v=1,i=4;AQIDBA=="),
            Outcome::Skipped
        ));
        assert!(matches!(
            feed(b"Ga=T,f=32,s=1,v=1,i=4;AQIDBA=="),
            Outcome::Complete { .. }
        ));
    }

    #[test]
    fn reset_drops_in_flight_uploads() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,f=32,s=1,v=1,i=9,m=1;AQID"),
            Outcome::Pending
        ));
        a.reset();
        // Nothing is in flight, so the continuation has nowhere to land.
        assert!(matches!(a.feed(b"Gm=0;BA=="), Outcome::Invalid));
    }

    // ---- standardizations adopted from the shared module -----------------

    #[test]
    fn format_now_defaults_to_rgba_not_png() {
        // Pre-hoist forge defaulted f= to PNG; the protocol default is RGBA,
        // so an f=-less command now means "raw RGBA, s=/v= required".
        assert!(matches!(
            feed(b"Ga=T,i=5,s=1,v=1;AQIDBA=="),
            Outcome::Complete { .. }
        ));
        // …and without s=/v= it is a malformed raw transfer, not a PNG.
        assert!(matches!(feed(b"Ga=T,i=5;AQIDBA=="), Outcome::Invalid));
    }

    #[test]
    fn raw_payload_length_must_match_exactly() {
        // Trailing slack used to be accepted; it no longer is.
        assert!(matches!(
            feed(b"Ga=T,f=32,i=13,s=1,v=1;AQIDBAU="),
            Outcome::Invalid
        ));
        assert!(matches!(
            feed(b"Ga=T,f=32,i=13,s=1,v=1;AQID"),
            Outcome::Invalid
        ));
        assert!(matches!(
            feed(b"Ga=T,f=32,i=13,s=1,v=1;AQIDBA=="),
            Outcome::Complete { .. }
        ));
    }

    #[test]
    fn non_standard_format_aliases_are_not_supported() {
        for alias in ["png", "jpeg", "rgba", "0"] {
            let payload = format!("Ga=T,i=1,f={alias},s=1,v=1;AQIDBA==");
            assert!(
                matches!(feed(payload.as_bytes()), Outcome::Skipped),
                "{alias}"
            );
        }
    }

    #[test]
    fn id_and_number_are_mutually_exclusive() {
        assert!(matches!(
            feed(b"Ga=T,f=32,i=1,I=2,s=1,v=1;AQIDBA=="),
            Outcome::Invalid
        ));
    }

    #[test]
    fn base64_rejects_garbage_interior_padding_and_impossible_lengths() {
        assert!(matches!(
            feed(b"Ga=T,f=32,i=1,s=1,v=1;!!!!"),
            Outcome::Invalid
        ));
        // Interior padding.
        assert!(matches!(
            feed(b"Ga=T,f=32,i=1,s=1,v=1;AQ=IDBA=="),
            Outcome::Invalid
        ));
        // len % 4 == 1 is not a possible base64 encoding.
        assert!(matches!(
            feed(b"Ga=T,f=32,i=1,s=1,v=1;AQIDB"),
            Outcome::Invalid
        ));
    }

    #[test]
    fn embedded_whitespace_is_stripped_across_chunks() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=T,f=32,i=14,s=1,v=1,m=1;AQ\r\n"),
            Outcome::Pending
        ));
        assert!(matches!(
            a.feed(b"Gm=0; ID  BA== \n"),
            Outcome::Complete { .. }
        ));
    }

    // ---- caps ------------------------------------------------------------

    #[test]
    fn oversized_raw_dimensions_are_skipped_without_overflow() {
        let cases: &[&[u8]] = &[
            b"Ga=T,f=32,s=4294967295,v=4294967295;AAAA",
            b"Ga=T,f=24,s=16384,v=16384;AAAA",
            b"Ga=T,f=32,s=16385,v=1;AAAA",
        ];
        for payload in cases {
            assert!(matches!(feed(payload), Outcome::Skipped));
        }
    }

    #[test]
    fn zero_dimensions_are_invalid_not_oversize() {
        assert!(matches!(
            feed(b"Ga=T,f=32,i=1,s=0,v=1;AAAA"),
            Outcome::Invalid
        ));
    }

    #[test]
    fn oversized_png_ihdr_is_skipped_before_gdk_decode() {
        let encoded = encode_base64(&png_header(CAPS.max_dimension + 1, 1));
        let mut payload = b"Ga=T,f=100;".to_vec();
        payload.extend_from_slice(&encoded);
        assert!(matches!(feed(&payload), Outcome::Skipped));
    }

    #[test]
    fn f100_that_is_not_a_png_is_invalid() {
        let mut payload = b"Ga=T,f=100,i=2;".to_vec();
        payload.extend_from_slice(&encode_base64(b"not a PNG at all"));
        assert!(matches!(feed(&payload), Outcome::Invalid));
    }

    // ---- a=q support probe -----------------------------------------------

    #[test]
    fn query_probe_validates_raw_pixels() {
        // kitten icat's support probe: 1×1 RGB sample under a=q.
        let mut a = Assembler::new();
        let payload = b"Ga=q,i=31,s=1,v=1,f=24,t=d;AAAA";
        let outcome = a.feed(payload);
        assert!(matches!(outcome, Outcome::QueryOk));
        assert_eq!(
            response_for(payload, &outcome.status()).as_deref(),
            Some(b"\x1b_Gi=31;OK\x1b\\".as_slice())
        );
    }

    #[test]
    fn query_probe_validates_a_png_sample() {
        let mut payload = b"Ga=q,i=32,f=100;".to_vec();
        payload.extend_from_slice(&encode_base64(&png_header(1, 1)));
        assert!(matches!(feed(&payload), Outcome::QueryOk));
    }

    #[test]
    fn query_probe_rejects_bad_or_oversize_payloads() {
        // Undecodable body.
        assert!(matches!(
            feed(b"Ga=q,i=1,s=1,v=1,f=24;!!!!"),
            Outcome::Invalid
        ));
        // Payload that does not match the advertised dimensions.
        assert!(matches!(
            feed(b"Ga=q,i=1,s=2,v=2,f=24;AAAA"),
            Outcome::Invalid
        ));
        // Empty body.
        assert!(matches!(feed(b"Ga=q,i=1,s=1,v=1,f=24;"), Outcome::Invalid));
        // Dimensions beyond the family cap.
        assert!(matches!(
            feed(b"Ga=q,i=1,s=16385,v=1,f=32;AAAA"),
            Outcome::Skipped
        ));
        // Non-direct transports are refused before anything is decoded.
        assert!(matches!(
            feed(b"Ga=q,i=1,s=1,v=1,f=24,t=f;AAAA"),
            Outcome::Skipped
        ));
    }

    #[test]
    fn a_probe_never_buffers_anything() {
        let mut a = Assembler::new();
        assert!(matches!(
            a.feed(b"Ga=q,i=31,s=1,v=1,f=24;AAAA"),
            Outcome::QueryOk
        ));
        // Nothing was stored, so a bare continuation has nowhere to land.
        assert!(matches!(a.feed(b"Gm=0;AAAA"), Outcome::Invalid));
    }

    // ---- responder -------------------------------------------------------

    #[test]
    fn responses_require_an_identifier() {
        assert_eq!(
            response_for(b"Ga=t,f=24,s=1,v=1;AAAA", &FeedStatus::Invalid),
            None
        );
        assert_eq!(response_for(b"Ga=T;AAAA", &FeedStatus::Skipped), None);
        assert_eq!(response_for(b"not graphics", &FeedStatus::Invalid), None);
    }

    #[test]
    fn responses_echo_ids_and_map_outcomes() {
        assert_eq!(
            response_for(b"Ga=t,i=41,s=1,v=1,f=24;AAAA", &FeedStatus::Skipped).as_deref(),
            Some(
                b"\x1b_Gi=41;ENOTSUP:action, format, transport, or size not supported\x1b\\"
                    .as_slice()
            )
        );
        assert_eq!(
            response_for(b"GI=13,a=T;AAAA", &FeedStatus::Invalid).as_deref(),
            Some(b"\x1b_GI=13;EINVAL:invalid graphics payload\x1b\\".as_slice())
        );
        assert_eq!(
            response_for(b"Ga=d,i=5,p=17;", &FeedStatus::Skipped).as_deref(),
            Some(
                b"\x1b_Gi=5,p=17;ENOTSUP:action, format, transport, or size not supported\x1b\\"
                    .as_slice()
            )
        );
    }

    #[test]
    fn responses_answer_commands_the_shared_parser_rejected() {
        // The reply keys are scanned leniently on purpose: a client blocked on
        // its i= must hear EINVAL even when the command never parsed.
        let payload = b"Ga=T,i=1,I=2,s=1,v=1,f=32;AQIDBA==";
        let outcome = feed(payload);
        assert!(matches!(outcome, Outcome::Invalid));
        assert_eq!(
            response_for(payload, &outcome.status()).as_deref(),
            Some(b"\x1b_Gi=1,I=2;EINVAL:invalid graphics payload\x1b\\".as_slice())
        );
    }

    #[test]
    fn responses_ignore_base64_padding_in_the_data_section() {
        // Splitting once at ';' keeps `=` padding out of the control scan.
        assert_eq!(
            response_for(b"Ga=T,i=8;AQIDBA==", &FeedStatus::Invalid).as_deref(),
            Some(b"\x1b_Gi=8;EINVAL:invalid graphics payload\x1b\\".as_slice())
        );
    }

    #[test]
    fn responses_wait_for_the_final_chunk() {
        assert_eq!(response_for(b"Ga=T,i=7,m=1;/w", &FeedStatus::Pending), None);
    }

    #[test]
    fn quiet_levels_suppress_responses() {
        assert_eq!(
            response_for(b"Ga=q,i=31,q=1,s=1,v=1,f=24;AAAA", &FeedStatus::QueryOk),
            None
        );
        // q=1 still reports errors …
        assert!(response_for(b"Ga=T,i=2,q=1;!!!!", &FeedStatus::Invalid).is_some());
        // … q=2 silences those too.
        assert_eq!(
            response_for(b"Ga=T,i=2,q=2;!!!!", &FeedStatus::Invalid),
            None
        );
        assert_eq!(response_for(b"Ga=d,i=3,q=2;", &FeedStatus::Skipped), None);
    }
}
