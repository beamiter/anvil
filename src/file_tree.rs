//! Sidebar file browser: a lazy-loading `TreeView` rooted at the active tab's
//! working directory (falling back to `$HOME`). Directories expand on demand;
//! activating a file inserts its shell-quoted path into the active terminal.
//! Ports forge's `ui/file_tree.rs` to anvil's relm4 structure.
//!
//! GTK4 deprecated the TreeView/TreeStore family in 4.10 in favor of the new
//! list/column views, but they remain fully functional and a ColumnView rewrite
//! is out of scope; suppress the deprecation lints module-wide.
#![allow(deprecated)]

use relm4::gtk;

use gtk::glib;
use gtk::prelude::*;
use gtk::{
    CellRendererPixbuf, CellRendererText, TreeIter, TreeModelFilter, TreeRowReference, TreeStore,
    TreeView, TreeViewColumn,
};
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

// TreeStore column indices.
pub(crate) const COL_NAME: u32 = 0;
pub(crate) const COL_PATH: u32 = 1;
pub(crate) const COL_IS_DIR: u32 = 2;
pub(crate) const COL_ICON: u32 = 3;
pub(crate) const COL_TOOLTIP: u32 = 4;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(16);
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_FILE_NAME_DISPLAY_BYTES: usize = 512;
const MAX_FILE_PATH_DISPLAY_BYTES: usize = 4 * 1024;
const MAX_FILE_PATH_IDENTITY_BYTES: usize = 64 * 1024;
const PATH_IDENTITY_PREFIX: &str = "unix-path-v1:";
const MAX_CONCURRENT_SCANS: usize = 16;
static ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);

struct ScanPermit;

impl ScanPermit {
    fn acquire() -> io::Result<Self> {
        ACTIVE_SCANS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_SCANS).then_some(active + 1)
            })
            .map(|_| Self)
            .map_err(|_| io::Error::other("file-tree scan concurrency limit reached"))
    }
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        ACTIVE_SCANS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct FileEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

impl FileEntry {
    /// `remote_fs` builds entries from probe output; the fields stay private
    /// so every entry is constructed through one place.
    pub(crate) fn new(name: String, path: PathBuf, is_dir: bool) -> Self {
        Self { name, path, is_dir }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[allow(dead_code)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub(crate) fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// Directories first, then case-insensitive name order — the one comparator
/// behind scans, inserts, and merge refreshes.
fn entry_cmp(a: &FileEntry, b: &FileEntry) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(entry_cmp);
}

pub(crate) fn scan_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)?
        .take(MAX_DIRECTORY_ENTRIES)
        .flatten()
    {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        entries.push(FileEntry {
            name: display_os_str(&entry.file_name()),
            // Do not follow directory symlinks: they can create cycles or turn
            // one expansion into a scan outside the tree the user selected.
            is_dir: file_type.is_dir(),
            path,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Encode a Linux path for storage in GTK's string-only tree model without
/// ever treating its bytes as UTF-8. The versioned hex form is reversible and
/// explicitly bounded before its 2x expansion.
pub(crate) fn encode_path_identity(path: &Path) -> Option<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let encoded_len = PATH_IDENTITY_PREFIX
        .len()
        .checked_add(bytes.len().checked_mul(2)?)?;
    let mut encoded = String::with_capacity(encoded_len);
    encoded.push_str(PATH_IDENTITY_PREFIX);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Some(encoded)
}

/// Recover the exact Linux path bytes stored by [`encode_path_identity`].
/// Malformed, unversioned, or oversized model values are rejected.
pub(crate) fn decode_path_identity(encoded: &str) -> Option<PathBuf> {
    let hex = encoded.strip_prefix(PATH_IDENTITY_PREFIX)?;
    if hex.len() % 2 != 0 || hex.len() / 2 > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Render valid UTF-8 normally and make every invalid byte visible. Escaping
/// literal backslashes keeps a real `\\xff` name distinct from a raw `0xff`.
pub(crate) fn display_os_str(value: &OsStr) -> String {
    let mut remaining = value.as_bytes();
    let mut display = String::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_valid_display(&mut display, valid);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let valid = std::str::from_utf8(&remaining[..valid_len])
                    .expect("Utf8Error::valid_up_to must end on a UTF-8 boundary");
                push_valid_display(&mut display, valid);
                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| remaining.len().saturating_sub(valid_len));
                for &byte in &remaining[valid_len..valid_len + invalid_len] {
                    use std::fmt::Write as _;
                    let _ = write!(display, "\\x{byte:02x}");
                }
                remaining = &remaining[valid_len + invalid_len..];
            }
        }
    }
    display
}

fn push_valid_display(display: &mut String, valid: &str) {
    for ch in valid.chars() {
        if ch == '\\' {
            display.push_str("\\\\");
        } else {
            display.push(ch);
        }
    }
}

pub(crate) fn is_notebook_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().ends_with(b".jtnb.md")
}

/// The launch authority behind the file-tree header's terminal button.
/// Local trees carry their exact root as the new pane cwd; remote trees carry
/// only a freshly validated managed profile. A remote tree path is
/// intentionally absent because ssh/docker startup decides its own directory.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FileTreeTerminalTarget {
    Local(String),
    Remote(crate::config::RemoteHost),
}

