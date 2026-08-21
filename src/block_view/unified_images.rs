//! Probe-addressed Kitty images for Unified's persistent VTE.
//!
//! APC G bytes never reach libvte, so a successful display transfer must do
//! two jobs here: reserve the protocol-requested cell rows and mount a GTK
//! picture above those cells. Every reserved row receives a pane-authenticated
//! OSC 8 marker. Visibility and placement are recovered by probing those
//! markers, not by guessing from Unified chrome's optional row projection;
//! this keeps images stable through scrollback saturation and rewrap.

#[cfg(test)]
use gtk::glib;
use gtk::prelude::*;
use relm4::gtk;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

use super::kitty_graphics::{self, DisplayPlacement};

const URI_PREFIX: &str = "kitty-image://";
const NONCE_HEX_LEN: usize = 32;
// VTE discards hyperlink metadata on an ordinary trailing ASCII space. NBSP
// is visually blank but remains a real one-cell glyph, so its OSC 8 identity
// survives scrollback and rewrap for probing.
const MARKER_CELL: &[u8] = b"\xc2\xa0";

/// A decoded image that passed every grid and retained-memory gate. The
/// backend parks it until the protocol reply has been written, preserving the
/// trait's reply-before-admit contract.
pub(super) struct PendingImage {
    texture: gtk::gdk::Texture,
    placement: ResolvedPlacement,
    retained_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolvedPlacement {
    columns: u32,
    rows: u32,
    cursor_moves: bool,
    cursor_col: i64,
    cursor_row: i64,
    final_cursor_col: i64,
}

struct MountedImage {
    serial: u64,
    row_epoch: u64,
    picture: gtk::Picture,
    columns: u32,
    rows: u32,
    placement_col: u32,
    probe_col: u32,
    probe_epoch: u64,
    start_row: i64,
    end_row_exclusive: i64,
    retained_bytes: usize,
}

struct LayerState {
    nonce: Option<[u8; 16]>,
    mounted: Vec<MountedImage>,
    retained_bytes: usize,
    next_serial: u64,
    row_epoch: u64,
    alt_screen: bool,
    /// Leaving alt screen clears only the override. One later surface update
    /// must prove marker positions before any pre-TUI picture is shown again.
    rescan_armed: bool,
    /// Allow one bounded all-column relocation after resize/scroll. Streaming
    /// contents changes stay on the cached unique-column hot path.
    full_scan_pending: bool,
    column_epoch: u64,
}

#[derive(Clone)]
pub(super) struct UnifiedImageLayer {
    terminal: Terminal,
    surface: gtk::Fixed,
    state: Rc<RefCell<LayerState>>,
    row_projection: super::unified_chrome::ImageRowProjection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MarkerAddress {
    serial: u64,
    image_row: u32,
    placement_col: u32,
}

#[derive(Clone, Copy)]
enum SyncReason {
    Fast,
    Scroll,
    ColumnsChanged,
}

fn probe_epochs_stale(epochs: impl IntoIterator<Item = u64>, current: u64) -> bool {
    epochs.into_iter().any(|epoch| epoch != current)
}

fn nonce_hex(nonce: [u8; 16]) -> String {
    let mut encoded = String::with_capacity(NONCE_HEX_LEN);
    for byte in nonce {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn marker_uri(nonce: [u8; 16], serial: u64, image_row: u32, placement_col: u32) -> String {
    format!(
        "{URI_PREFIX}{}/{serial}/{image_row}/{placement_col}",
        nonce_hex(nonce)
    )
}

fn parse_marker_uri(uri: &str, expected_nonce: [u8; 16]) -> Option<MarkerAddress> {
    let rest = uri.strip_prefix(URI_PREFIX)?;
    let mut parts = rest.split('/');
    let nonce = parts.next()?;
    let serial = parts.next()?;
    let image_row = parts.next()?;
    let placement_col = parts.next()?;
    if parts.next().is_some()
        || nonce.len() != NONCE_HEX_LEN
        || nonce != nonce_hex(expected_nonce)
        || !canonical_decimal(serial)
        || !canonical_decimal(image_row)
        || !canonical_decimal(placement_col)
    {
        return None;
    }
    Some(MarkerAddress {
        serial: serial.parse().ok()?,
        image_row: image_row.parse().ok()?,
        placement_col: placement_col.parse().ok()?,
    })
}

fn canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Terminal bytes which attach one authenticated marker to every image row.
/// Exactly `rows - 1` LF bytes reserve the image's height, leaving the cursor
/// on its last row. `C=1` wraps the operation in DECSC/DECRC so the client's
/// explicit no-movement request wins.
fn placement_marker_bytes(
    nonce: [u8; 16],
    serial: u64,
    placement: ResolvedPlacement,
    zone_reopen: Option<&[u8]>,
) -> Vec<u8> {
    let rows = placement.rows.max(1);
    let mut bytes = Vec::with_capacity(rows as usize * 128);
    if !placement.cursor_moves {
        bytes.extend_from_slice(b"\x1b7");
    }
    for image_row in 0..rows {
        // CHA both before and after the marker clears delayed-wrap state even
        // when the placement begins in the last terminal column.
        let _ = write!(bytes, "\x1b[{}G", placement.cursor_col.saturating_add(1));
        let uri = marker_uri(
            nonce,
            serial,
            image_row,
            u32::try_from(placement.cursor_col).unwrap_or(0),
        );
        let _ = write!(bytes, "\x1b]8;;{uri}\x1b\\");
        bytes.extend_from_slice(MARKER_CELL);
        bytes.extend_from_slice(super::ZONE_MARKER_CLOSE);
        // Closing the image marker otherwise silently unlinks every following
        // cell in the same feed. Restore an active zone immediately.
        if let Some(zone_reopen) = zone_reopen {
            bytes.extend_from_slice(zone_reopen);
        }
        let _ = write!(bytes, "\x1b[{}G", placement.cursor_col.saturating_add(1));
        if image_row + 1 < rows {
            bytes.push(b'\n');
        }
    }
    if !placement.cursor_moves {
        bytes.extend_from_slice(b"\x1b8");
    } else {
        // Kitty's reference implementation advances x by the effective cell
        // width and y by rows-1. The LF loop above supplies y; this final CHA
        // supplies x so trailing text does not start underneath the image.
        let _ = write!(
            bytes,
            "\x1b[{}G",
            placement.final_cursor_col.saturating_add(1)
        );
    }
    bytes
}

fn aspect_columns_for_rows(
    rows: u32,
    cell_width_px: i64,
    cell_height_px: i64,
    texture_width_px: i32,
    texture_height_px: i32,
) -> Option<u32> {
    if rows == 0
        || cell_width_px <= 0
        || cell_height_px <= 0
        || texture_width_px <= 0
        || texture_height_px <= 0
    {
        return None;
    }
    let numerator = u128::from(rows)
        .checked_mul(u128::try_from(cell_height_px).ok()?)?
        .checked_mul(u128::try_from(texture_width_px).ok()?)?;
    let denominator = u128::try_from(texture_height_px)
        .ok()?
        .checked_mul(u128::try_from(cell_width_px).ok()?)?;
    let columns = numerator.checked_add(denominator.checked_sub(1)?)? / denominator;
    u32::try_from(columns.max(1)).ok()
}

fn resolve_placement(
    terminal: &Terminal,
    texture: &gtk::gdk::Texture,
    requested: DisplayPlacement,
) -> Option<ResolvedPlacement> {
    requested.geometry_valid.then_some(())?;
    let rows = requested.rows.filter(|rows| *rows != 0)?;
    let cursor_moves = requested.cursor_moves?;
    let terminal_columns = terminal.column_count();
    let terminal_rows = terminal.row_count();
    if terminal_columns <= 0
        || terminal_rows <= 0
        || requested.cursor_col < 0
        || requested.cursor_row < 0
        || rows > u32::try_from(terminal_rows).ok()?
    {
        return None;
    }
    // `r=` is authoritative for height. Pixel-height inference is observably
    // wrong for clients whose cell geometry differs from VTE's (40px / 17px
    // was measured as three rows while the client requested five). Width may
    // be inferred only when `c=` is absent because it does not reserve rows.
    let columns = requested
        .columns
        .filter(|columns| *columns != 0)
        .or_else(|| {
            aspect_columns_for_rows(
                rows,
                terminal.char_width(),
                terminal.char_height(),
                texture.width(),
                texture.height(),
            )
        })?;
    let end_col = requested.cursor_col.checked_add(i64::from(columns))?;
    if end_col > terminal_columns {
        return None;
    }
    // No ordinary terminal bytes may have moved the cursor between a chunked
    // transfer's first and final APC. A stale scrollback anchor cannot be
    // reached safely with cursor-addressing sequences, so fail closed.
    if terminal.cursor_position() != (requested.cursor_col, requested.cursor_row) {
        return None;
    }
    Some(ResolvedPlacement {
        columns,
        rows,
        cursor_moves,
        cursor_col: requested.cursor_col,
        cursor_row: requested.cursor_row,
        final_cursor_col: end_col.min(terminal_columns.saturating_sub(1)),
    })
}

fn placement_fits_columns(placement_col: u32, columns: u32, terminal_columns: i64) -> bool {
    terminal_columns > 0
        && i64::from(placement_col)
            .checked_add(i64::from(columns))
            .is_some_and(|end| end <= terminal_columns)
}

fn retained_by_floor(
    image_epoch: u64,
    start_row: i64,
    end_row_exclusive: i64,
    proof: super::unified_chrome::RetainedFloorProof,
) -> bool {
    debug_assert!(start_row <= end_row_exclusive);
    image_epoch != proof.row_epoch || end_row_exclusive > proof.retained_floor
}

fn marker_can_show(
    image_row: u32,
    image_rows: u32,
    placement_col: u32,
    image_columns: u32,
    terminal_columns: i64,
) -> bool {
    image_row < image_rows && placement_fits_columns(placement_col, image_columns, terminal_columns)
}

fn unique_probe_columns(
    placements: impl IntoIterator<Item = (u32, u32)>,
    terminal_columns: i64,
) -> BTreeSet<u32> {
    placements
        .into_iter()
        .filter(|(placement_col, columns)| {
            placement_fits_columns(*placement_col, *columns, terminal_columns)
        })
        .map(|(placement_col, _)| placement_col)
        .collect()
}

fn rebased_span(marker_ring_row: i64, image_row: u32, image_rows: u32) -> (i64, i64) {
    let start = marker_ring_row.saturating_sub(i64::from(image_row));
    (start, start.saturating_add(i64::from(image_rows)))
}

impl UnifiedImageLayer {
    pub(super) fn new(
        terminal: &Terminal,
        surface: &gtk::Fixed,
        nonce: Option<[u8; 16]>,
        row_projection: super::unified_chrome::ImageRowProjection,
    ) -> Self {
        let state = Rc::new(RefCell::new(LayerState {
            nonce,
            mounted: Vec::new(),
            retained_bytes: 0,
            next_serial: 1,
            row_epoch: 1,
            alt_screen: false,
            rescan_armed: true,
            full_scan_pending: true,
            column_epoch: 1,
        }));
        let layer = Self {
            terminal: terminal.clone(),
            surface: surface.clone(),
            state,
            row_projection,
        };
        layer.install_updates();
        layer
    }

    fn install_updates(&self) {
        let sync = {
            let terminal = self.terminal.downgrade();
            let surface = self.surface.downgrade();
            let state = self.state.clone();
            let row_projection = self.row_projection.clone();
            Rc::new(move |reason: SyncReason| {
                let (Some(terminal), Some(surface)) = (terminal.upgrade(), surface.upgrade())
                else {
                    return;
                };
                if let Ok(mut state) = state.try_borrow_mut() {
                    match reason {
                        SyncReason::Fast => {}
                        SyncReason::Scroll => {
                            let column_epoch = state.column_epoch;
                            let stale = probe_epochs_stale(
                                state.mounted.iter().map(|image| image.probe_epoch),
                                column_epoch,
                            );
                            state.full_scan_pending |= stale;
                        }
                        SyncReason::ColumnsChanged => {
                            state.column_epoch = state.column_epoch.wrapping_add(1);
                            state.full_scan_pending = true;
                        }
                    }
                }
                sync_layer(&terminal, &surface, &state, &row_projection);
            })
        };
        {
            let sync = sync.clone();
            self.terminal
                .connect_contents_changed(move |_| sync(SyncReason::Fast));
        }
        if let Some(adjustment) = self.terminal.vadjustment() {
            let sync_value = sync.clone();
            adjustment.connect_value_changed(move |_| sync_value(SyncReason::Scroll));
            let sync_changed = sync.clone();
            adjustment.connect_changed(move |_| sync_changed(SyncReason::Fast));
        }
        {
            let sync = sync.clone();
            self.terminal
                .connect_notify_local(Some("column-count"), move |_, _| {
                    sync(SyncReason::ColumnsChanged)
                });
        }
        for property in ["row-count", "font-scale"] {
            let sync = sync.clone();
            self.terminal
                .connect_notify_local(Some(property), move |_, _| sync(SyncReason::Fast));
        }
        let sync_map = sync.clone();
        self.surface
            .connect_map(move |_| sync_map(SyncReason::Scroll));
    }

    pub(super) fn prepare(
        &self,
        texture: gtk::gdk::Texture,
        encoded_source_backing_bytes: usize,
        requested: Option<DisplayPlacement>,
    ) -> Option<PendingImage> {
        // Rewrap invalidates absolute row epochs without invalidating visible
        // nonce markers. Rebase every currently visible stale placement before
        // a budget-pressure pass decides which still-unproven epoch to shed.
        sync_layer(
            &self.terminal,
            &self.surface,
            &self.state,
            &self.row_projection,
        );
        let mut state = self.state.borrow_mut();
        if state.alt_screen || state.nonce.is_none() {
            return None;
        }
        let placement = resolve_placement(&self.terminal, &texture, requested?)?;
        let pixel_bytes = (texture.width().max(0) as usize)
            .checked_mul(texture.height().max(0) as usize)?
            .checked_mul(4)?;
        let mut next = kitty_graphics::pending_image_bytes_after_admission(
            state.retained_bytes,
            state.mounted.len(),
            pixel_bytes,
            encoded_source_backing_bytes,
        );
        if next.is_none() {
            // A rewrap epoch can leave old absolute row spans intentionally
            // quarantined until their OSC markers become visible again. They
            // must not permanently lock the bounded image budget: under real
            // admission pressure, retire only those stale-epoch placements,
            // never a current proven span or a frame-based LRU victim.
            let epoch = state.row_epoch;
            let mut retained_bytes = state.retained_bytes;
            let mut removed = Vec::new();
            state.mounted.retain(|image| {
                if image.row_epoch == epoch {
                    true
                } else {
                    removed.push(image.picture.clone());
                    retained_bytes = retained_bytes.saturating_sub(image.retained_bytes);
                    false
                }
            });
            state.retained_bytes = retained_bytes;
            drop(state);
            for picture in removed {
                self.surface.remove(&picture);
            }
            state = self.state.borrow_mut();
            next = kitty_graphics::pending_image_bytes_after_admission(
                state.retained_bytes,
                state.mounted.len(),
                pixel_bytes,
                encoded_source_backing_bytes,
            );
        }
        let next = next?;
        Some(PendingImage {
            texture,
            placement,
            retained_bytes: next.saturating_sub(state.retained_bytes),
        })
    }

    pub(super) fn admit(&self, pending: PendingImage, zone_reopen: Option<&[u8]>) {
        let (nonce, serial, row_epoch) = {
            let mut state = self.state.borrow_mut();
            if state.alt_screen {
                return;
            }
            let Some(nonce) = state.nonce else {
                return;
            };
            let serial = state.next_serial;
            state.next_serial = state
                .next_serial
                .checked_add(1)
                .expect("Unified Kitty placement serial exhausted");
            (nonce, serial, state.row_epoch)
        };
        let bytes = placement_marker_bytes(nonce, serial, pending.placement, zone_reopen);
        self.terminal.feed(&bytes);

        let picture = gtk::Picture::for_paintable(&pending.texture);
        picture.set_content_fit(gtk::ContentFit::Fill);
        picture.set_can_shrink(true);
        picture.set_can_target(false);
        picture.set_focusable(false);
        picture.set_visible(false);
        self.surface.put(&picture, 0.0, 0.0);
        {
            let mut state = self.state.borrow_mut();
            let column_epoch = state.column_epoch;
            state.retained_bytes = state.retained_bytes.saturating_add(pending.retained_bytes);
            state.mounted.push(MountedImage {
                serial,
                row_epoch,
                picture,
                columns: pending.placement.columns,
                rows: pending.placement.rows,
                placement_col: u32::try_from(pending.placement.cursor_col).unwrap_or(0),
                probe_col: u32::try_from(pending.placement.cursor_col).unwrap_or(0),
                probe_epoch: column_epoch,
                start_row: pending.placement.cursor_row,
                end_row_exclusive: pending
                    .placement
                    .cursor_row
                    .saturating_add(i64::from(pending.placement.rows)),
                retained_bytes: pending.retained_bytes,
            });
        }
        sync_layer(
            &self.terminal,
            &self.surface,
            &self.state,
            &self.row_projection,
        );
    }

    /// Retire only placements whose complete row span is proven below a
    /// retained floor. This is fed by UnifiedChrome's exact ForwardTrim,
    /// capacity-reconciliation and ED3 cutoff calculations; widget visibility
    /// is never row-retention evidence.
    pub(super) fn retire_before(&self, proof: super::unified_chrome::RetainedFloorProof) {
        let mut state = self.state.borrow_mut();
        let mut retained_bytes = state.retained_bytes;
        let mut removed = Vec::new();
        state.mounted.retain(|image| {
            if retained_by_floor(
                image.row_epoch,
                image.start_row,
                image.end_row_exclusive,
                proof,
            ) {
                true
            } else {
                removed.push(image.picture.clone());
                retained_bytes = retained_bytes.saturating_sub(image.retained_bytes);
                false
            }
        });
        state.retained_bytes = retained_bytes;
        drop(state);
        for picture in removed {
            self.surface.remove(&picture);
        }
    }

    pub(super) fn set_row_epoch(&self, row_epoch: u64) {
        self.state.borrow_mut().row_epoch = row_epoch;
    }

    pub(super) fn hard_reset(&self) {
        let mut state = self.state.borrow_mut();
        let removed = state
            .mounted
            .drain(..)
            .map(|image| image.picture)
            .collect::<Vec<_>>();
        state.retained_bytes = 0;
        state.row_epoch = state.row_epoch.wrapping_add(1);
        state.rescan_armed = true;
        drop(state);
        for picture in removed {
            self.surface.remove(&picture);
        }
        self.surface.set_visible(false);
    }

    pub(super) fn set_alt_screen(&self, entering: bool) {
        let mut state = self.state.borrow_mut();
        state.alt_screen = entering;
        state.rescan_armed = false;
        let pictures = state
            .mounted
            .iter()
            .map(|image| image.picture.clone())
            .collect::<Vec<_>>();
        drop(state);
        for picture in pictures {
            picture.set_visible(false);
        }
        self.surface.set_visible(false);
        if !entering {
            // rmcup may be the final byte for a while. Schedule both halves of
            // the fail-closed gate so a static restored main screen becomes
            // visible without depending on unrelated later output.
            let terminal = self.terminal.downgrade();
            let surface = self.surface.downgrade();
            let state = self.state.clone();
            let projection = self.row_projection.clone();
            gtk::glib::idle_add_local_once(move || {
                let (Some(terminal), Some(surface)) = (terminal.upgrade(), surface.upgrade())
                else {
                    return;
                };
                sync_layer(&terminal, &surface, &state, &projection);
                let terminal = terminal.downgrade();
                let surface = surface.downgrade();
                gtk::glib::idle_add_local_once(move || {
                    if let (Some(terminal), Some(surface)) = (terminal.upgrade(), surface.upgrade())
                    {
                        sync_layer(&terminal, &surface, &state, &projection);
                    }
                });
            });
        }
    }
}

fn image_probe_gate(rescan_armed: &mut bool) -> bool {
    if !*rescan_armed {
        *rescan_armed = true;
        false
    } else {
        true
    }
}

fn sync_layer(
    terminal: &Terminal,
    surface: &gtk::Fixed,
    state: &Rc<RefCell<LayerState>>,
    row_projection: &super::unified_chrome::ImageRowProjection,
) {
    // GTK visibility/map notifications may synchronously re-enter this
    // function. A nested pass is redundant and must never panic across the C
    // signal boundary, so it simply yields to the outer authoritative pass.
    let Ok(mut state) = state.try_borrow_mut() else {
        return;
    };
    if state.alt_screen || state.nonce.is_none() || state.mounted.is_empty() {
        drop(state);
        surface.set_visible(false);
        return;
    }
    if !image_probe_gate(&mut state.rescan_armed) {
        // The first callback after rmcup only arms probing. This prevents the
        // synchronous screen swap from resurrecting stale pre-TUI positions.
        drop(state);
        surface.set_visible(false);
        return;
    }
    let nonce = state.nonce.expect("checked above");
    let cell_width_px = terminal.char_width().max(1);
    let cell_height_px = terminal.char_height().max(1);
    let columns = terminal.column_count().max(0);
    let rows = terminal.row_count().max(0);
    if columns == 0 || rows == 0 {
        drop(state);
        surface.set_visible(false);
        return;
    }
    let Some(adjustment) = terminal.vadjustment() else {
        drop(state);
        surface.set_visible(false);
        return;
    };
    let border = gtk::prelude::ScrollableExt::border(terminal);
    let (content_x, content_y) = border.as_ref().map_or((0.0, 0.0), |border| {
        (f64::from(border.left()), f64::from(border.top()))
    });
    // The image surface is intentionally hidden when no marker is reachable;
    // using its own allocation would make hidden -> height 0 -> no probes ->
    // hidden a permanent state. The VTE is always the measured live child.
    let visible_rows = rows.min(
        ((f64::from(terminal.height()) - content_y).max(0.0) / cell_height_px as f64).ceil() as i64,
    );
    let mounted_serials = state
        .mounted
        .iter()
        .map(|image| image.serial)
        .collect::<std::collections::HashSet<_>>();
    let mounted_origins = state
        .mounted
        .iter()
        .map(|image| (image.serial, image.placement_col))
        .collect::<HashMap<_, _>>();
    // Marker cells are written at the authenticated placement column and that
    // same column is carried in the URI. Probing every terminal column made a
    // 200×60 pane issue 12,000 VTE hyperlink lookups on every output chunk.
    // Deduplicate mounted origins instead: common one-image output is exactly
    // one probe per visible row, and the hard image-count cap bounds the rest.
    let full_scan = std::mem::take(&mut state.full_scan_pending);
    let probe_columns = if full_scan {
        (0..u32::try_from(columns).unwrap_or(0)).collect::<BTreeSet<_>>()
    } else {
        unique_probe_columns(
            state
                .mounted
                .iter()
                .map(|image| (image.probe_col, image.columns)),
            columns,
        )
    };
    let mut hits = HashMap::<u64, (u32, u32, f64, Option<(u64, i64)>)>::new();
    'rows: for band in 0..visible_rows {
        let y = content_y + (band as f64 + 0.5) * cell_height_px as f64;
        for &column in &probe_columns {
            let x = content_x + (f64::from(column) + 0.5) * cell_width_px as f64;
            let Some(address) = terminal
                .check_hyperlink_at(x, y)
                .as_deref()
                .and_then(|uri| parse_marker_uri(uri, nonce))
            else {
                continue;
            };
            if !mounted_serials.contains(&address.serial)
                || mounted_origins.get(&address.serial) != Some(&address.placement_col)
                || hits.contains_key(&address.serial)
            {
                continue;
            }
            let Some(adjustment_row) = super::unified_chrome::adjustment_row_at_probe_band(
                adjustment.value(),
                band,
                cell_height_px,
            ) else {
                continue;
            };
            let Some(marker_y) = super::unified_chrome::adjustment_row_y_px(
                adjustment_row,
                adjustment.value(),
                cell_height_px,
            ) else {
                continue;
            };
            hits.insert(
                address.serial,
                (
                    address.image_row,
                    column,
                    marker_y,
                    row_projection.ring_row_at_probe_band(adjustment.value(), band, cell_height_px),
                ),
            );
            if hits.len() == mounted_serials.len() {
                break 'rows;
            }
        }
    }

    let current_column_epoch = state.column_epoch;
    let mut any_visible = false;
    for image in &mut state.mounted {
        let Some((image_row, placement_col, marker_y, ring_row)) = hits.get(&image.serial).copied()
        else {
            image.picture.set_visible(false);
            continue;
        };
        if !marker_can_show(image_row, image.rows, placement_col, image.columns, columns) {
            image.picture.set_visible(false);
            continue;
        }
        // A stale epoch never hides a nonce-proven visible marker. It only
        // makes the old absolute span ineligible for floor retirement. When
        // chrome has a current projection, refresh that span in place.
        if let Some((row_epoch, marker_ring_row)) = ring_row {
            let (start_row, end_row_exclusive) =
                rebased_span(marker_ring_row, image_row, image.rows);
            image.row_epoch = row_epoch;
            image.start_row = start_row;
            image.end_row_exclusive = end_row_exclusive;
        }
        image.probe_col = placement_col;
        image.probe_epoch = current_column_epoch;
        let width = i32::try_from(image.columns)
            .unwrap_or(i32::MAX)
            .saturating_mul(i32::try_from(cell_width_px).unwrap_or(i32::MAX));
        let height = i32::try_from(image.rows)
            .unwrap_or(i32::MAX)
            .saturating_mul(i32::try_from(cell_height_px).unwrap_or(i32::MAX));
        // Both dimensions are required inside GtkFixed: -1 can collapse a
        // shrinkable Picture to zero in the unconstrained dimension.
        image.picture.set_size_request(width.max(1), height.max(1));
        let x = content_x + f64::from(placement_col) * cell_width_px as f64;
        let y = content_y + marker_y - f64::from(image_row) * cell_height_px as f64;
        surface.move_(&image.picture, x, y);
        image.picture.set_visible(true);
        any_visible = true;
    }
    // `set_visible(true)` can synchronously emit map and call us again.
    drop(state);
    surface.set_visible(any_visible);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: [u8; 16] = [0xab; 16];

    fn placement(rows: u32, cursor_moves: bool) -> ResolvedPlacement {
        ResolvedPlacement {
            columns: 5,
            rows,
            cursor_moves,
            cursor_col: 7,
            cursor_row: 11,
            final_cursor_col: 12,
        }
    }

    #[test]
    fn marker_uri_is_canonical_and_nonce_scoped() {
        let uri = marker_uri(NONCE, 19, 2, 7);
        assert_eq!(
            parse_marker_uri(&uri, NONCE),
            Some(MarkerAddress {
                serial: 19,
                image_row: 2,
                placement_col: 7,
            })
        );
        assert_eq!(parse_marker_uri(&uri, [0xcd; 16]), None);
        for forged in [
            "kitty-image://ABABABABABABABABABABABABABABABAB/19/2/7",
            "kitty-image://abababababababababababababababab/019/2/7",
            "kitty-image://abababababababababababababababab/19/2/7/tail",
        ] {
            assert_eq!(parse_marker_uri(forged, NONCE), None, "{forged}");
        }
    }

    #[test]
    fn marker_stream_reserves_rows_and_reopens_zone_after_every_closer() {
        let zone = b"\x1b]8;;block://zone/4\x1b\\";
        let bytes = placement_marker_bytes(NONCE, 9, placement(4, true), Some(zone));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 3);
        assert!(!bytes.starts_with(b"\x1b7"));
        assert!(!bytes.ends_with(b"\x1b8"));
        assert!(bytes.ends_with(b"\x1b[13G"));
        assert_eq!(
            bytes
                .windows(super::super::ZONE_MARKER_CLOSE.len() + zone.len())
                .filter(|window| {
                    window.starts_with(super::super::ZONE_MARKER_CLOSE) && window.ends_with(zone)
                })
                .count(),
            4
        );
    }

    #[test]
    fn no_cursor_movement_wraps_the_same_row_reservation_in_save_restore() {
        let bytes = placement_marker_bytes(NONCE, 10, placement(3, false), None);
        assert!(bytes.starts_with(b"\x1b7"));
        assert!(bytes.ends_with(b"\x1b8"));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
    }

    #[test]
    fn resize_hides_an_image_that_no_longer_fits_without_moving_its_x_origin() {
        assert!(placement_fits_columns(7, 5, 12));
        assert!(!placement_fits_columns(7, 5, 11));
        // The marker may be probed in a different physical column after a
        // rewrap. The authenticated URI retains the requested x origin.
        let address = parse_marker_uri(&marker_uri(NONCE, 3, 1, 7), NONCE).unwrap();
        assert_eq!(address.placement_col, 7);
    }

    #[test]
    fn row_only_geometry_preserves_aspect_across_non_square_cells() {
        assert_eq!(aspect_columns_for_rows(5, 8, 17, 80, 40), Some(22));
        assert_eq!(aspect_columns_for_rows(1, 20, 10, 1, 100), Some(1));
        assert_eq!(aspect_columns_for_rows(0, 8, 17, 80, 40), None);
        assert_eq!(aspect_columns_for_rows(5, 0, 17, 80, 40), None);
    }

    #[test]
    fn retained_floor_retires_only_complete_spans_in_the_same_epoch() {
        let proof = super::super::unified_chrome::RetainedFloorProof {
            row_epoch: 7,
            retained_floor: 30,
        };
        assert!(!retained_by_floor(7, 10, 30, proof));
        assert!(retained_by_floor(7, 29, 33, proof));
        assert!(retained_by_floor(8, 10, 20, proof));
    }

    #[test]
    fn a_nonce_proven_marker_survives_ed3_epoch_and_new_image_coexistence() {
        // Epoch is deliberately absent from the visibility gate: ED3 and
        // rewrap invalidate old absolute spans, not the marker itself.
        assert!(marker_can_show(1, 3, 7, 5, 20));
        assert!(marker_can_show(0, 1, 0, 1, 20));
        assert!(!marker_can_show(3, 3, 7, 5, 20));
        assert!(!marker_can_show(1, 3, 18, 5, 20));
        assert_eq!(rebased_span(104, 2, 5), (102, 107));
    }

    #[test]
    fn alt_leave_two_stage_gate_reprobes_without_terminal_output() {
        let mut armed = false;
        assert!(!image_probe_gate(&mut armed));
        assert!(armed);
        assert!(image_probe_gate(&mut armed));
    }

    #[test]
    fn probe_columns_are_deduplicated_and_out_of_bounds_origins_are_skipped() {
        // This pure selector is the hot-path bound: 60 visible rows with this
        // set perform 120 probes, not 60*200 and not 60*4.
        let images = [(7, 5), (7, 1), (31, 2), (39, 4)];
        assert_eq!(
            unique_probe_columns(images, 40)
                .into_iter()
                .collect::<Vec<_>>(),
            [7, 31]
        );
    }

    #[test]
    fn ordinary_scroll_keeps_fast_path_after_every_probe_column_is_current() {
        assert!(!probe_epochs_stale([7, 7, 7], 7));
        assert!(probe_epochs_stale([7, 6, 7], 7));
    }

    #[test]
    #[ignore = "requires a mapped GTK/VTE surface; run under Xvfb"]
    fn real_vte_keeps_nonzero_marker_column_through_narrow_wide_rewrap() {
        if gtk::init().is_err() {
            return;
        }
        let terminal = Terminal::new();
        terminal.set_allow_hyperlink(true);
        terminal.set_scrollback_lines(100);
        terminal.set_size(20, 6);
        let window = gtk::Window::builder()
            .default_width(320)
            .default_height(160)
            .child(&terminal)
            .build();
        window.present();
        let settle = || {
            let context = glib::MainContext::default();
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_millis(100) {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle();
        let placement = ResolvedPlacement {
            columns: 3,
            rows: 3,
            cursor_moves: true,
            cursor_col: 7,
            cursor_row: terminal.cursor_position().1,
            final_cursor_col: 10,
        };
        let marker_bytes = placement_marker_bytes(NONCE, 44, placement, None);
        terminal.feed(&marker_bytes);
        settle();
        assert_eq!(
            terminal.cursor_position(),
            (10, 2),
            "C=0 advances right by columns and down by rows-1"
        );

        let observed_rows = |terminal: &Terminal| {
            let border = gtk::prelude::ScrollableExt::border(terminal);
            let content_x = border.as_ref().map_or(0.0, |b| f64::from(b.left()));
            let content_y = border.as_ref().map_or(0.0, |b| f64::from(b.top()));
            let x = content_x + 7.5 * terminal.char_width().max(1) as f64;
            (0..terminal.row_count())
                .filter_map(|row| {
                    terminal
                        .check_hyperlink_at(
                            x,
                            content_y + (row as f64 + 0.5) * terminal.char_height().max(1) as f64,
                        )
                        .as_deref()
                        .and_then(|uri| parse_marker_uri(uri, NONCE))
                        .filter(|address| address.serial == 44)
                        .map(|address| address.image_row)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(observed_rows(&terminal), [0, 1, 2]);
        terminal.set_size(12, 6);
        assert_eq!(terminal.column_count(), 12, "narrow rewrap was applied");
        settle();
        assert_eq!(observed_rows(&terminal), [0, 1, 2]);
        terminal.set_size(30, 6);
        assert_eq!(terminal.column_count(), 30, "wide rewrap was applied");
        settle();
        assert_eq!(observed_rows(&terminal), [0, 1, 2]);

        terminal.reset(true, true);
        terminal.set_size(40, 6);
        let long_line = ResolvedPlacement {
            columns: 3,
            rows: 1,
            cursor_moves: true,
            cursor_col: 30,
            cursor_row: terminal.cursor_position().1,
            final_cursor_col: 33,
        };
        terminal.feed(&placement_marker_bytes(NONCE, 45, long_line, None));
        settle();
        let marker_position = |terminal: &Terminal| {
            let border = gtk::prelude::ScrollableExt::border(terminal);
            let content_x = border.as_ref().map_or(0.0, |b| f64::from(b.left()));
            let content_y = border.as_ref().map_or(0.0, |b| f64::from(b.top()));
            (0..terminal.row_count()).find_map(|row| {
                (0..terminal.column_count()).find_map(|col| {
                    terminal
                        .check_hyperlink_at(
                            content_x + (col as f64 + 0.5) * terminal.char_width().max(1) as f64,
                            content_y + (row as f64 + 0.5) * terminal.char_height().max(1) as f64,
                        )
                        .as_deref()
                        .and_then(|uri| parse_marker_uri(uri, NONCE))
                        .filter(|address| address.serial == 45)
                        .map(|_| (row, col))
                })
            })
        };
        assert_eq!(marker_position(&terminal), Some((0, 30)));
        terminal.set_size(20, 6);
        assert_eq!(terminal.column_count(), 20);
        assert_eq!(marker_position(&terminal), Some((1, 10)));
        terminal.set_size(40, 6);
        assert_eq!(marker_position(&terminal), Some((0, 30)));
        window.close();
    }
}