pub(crate) fn terminal_target(
    location: &crate::remote_fs::FsLocation,
    root: &Path,
    hosts: &[crate::config::RemoteHost],
) -> Result<FileTreeTerminalTarget, &'static str> {
    match location {
        crate::remote_fs::FsLocation::Local => {
            if !root.is_absolute() {
                return Err("The current file-tree directory is unavailable.");
            }
            let cwd = root.to_str().ok_or(
                "The current file-tree directory contains non-UTF-8 bytes and cannot be used as a terminal cwd.",
            )?;
            Ok(FileTreeTerminalTarget::Local(cwd.to_string()))
        }
        crate::remote_fs::FsLocation::Remote(index) => {
            crate::config::checked_remote_host(hosts, *index)
                .cloned()
                .map(FileTreeTerminalTarget::Remote)
        }
    }
}

/// Authority captured when a user opens a delayed file-operation dialog.
/// Paths alone are not enough: after the dialog appears, the tree can move to
/// another filesystem or an index-backed remote profile can be edited in
/// place. Confirming such a stale dialog must never reinterpret its old path
/// against the new backend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileTreeIntent {
    generation: u64,
    location: crate::remote_fs::FsLocation,
    remote_profile: Option<crate::config::RemoteHost>,
}

pub(crate) fn capture_file_tree_intent(
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> FileTreeIntent {
    let remote_profile = match location {
        crate::remote_fs::FsLocation::Local => None,
        crate::remote_fs::FsLocation::Remote(index) => {
            crate::config::checked_remote_host(hosts, *index)
                .ok()
                .cloned()
        }
    };
    FileTreeIntent {
        generation,
        location: location.clone(),
        remote_profile,
    }
}

/// Revalidate every part of a delayed operation's launch authority. An
/// invalid remote slot deliberately cannot match itself: both its original
/// and current profile would otherwise be `None` and accidentally pass.
pub(crate) fn file_tree_intent_is_current(
    intent: &FileTreeIntent,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> bool {
    if intent.generation != generation || intent.location != *location {
        return false;
    }
    match (&intent.location, &intent.remote_profile) {
        (crate::remote_fs::FsLocation::Local, None) => true,
        (crate::remote_fs::FsLocation::Remote(index), Some(expected)) => {
            crate::config::checked_remote_host(hosts, *index)
                .is_ok_and(|current| current == expected)
        }
        _ => false,
    }
}

/// Revalidate a background callback's tree authority and, for transfers, its
/// monotonic UI identity. Filesystem/clipboard settlement is intentionally
/// independent of this predicate; only progress, toasts, and refreshes are
/// allowed to publish when it returns true.
pub(crate) fn file_tree_async_ui_is_current(
    intent: &FileTreeIntent,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
    expected_transfer: Option<u64>,
    current_transfer: u64,
) -> bool {
    file_tree_intent_is_current(intent, generation, location, hosts)
        && expected_transfer.is_none_or(|expected| expected == current_transfer)
}

/// Following a managed pane must rebuild on a backend change even when both
/// hosts report the same textual cwd. Paths are meaningful only together with
/// their filesystem location; retaining the old rows would relabel B as A.
pub(crate) fn file_tree_follow_requires_reroot(
    current_location: &crate::remote_fs::FsLocation,
    target_location: &crate::remote_fs::FsLocation,
    current_root: &Path,
    target_root: &Path,
) -> bool {
    current_location != target_location || current_root != target_root
}

/// Scan `dir` on a worker thread under the shared permit, then hand the
/// result to `apply` on the GTK thread via the glib poll. `loc` + `hosts`
/// snapshot the backend at request time; `remote_fs::list_dir` does the work.
pub(crate) fn request_dir_scan<F>(
    loc: crate::remote_fs::FsLocation,
    hosts: Vec<crate::config::RemoteHost>,
    dir: PathBuf,
    apply: F,
) -> io::Result<()>
where
    F: FnOnce(io::Result<Vec<FileEntry>>) + 'static,
{
    request_fs_op(
        move || crate::remote_fs::list_dir(&loc, &hosts, &dir),
        apply,
    )
}

/// One event from a streaming filesystem op: throttled byte progress, then
/// exactly one terminal result.
pub(crate) enum FsOpOutcome<T> {
    Progress(u64),
    Done(io::Result<T>),
}

/// Run one blocking op on a worker thread under the shared permit, streaming
/// throttled progress events and the terminal result to `apply` on the GTK
/// thread via the glib poll. The worker's progress callback is non-blocking:
/// a stalled UI must never back-pressure a transfer.
pub(crate) fn request_fs_op_streaming<T, O, F>(op: O, apply: F) -> io::Result<()>
where
    O: FnOnce(&dyn Fn(u64)) -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnMut(FsOpOutcome<T>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel::<FsOpOutcome<T>>(64);
    std::thread::Builder::new()
        .name("anvil-file-tree-op".to_string())
        .spawn(move || {
            let _permit = permit;
            let progress = |bytes: u64| {
                let _ = tx.try_send(FsOpOutcome::Progress(bytes));
            };
            let result = op(&progress);
            let _ = tx.send(FsOpOutcome::Done(result));
        })?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || {
        let mut flow = glib::ControlFlow::Continue;
        loop {
            match rx.try_recv() {
                Ok(FsOpOutcome::Progress(bytes)) => {
                    if let Some(apply) = apply.as_mut() {
                        apply(FsOpOutcome::Progress(bytes));
                    }
                }
                Ok(FsOpOutcome::Done(result)) => {
                    if let Some(mut apply) = apply.take() {
                        apply(FsOpOutcome::Done(result));
                    }
                    flow = glib::ControlFlow::Break;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(mut apply) = apply.take() {
                        apply(FsOpOutcome::Done(Err(io::Error::other(
                            "file-tree op worker disconnected",
                        ))));
                    }
                    flow = glib::ControlFlow::Break;
                    break;
                }
            }
        }
        flow
    });
    Ok(())
}

/// Run one blocking filesystem op on a worker thread with the same permit /
/// glib-poll skeleton as directory scans, so mutations and listings share the
/// concurrency budget.
pub(crate) fn request_fs_op<T, O, F>(op: O, apply: F) -> io::Result<()>
where
    O: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnOnce(io::Result<T>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("anvil-file-tree-scan".to_string())
        .spawn(move || {
            let _permit = permit;
            let _ = tx.send(op());
        })?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(result) => {
            if let Some(apply) = apply.take() {
                apply(result);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            if let Some(apply) = apply.take() {
                apply(Err(io::Error::other("file-tree scan worker disconnected")));
            }
            glib::ControlFlow::Break
        }
    });
    Ok(())
}

/// Display name, reversible path identity, is-directory, icon name, safe tooltip.
pub(crate) fn new_store() -> TreeStore {
    TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::BOOL,
        glib::Type::STRING,
        glib::Type::STRING,
    ])
}

/// Build the headerless `TreeView` (icon + name in one column) over a
/// `TreeModelFilter` driven by `filter`, with multi-selection enabled. No
/// signals wired. Returns the filter model and the view; every path/iter the
/// view hands out (signals, path_at_pos, selection) is in FILTER-model
/// coordinates and must be converted before indexing `store`.
pub(crate) fn new_view(
    store: &TreeStore,
    filter: &Rc<RefCell<TreeFilter>>,
) -> (TreeModelFilter, TreeView) {
    let filter_model = TreeModelFilter::new(store, None::<&gtk::TreePath>);
    {
        let filter = filter.clone();
        filter_model.set_visible_func(move |model, iter| {
            let state = filter.borrow();
            if !state.is_active() {
                return true;
            }
            let identity: String = model
                .get_value(iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            // Placeholders (empty identity) never count as matches.
            !identity.is_empty() && state.is_visible(&identity)
        });
    }
    let view = TreeView::with_model(&filter_model);
    view.set_headers_visible(false);
    view.set_vexpand(true);
    view.set_tooltip_column(COL_TOOLTIP as i32);
    view.selection().set_mode(gtk::SelectionMode::Multiple);

    let column = TreeViewColumn::new();
    let icon = CellRendererPixbuf::new();
    column.pack_start(&icon, false);
    column.add_attribute(&icon, "icon-name", COL_ICON as i32);
    let text = CellRendererText::new();
    column.pack_start(&text, true);
    column.add_attribute(&text, "text", COL_NAME as i32);
    view.append_column(&column);
    (filter_model, view)
}

/// Insert one entry row under `parent` at `position` (None = append), with
/// the lazy-expansion placeholder child for directories.
fn insert_entry_row(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    position: Option<u32>,
    entry: &FileEntry,
) {
    let FileEntry { name, path, is_dir } = entry;
    let icon = if *is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    };
    let Some(path_identity) = encode_path_identity(path) else {
        log::warn!(
            "file-tree path exceeds the {}-byte identity limit: {}",
            MAX_FILE_PATH_IDENTITY_BYTES,
            display_full_path(path)
        );
        return;
    };
    let display_name = crate::review_input::safe_inline_display(name, MAX_FILE_NAME_DISPLAY_BYTES);
    let tooltip = display_full_path(path);
    let iter = store.insert_with_values(
        parent,
        position,
        &[
            (COL_NAME, &display_name),
            (COL_PATH, &path_identity),
            (COL_IS_DIR, is_dir),
            (COL_ICON, &icon),
            (COL_TOOLTIP, &tooltip),
        ],
    );
    if *is_dir {
        // Placeholder child (empty path) → expander shows, loaded lazily.
        store.insert_with_values(
            Some(&iter),
            None,
            &[
                (COL_NAME, &""),
                (COL_PATH, &""),
                (COL_IS_DIR, &false),
                (COL_ICON, &""),
                (COL_TOOLTIP, &""),
            ],
        );
    }
}

/// Insert one row per pre-scanned directory entry under `parent`.
pub(crate) fn append_entries(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    entries: Vec<FileEntry>,
) {
    for entry in &entries {
        insert_entry_row(store, parent, None, entry);
    }
}

/// Find the first row whose COL_PATH identity matches, walking the whole
/// model. Used to target an in-place refresh at one materialized directory.
pub(crate) fn find_row_by_identity(store: &TreeStore, identity: &str) -> Option<TreeIter> {
    fn walk(store: &TreeStore, parent: Option<&TreeIter>, identity: &str) -> Option<TreeIter> {
        let mut index = 0;
        while let Some(iter) = store.iter_nth_child(parent, index) {
            let value: String = store
                .get_value(&iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if value == identity {
                return Some(iter);
            }
            if let Some(found) = walk(store, Some(&iter), identity) {
                return Some(found);
            }
            index += 1;
        }
        None
    }
    walk(store, None, identity)
}

/// Attach identities to a fresh scan, dropping paths too long to encode.
fn identified(entries: Vec<FileEntry>) -> Vec<(String, FileEntry)> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let identity = encode_path_identity(&entry.path)?;
            Some((identity, entry))
        })
        .collect()
}

/// The edits that reconcile one directory's rows with a fresh scan.
struct MergeEdit<'a> {
    /// Indexes of current children to remove, ascending.
    removals: Vec<usize>,
    /// (position, entry) inserts in ascending order; positions apply to the
    /// post-removal model as the inserts land one by one.
    inserts: Vec<(u32, &'a FileEntry)>,
}

/// Pure merge computation behind [`merge_refresh_children`]: rows whose path
/// vanished are removed, new entries are inserted in sort order, survivors
/// keep their place (and with it their children and expansion). Returns None
/// when a placeholder child marks a never-expanded directory — its lazy scan
/// sees the fresh state on expansion, so the row stays untouched.
fn plan_merge_refresh<'a>(
    current: &[String],
    fresh: &'a [(String, FileEntry)],
) -> Option<MergeEdit<'a>> {
    if current.iter().any(String::is_empty) {
        return None;
    }
    let fresh_ids: std::collections::HashSet<&str> =
        fresh.iter().map(|(id, _)| id.as_str()).collect();
    let fresh_by_id: std::collections::HashMap<&str, &FileEntry> = fresh
        .iter()
        .map(|(id, entry)| (id.as_str(), entry))
        .collect();

    let mut removals = Vec::new();
    let mut survivors: Vec<&str> = Vec::new();
    for (index, identity) in current.iter().enumerate() {
        if fresh_ids.contains(identity.as_str()) {
            survivors.push(identity.as_str());
        } else {
            removals.push(index);
        }
    }

    let mut inserts = Vec::new();
    let mut insert_at = 0u32;
    let mut survivor_index = 0;
    for (identity, entry) in fresh {
        if survivors.contains(&identity.as_str()) {
            continue;
        }
        while survivor_index < survivors.len() {
            let survivor = fresh_by_id[survivors[survivor_index]];
            if entry_cmp(entry, survivor) == std::cmp::Ordering::Less {
                break;
            }
            survivor_index += 1;
            insert_at += 1;
        }
        inserts.push((insert_at, entry));
        insert_at += 1;
    }
    Some(MergeEdit { removals, inserts })
}

/// Reconcile one directory's rows with a fresh scan, preserving surviving
/// rows (and their expansion). `parent: None` merges at the top level.
pub(crate) fn merge_refresh_children(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    fresh: Vec<FileEntry>,
) {
    let fresh = identified(fresh);
    let mut current = Vec::new();
    let mut index = 0;
    while let Some(iter) = store.iter_nth_child(parent, index) {
        current.push(
            store
                .get_value(&iter, COL_PATH as i32)
                .get::<String>()
                .unwrap_or_default(),
        );
        index += 1;
    }
    let Some(edit) = plan_merge_refresh(&current, &fresh) else {
        return;
    };
    // Descending removal keeps the still-valid lower indexes intact.
    for index in edit.removals.iter().rev() {
        if let Some(iter) = store.iter_nth_child(parent, *index as i32) {
            store.remove(&iter);
        }
    }
    for (position, entry) in edit.inserts {
        insert_entry_row(store, parent, Some(position), entry);
    }
}

/// Lazily fill a directory row's real children on first expansion. `location`
/// decides the backend (local disk or one remote host); a location switch
/// mid-scan drops the stale result before it touches the store.
pub(crate) fn on_expand(
    store: &TreeStore,
    iter: &TreeIter,
    scan_generation: &Rc<Cell<u64>>,
    location: &Rc<RefCell<crate::remote_fs::FsLocation>>,
    hosts: Vec<crate::config::RemoteHost>,
) {
    // A not-yet-loaded directory has a single placeholder child (empty path).
    let Some(first_child) = store.iter_children(Some(iter)) else {
        return;
    };
    let child_path: String = store
        .get_value(&first_child, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if !child_path.is_empty() {
        return; // already populated
    }
    let scan_in_progress: bool = store
        .get_value(&first_child, COL_IS_DIR as i32)
        .get()
        .unwrap_or(false);
    if scan_in_progress {
        return;
    }
    let dir_identity: String = store
        .get_value(iter, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if dir_identity.is_empty() {
        return;
    }
    let Some(dir_path) = decode_path_identity(&dir_identity) else {
        log::warn!("file-tree row contains an invalid path identity");
        return;
    };
    let Some(row_ref) = TreeRowReference::new(store, &store.path(iter)) else {
        return;
    };

    store.set(&first_child, &[(COL_IS_DIR, &true)]);
    let store_for_result = store.clone();
    let active_generation = scan_generation.clone();
    let generation = active_generation.get();
    let expected_identity = dir_identity.clone();
    let expected_display = display_full_path(&dir_path);
    let loc = location.borrow().clone();
    let active_location = location.clone();
    let expected_loc = loc.clone();
    if let Err(error) = request_dir_scan(loc, hosts, dir_path, move |result| {
        if active_generation.get() != generation || *active_location.borrow() != expected_loc {
            return;
        }
        let Some(row_path) = row_ref.path() else {
            return;
        };
        let Some(parent) = store_for_result.iter(&row_path) else {
            return;
        };
        let current_path: String = store_for_result
            .get_value(&parent, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if current_path != expected_identity {
            return;
        }
        let Some(placeholder) = store_for_result.iter_children(Some(&parent)) else {
            return;
        };
        let placeholder_path: String = store_for_result
            .get_value(&placeholder, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if !placeholder_path.is_empty() {
            return;
        }
        store_for_result.remove(&placeholder);
        match result {
            Ok(entries) => append_entries(&store_for_result, Some(&parent), entries),
            Err(error) => log::warn!("failed to scan directory {expected_display}: {error}"),
        }
    }) {
        store.set(&first_child, &[(COL_IS_DIR, &false)]);
        log::warn!("failed to start directory scan: {error}");
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Abbreviate the home directory to `~` for the header label.
pub(crate) fn display_path(path: &Path) -> String {
    let display = if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", display_os_str(rel.as_os_str()))
            }
        } else {
            display_os_str(path.as_os_str())
        }
    } else {
        display_os_str(path.as_os_str())
    };
    crate::review_input::safe_inline_display(&display, MAX_FILE_PATH_DISPLAY_BYTES)
}

pub(crate) fn display_full_path(path: &Path) -> String {
    crate::review_input::safe_inline_display(
        &display_os_str(path.as_os_str()),
        MAX_FILE_PATH_DISPLAY_BYTES,
    )
}

/// The Copy Path payload: the row's full path text, display-escaped so a
/// non-UTF-8 name stays unambiguous. Remote rows intentionally get the plain
/// path with no prefix — that is what users paste into the remote shell.
pub(crate) fn copy_path_payload(path: &Path) -> String {
    display_full_path(path)
}

/// Right-click target resolution: a click inside the current selection aims
/// the menu at the whole selection; a click outside collapses the selection
/// to the clicked row first (the bool tells the caller to reselect).
pub(crate) fn menu_targets(selected: &[PathBuf], clicked: &Path) -> (Vec<PathBuf>, bool) {
    if selected.iter().any(|path| path == clicked) {
        (selected.to_vec(), false)
    } else {
        (vec![clicked.to_path_buf()], true)
    }
}

// ---------------------------------------------------------------------------
// Client-side filter of the loaded tree
// ---------------------------------------------------------------------------

/// Live filter state for the sidebar tree. While active, `visible` holds the
/// identities shown (matches plus ancestors); clearing restores the expansion
/// snapshot taken when filtering began.
pub(crate) struct TreeFilter {
    query: String,
    visible: std::collections::HashSet<String>,
    saved_expansion: Option<std::collections::HashSet<String>>,
}

impl TreeFilter {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            visible: std::collections::HashSet::new(),
            saved_expansion: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    fn is_visible(&self, identity: &str) -> bool {
        !self.is_active() || self.visible.contains(identity)
    }
}

/// One loaded row for filter planning: path identity, display name, parent
/// (index into the same list, depth-first order).
pub(crate) struct FilterRow {
    pub(crate) identity: String,
    pub(crate) name: String,
    pub(crate) parent: Option<usize>,
}

/// Rows whose name contains `query` (case-insensitive) plus every ancestor
/// of a match. An empty query keeps everything.
pub(crate) fn filter_visible(rows: &[FilterRow], query: &str) -> Vec<bool> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return vec![true; rows.len()];
    }
    let mut visible = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if row.name.to_lowercase().contains(&query) {
            visible[index] = true;
            let mut parent = row.parent;
            while let Some(index) = parent {
                if visible[index] {
                    break;
                }
                visible[index] = true;
                parent = rows[index].parent;
            }
        }
    }
    visible
}

/// All loaded rows in depth-first order with their store paths.
fn collect_filter_rows(store: &TreeStore) -> (Vec<FilterRow>, Vec<gtk::TreePath>) {
    fn walk(
        store: &TreeStore,
        parent: Option<&TreeIter>,
        parent_index: Option<usize>,
        rows: &mut Vec<FilterRow>,
        paths: &mut Vec<gtk::TreePath>,
    ) {
        let mut index = 0;
        while let Some(iter) = store.iter_nth_child(parent, index) {
            let identity: String = store
                .get_value(&iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if identity.is_empty() {
                index += 1;
                continue; // placeholders are not filterable rows
            }
            let name: String = store
                .get_value(&iter, COL_NAME as i32)
                .get()
                .unwrap_or_default();
            let row_index = rows.len();
            rows.push(FilterRow {
                identity,
                name,
                parent: parent_index,
            });
            paths.push(store.path(&iter));
            walk(store, Some(&iter), Some(row_index), rows, paths);
            index += 1;
        }
    }
    let mut rows = Vec::new();
    let mut paths = Vec::new();
    walk(store, None, None, &mut rows, &mut paths);
    (rows, paths)
}

/// Apply or update the filter: recompute visibility over the loaded rows,
/// refilter, and auto-expand ancestors of matches. On clear, restore the
/// expansion snapshot from when filtering began. Pure lookup — never scans.
pub(crate) fn apply_tree_filter(
    store: &TreeStore,
    view: &TreeView,
    filter_model: &TreeModelFilter,
    state: &mut TreeFilter,
    query: &str,
) {
    let was_active = state.is_active();
    if !was_active && !query.is_empty() {
        state.saved_expansion = Some(collect_expanded_identities(store, view, filter_model));
    }
    state.query.clear();
    state.query.push_str(query);
    if state.is_active() {
        let (rows, _) = collect_filter_rows(store);
        let visible = filter_visible(&rows, query);
        state.visible = rows
            .iter()
            .zip(visible.iter())
            .filter(|(_, visible)| **visible)
            .map(|(row, _)| row.identity.clone())
            .collect();
    } else {
        state.visible.clear();
    }
    filter_model.refilter();
    if state.is_active() {
        // Expand every ancestor of a visible row. Those rows are all fully
        // loaded (a loaded descendant implies a loaded chain), so this never
        // triggers a scan.
        let (rows, paths) = collect_filter_rows(store);
        let mut expand = vec![false; rows.len()];
        for index in 0..rows.len() {
            if !state.visible.contains(&rows[index].identity) {
                continue;
            }
            let mut parent = rows[index].parent;
            while let Some(p) = parent {
                expand[p] = true;
                parent = rows[p].parent;
            }
        }
        for (index, _) in rows.iter().enumerate() {
            if !expand[index] {
                continue;
            }
            if let Some(filter_path) = filter_model.convert_child_path_to_path(&paths[index]) {
                view.expand_row(&filter_path, false);
            }
        }
    } else if was_active {
        view.collapse_all();
        if let Some(saved) = state.saved_expansion.take() {
            let (rows, paths) = collect_filter_rows(store);
            for (index, row) in rows.iter().enumerate() {
                if !saved.contains(&row.identity) {
                    continue;
                }
                if let Some(filter_path) = filter_model.convert_child_path_to_path(&paths[index]) {
                    view.expand_row(&filter_path, false);
                }
            }
        }
    }
}

/// Identities of every currently expanded row, for the clear-time restore.
fn collect_expanded_identities(
    store: &TreeStore,
    view: &TreeView,
    filter_model: &TreeModelFilter,
) -> std::collections::HashSet<String> {
    let (rows, paths) = collect_filter_rows(store);
    let mut expanded = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let expanded_now = filter_model
            .convert_child_path_to_path(&paths[index])
            .is_some_and(|filter_path| view.row_expanded(&filter_path));
        if expanded_now {
            expanded.insert(row.identity.clone());
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn remote_host() -> crate::config::RemoteHost {
        crate::config::RemoteHost {
            name: "staging".to_string(),
            host: "server.example.com".to_string(),
            user: Some("deploy".to_string()),
            docker: false,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            ssh_args: vec!["-p".to_string(), "2222".to_string()],
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        }
    }

    #[test]
    fn entries_sort_directories_first_then_by_name() {
        let mut entries = vec![
            FileEntry {
                name: "Zulu.txt".into(),
                path: PathBuf::from("Zulu.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "beta".into(),
                path: PathBuf::from("beta"),
                is_dir: true,
            },
            FileEntry {
                name: "Alpha.txt".into(),
                path: PathBuf::from("Alpha.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "Able".into(),
                path: PathBuf::from("Able"),
                is_dir: true,
            },
        ];

        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Able", "beta", "Alpha.txt", "Zulu.txt"]);
    }

    #[test]
    fn terminal_target_keeps_local_cwd_but_remote_launches_only_the_profile() {
        assert_eq!(
            terminal_target(
                &crate::remote_fs::FsLocation::Local,
                Path::new("/work/tree"),
                &[]
            ),
            Ok(FileTreeTerminalTarget::Local("/work/tree".to_string()))
        );

        let host = remote_host();
        assert_eq!(
            terminal_target(
                &crate::remote_fs::FsLocation::Remote(0),
                Path::new("/remote/browsed/path"),
                std::slice::from_ref(&host)
            ),
            Ok(FileTreeTerminalTarget::Remote(host))
        );
    }

    #[test]
    fn terminal_target_rejects_unusable_local_roots_and_stale_remote_slots() {
        assert!(terminal_target(
            &crate::remote_fs::FsLocation::Local,
            Path::new("relative"),
            &[]
        )
        .is_err());
        let non_utf8 = PathBuf::from(OsString::from_vec(b"/work/\xff".to_vec()));
        assert!(terminal_target(&crate::remote_fs::FsLocation::Local, &non_utf8, &[]).is_err());
        assert!(terminal_target(
            &crate::remote_fs::FsLocation::Remote(1),
            Path::new("/ignored"),
            &[remote_host()]
        )
        .is_err());
    }

    #[test]
    fn delayed_file_tree_intent_requires_the_same_generation_and_location() {
        let intent = capture_file_tree_intent(41, &crate::remote_fs::FsLocation::Local, &[]);
        assert!(file_tree_intent_is_current(
            &intent,
            41,
            &crate::remote_fs::FsLocation::Local,
            &[]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            42,
            &crate::remote_fs::FsLocation::Local,
            &[]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            41,
            &crate::remote_fs::FsLocation::Remote(0),
            &[remote_host()]
        ));
    }

    #[test]
    fn delayed_remote_intent_requires_the_complete_original_profile() {
        let host = remote_host();
        let intent = capture_file_tree_intent(
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host),
        );
        assert!(file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host)
        ));

        let mut edited = host.clone();
        edited.host = "replacement.example.com".to_string();
        assert!(!file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            &[edited]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Local,
            &[host]
        ));
    }

    #[test]
    fn remote_home_probe_cannot_cross_generation_or_reused_numeric_slot() {
        let host_a = remote_host();
        let intent = capture_file_tree_intent(
            17,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );

        // A -> Local -> B -> slot 0 can end at the same numeric FsLocation,
        // but both the intervening tree generation and profile identity are
        // part of the frozen probe authority.
        let mut host_b = host_a.clone();
        host_b.host = "replacement.example.com".to_string();
        assert!(!file_tree_intent_is_current(
            &intent,
            19,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_b),
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            17,
            &crate::remote_fs::FsLocation::Remote(0),
            &[host_b],
        ));
    }

    #[test]
    fn delayed_header_terminal_and_drop_require_the_open_time_tree_authority() {
        let local = capture_file_tree_intent(23, &crate::remote_fs::FsLocation::Local, &[]);
        assert!(!file_tree_intent_is_current(
            &local,
            24,
            &crate::remote_fs::FsLocation::Local,
            &[],
        ));

        let host_a = remote_host();
        let mut host_b = host_a.clone();
        host_b.name = "production".to_string();
        host_b.host = "production.example.com".to_string();
        let remote = capture_file_tree_intent(
            30,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );
        assert!(!file_tree_intent_is_current(
            &remote,
            30,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_b),
        ));
        assert!(!file_tree_intent_is_current(
            &remote,
            31,
            &crate::remote_fs::FsLocation::Remote(1),
            &[host_b, host_a],
        ));
    }

    #[test]
    fn invalid_remote_slot_never_authorizes_a_delayed_operation() {
        let intent = capture_file_tree_intent(
            9,
            &crate::remote_fs::FsLocation::Remote(1),
            &[remote_host()],
        );
        assert!(!file_tree_intent_is_current(
            &intent,
            9,
            &crate::remote_fs::FsLocation::Remote(1),
            &[remote_host()]
        ));
    }

    #[test]
    fn async_ui_publication_requires_tree_authority_and_latest_transfer_identity() {
        let host_a = remote_host();
        let intent = capture_file_tree_intent(
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );

        assert!(file_tree_async_ui_is_current(
            &intent,
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            Some(8),
            8,
        ));

        // A -> B suppresses late progress, success, and error publication.
        let mut host_b = host_a.clone();
        host_b.host = "replacement.example.com".to_string();
        for (event, transfer) in [
            ("operation success/error", None),
            ("transfer progress/success/error", Some(8)),
        ] {
            assert!(
                !file_tree_async_ui_is_current(
                    &intent,
                    12,
                    &crate::remote_fs::FsLocation::Remote(0),
                    std::slice::from_ref(&host_b),
                    transfer,
                    8,
                ),
                "stale {event} must not publish after A -> B"
            );
        }

        // Starting a newer transfer suppresses an older callback even when
        // both transfers target the same tree (including an ABA payload).
        assert!(!file_tree_async_ui_is_current(
            &intent,
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            Some(8),
            9,
        ));

        // Ordinary operations have no transfer identity but still fail closed
        // across a root generation change.
        assert!(!file_tree_async_ui_is_current(
            &intent,
            13,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            None,
            9,
        ));
    }

    #[test]
    fn remote_follow_reroots_when_only_the_backend_changes() {
        let same_path = Path::new("/home/deploy");
        assert!(file_tree_follow_requires_reroot(
            &crate::remote_fs::FsLocation::Remote(0),
            &crate::remote_fs::FsLocation::Remote(1),
            same_path,
            same_path,
        ));
        assert!(!file_tree_follow_requires_reroot(
            &crate::remote_fs::FsLocation::Remote(1),
            &crate::remote_fs::FsLocation::Remote(1),
            same_path,
            same_path,
        ));
    }

    #[test]
    fn non_utf8_path_identities_round_trip_without_colliding() {
        let ff = PathBuf::from(OsString::from_vec(b"a\xff".to_vec()));
        let fe = PathBuf::from(OsString::from_vec(b"a\xfe".to_vec()));

        let ff_identity = encode_path_identity(&ff).expect("bounded path should encode");
        let fe_identity = encode_path_identity(&fe).expect("bounded path should encode");

        assert_ne!(ff_identity, fe_identity);
        assert_eq!(decode_path_identity(&ff_identity), Some(ff.clone()));
        assert_eq!(decode_path_identity(&fe_identity), Some(fe.clone()));
        assert_eq!(display_os_str(ff.as_os_str()), r"a\xff");
        assert_eq!(display_os_str(fe.as_os_str()), r"a\xfe");
    }

    #[test]
    fn path_identity_rejects_malformed_or_oversized_values() {
        assert_eq!(decode_path_identity(""), None);
        assert_eq!(decode_path_identity("unix-path-v1:0"), None);
        assert_eq!(decode_path_identity("unix-path-v1:gg"), None);
        let oversized = PathBuf::from(OsString::from_vec(vec![
            b'a';
            MAX_FILE_PATH_IDENTITY_BYTES + 1
        ]));
        assert_eq!(encode_path_identity(&oversized), None);
    }

    #[test]
    fn copy_path_payload_is_the_plain_display_path() {
        assert_eq!(
            copy_path_payload(Path::new("/etc/hostname")),
            "/etc/hostname"
        );
        // Non-UTF-8 names keep the unambiguous escaped display form.
        let weird = PathBuf::from(OsString::from_vec(b"/tmp/a\xffb".to_vec()));
        assert_eq!(copy_path_payload(&weird), r"/tmp/a\xffb");
    }

    #[test]
    fn menu_targets_selection_inside_vs_outside() {
        let selected = vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")];

        // A click inside the selection targets the whole selection and keeps it.
        let (targets, collapse) = menu_targets(&selected, Path::new("/a/two"));
        assert_eq!(targets, selected);
        assert!(!collapse);

        // A click outside targets that row alone and collapses the selection.
        let (targets, collapse) = menu_targets(&selected, Path::new("/a/other"));
        assert_eq!(targets, [PathBuf::from("/a/other")]);
        assert!(collapse);

        // No selection at all behaves like a plain single-row click.
        let (targets, collapse) = menu_targets(&[], Path::new("/a/one"));
        assert_eq!(targets, [PathBuf::from("/a/one")]);
        assert!(collapse);
    }

    // -- tree filter (loaded rows only) ---------------------------------------

    /// rows: (identity, name, parent index)
    fn filter_rows(spec: &[(&str, &str, Option<usize>)]) -> Vec<FilterRow> {
        spec.iter()
            .map(|(identity, name, parent)| FilterRow {
                identity: (*identity).to_string(),
                name: (*name).to_string(),
                parent: *parent,
            })
            .collect()
    }

    fn visible_names<'a>(rows: &'a [FilterRow], query: &str) -> Vec<&'a str> {
        rows.iter()
            .zip(filter_visible(rows, query))
            .filter(|(_, visible)| *visible)
            .map(|(row, _)| row.name.as_str())
            .collect()
    }

    #[test]
    fn filter_visible_matches_and_keeps_ancestors() {
        let rows = filter_rows(&[
            ("/r", "r", None),                         // 0
            ("/r/docs", "docs", Some(0)),              // 1
            ("/r/docs/notes.md", "notes.md", Some(1)), // 2
            ("/r/src", "src", Some(0)),                // 3
            ("/r/src/main.rs", "main.rs", Some(3)),    // 4
            ("/r/README.md", "README.md", Some(0)),    // 5
        ]);

        // Nested match keeps the whole ancestor chain, hides siblings.
        assert_eq!(visible_names(&rows, "notes"), ["r", "docs", "notes.md"]);
        // Case-insensitive.
        assert_eq!(visible_names(&rows, "README"), ["r", "README.md"]);
        // Multiple matches keep each ancestor chain once.
        assert_eq!(
            visible_names(&rows, "md"),
            ["r", "docs", "notes.md", "README.md"]
        );
        // No match → nothing visible.
        assert!(visible_names(&rows, "zzz").is_empty());
        // Empty query is the identity.
        assert_eq!(filter_visible(&rows, ""), vec![true; rows.len()]);
        assert_eq!(filter_visible(&rows, "  "), vec![false; 6]); // "  " matches nothing
    }

    // -- merge refresh (in-place directory update) ----------------------------

    fn entry(path: &str, is_dir: bool) -> FileEntry {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        FileEntry::new(name, PathBuf::from(path), is_dir)
    }

    fn identity_of(path: &str) -> String {
        encode_path_identity(Path::new(path)).expect("short paths encode")
    }

    /// Simulate the model after applying an edit: stale rows removed, inserts
    /// at their planned positions, survivors untouched.
    fn apply_plan(current: &[String], edit: &MergeEdit) -> Vec<String> {
        let mut model: Vec<String> = current.to_vec();
        for index in edit.removals.iter().rev() {
            model.remove(*index);
        }
        for (position, entry) in &edit.inserts {
            model.insert(
                *position as usize,
                encode_path_identity(&entry.path).expect("short paths encode"),
            );
        }
        model
            .iter()
            .map(|identity| {
                decode_path_identity(identity)
                    .expect("rows carry valid identities")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn merge_plan_removes_stale_inserts_sorted_and_keeps_survivors() {
        let current: Vec<String> = ["/r/aaa", "/r/bbb", "/r/file1", "/r/file2"]
            .into_iter()
            .map(identity_of)
            .collect();
        let mut fresh_entries = vec![
            entry("/r/aaa", true),    // survives
            entry("/r/ccc", true),    // new dir
            entry("/r/file0", false), // new file
            entry("/r/file1", false), // survives
        ];
        sort_entries(&mut fresh_entries);
        let fresh = identified(fresh_entries);

        let edit = plan_merge_refresh(&current, &fresh).expect("no placeholder");
        assert_eq!(edit.removals, [1, 3], "bbb and file2 are removed");
        let insert_paths: Vec<(u32, String)> = edit
            .inserts
            .iter()
            .map(|(position, entry)| (*position, entry.path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            insert_paths,
            [(1, "/r/ccc".to_string()), (2, "/r/file0".to_string())],
            "ccc and file0 land at their sorted positions"
        );
        assert_eq!(
            apply_plan(&current, &edit),
            ["/r/aaa", "/r/ccc", "/r/file0", "/r/file1"]
        );
    }

    #[test]
    fn merge_plan_skips_placeholder_rows() {
        // A never-expanded directory has one placeholder child (empty path).
        let current = vec![String::new()];
        let fresh = identified(vec![entry("/r/aaa/new", false)]);
        assert!(plan_merge_refresh(&current, &fresh).is_none());
    }

    #[test]
    fn merge_plan_handles_rename_shape_and_empty_results() {
        let current: Vec<String> = ["/r/alpha.txt", "/r/zeta.txt"]
            .into_iter()
            .map(identity_of)
            .collect();
        let mut fresh_entries = vec![entry("/r/alpha.txt", false), entry("/r/mid.txt", false)];
        sort_entries(&mut fresh_entries);
        let fresh = identified(fresh_entries);
        let edit = plan_merge_refresh(&current, &fresh).expect("no placeholder");
        assert_eq!(edit.removals, [1]);
        assert_eq!(apply_plan(&current, &edit), ["/r/alpha.txt", "/r/mid.txt"]);

        // Everything vanished: the children are all removed.
        let edit = plan_merge_refresh(&current, &[]).expect("no placeholder");
        assert_eq!(edit.removals, [0, 1]);
        assert!(edit.inserts.is_empty());
        assert!(apply_plan(&current, &edit).is_empty());
    }
}
