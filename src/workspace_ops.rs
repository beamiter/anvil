//! Workspace, tab, pane, session-restore, and remote-connection operations.
//!
//! This remains an inherent `AppModel` implementation inside the same Relm4
//! component. The extraction only separates responsibilities from `main.rs`; it
//! does not introduce another model, message loop, or UI framework.

use super::*;

const PERSISTENCE_FAILURE_NOTICE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Seconds an undo offer stays on screen. The default toast timeout is tuned
/// for statements; this one is a decision, and the user has to notice the
/// mistake before they can decide to take it back.
const UNDO_TOAST_TIMEOUT: u32 = 12;

/// Run every fallible preparation step before allowing a structural commit.
/// Split uses this with an injected constructor in tests and with Relm4's
/// synchronously initialized Block component in production.
fn prepare_then_commit<T, E>(
    prepare: impl FnOnce() -> Result<T, E>,
    commit: impl FnOnce(T),
) -> Result<(), E> {
    let prepared = prepare()?;
    commit(prepared);
    Ok(())
}

/// Combine the number of equal pane slots below two children for one axis.
/// A same-axis split consumes both spans; a cross-axis split stacks them and
/// therefore consumes only the larger span on the measured axis.
fn combined_axis_span(same_axis: bool, start: u32, end: u32) -> u32 {
    if same_axis {
        start.saturating_add(end)
    } else {
        start.max(end)
    }
}

fn pane_axis_span(widget: &gtk::Widget, axis: gtk::Orientation) -> u32 {
    let Ok(paned) = widget.clone().downcast::<gtk::Paned>() else {
        return 1;
    };
    let (Some(start), Some(end)) = (paned.start_child(), paned.end_child()) else {
        return 1;
    };
    combined_axis_span(
        paned.orientation() == axis,
        pane_axis_span(&start, axis),
        pane_axis_span(&end, axis),
    )
}

fn balanced_split_position(extent: i32, start_span: u32, end_span: u32) -> Option<i32> {
    if extent <= 1 || start_span == 0 || end_span == 0 {
        return None;
    }
    let total_span = u64::from(start_span) + u64::from(end_span);
    let position = i64::from(extent) * i64::from(start_span) / total_span as i64;
    Some(position.clamp(1, i64::from(extent - 1)) as i32)
}

/// Rebalance nested panes from their subtree spans. Repeated same-axis splits
/// then receive one equal slot per leaf instead of geometrically shrinking the
/// newest half (1/2, 1/4, 1/8, ...).
fn rebalance_pane_tree(widget: &gtk::Widget) {
    let Ok(paned) = widget.clone().downcast::<gtk::Paned>() else {
        return;
    };
    let (Some(start), Some(end)) = (paned.start_child(), paned.end_child()) else {
        return;
    };
    let axis = paned.orientation();
    let extent = if axis == gtk::Orientation::Horizontal {
        paned.width()
    } else {
        paned.height()
    };
    let start_span = pane_axis_span(&start, axis);
    let end_span = pane_axis_span(&end, axis);
    if let Some(position) = balanced_split_position(extent, start_span, end_span) {
        paned.set_position(position);
    }
    rebalance_pane_tree(&start);
    rebalance_pane_tree(&end);
}

pub(super) fn schedule_pane_rebalance(root: gtk::Widget) {
    // The first idle observes the newly inserted Paned. Moving an ancestor
    // divider changes nested allocations, so a second pass settles descendants.
    gtk::glib::idle_add_local_once(move || {
        rebalance_pane_tree(&root);
        let root = root.clone();
        gtk::glib::idle_add_local_once(move || rebalance_pane_tree(&root));
    });
}

/// GTK keeps per-container focus history across child removal. Clear the root
/// while the leaf still belongs to its old tree so a later focus traversal
/// cannot jump back to a widget that was reparented elsewhere.
fn clear_root_focus_before_reparent(widget: &gtk::Widget) {
    if let Some(root) = widget.root() {
        root.set_focus(None::<&gtk::Widget>);
    }
}

/// Detach one terminal leaf, remove its immediate split, and promote the sibling
/// into the validated grandparent slot. Validation happens before mutation so a
/// malformed widget tree cannot leave the model and GTK hierarchy out of sync.
fn detach_leaf_and_promote(holder: &gtk::Box, leaf: &gtk::Widget) -> Option<gtk::Widget> {
    let parent = leaf.parent()?.downcast::<gtk::Paned>().ok()?;
    let start = parent.start_child();
    let end = parent.end_child();
    let sibling = if start.as_ref() == Some(leaf) {
        end?
    } else if end.as_ref() == Some(leaf) {
        start?
    } else {
        return None;
    };

    let parent_widget = parent.clone().upcast::<gtk::Widget>();
    // Validate the collapsing split through the exact holder boundary before
    // touching either child. Checking only its immediate grandparent lets a
    // model leaf that was accidentally attached under another tab mutate that
    // foreign tree and then be removed from the model that did not own it.
    let destination = LeafSlot::of(holder, &parent_widget)?;

    clear_root_focus_before_reparent(leaf);
    parent.set_start_child(None::<&gtk::Widget>);
    parent.set_end_child(None::<&gtk::Widget>);
    match destination {
        LeafSlot::PanedStart(grandparent) => grandparent.set_start_child(Some(&sibling)),
        LeafSlot::PanedEnd(grandparent) => grandparent.set_end_child(Some(&sibling)),
        LeafSlot::Holder(holder) => {
            holder.remove(&parent_widget);
            holder.append(&sibling);
        }
    }
    Some(sibling)
}

/// Validated location of a pane leaf in one tab's widget tree. Resolve this
/// before detaching a source session so malformed GTK ancestry remains a
/// no-op instead of leaving the two ownership representations half-mutated.
enum LeafSlot {
    PanedStart(gtk::Paned),
    PanedEnd(gtk::Paned),
    Holder(gtk::Box),
}

impl LeafSlot {
    fn of(holder: &gtk::Box, leaf: &gtk::Widget) -> Option<Self> {
        let parent = leaf.parent()?;
        let holder_widget = holder.clone().upcast::<gtk::Widget>();
        if parent == holder_widget {
            return (holder.first_child().as_ref() == Some(leaf) && leaf.next_sibling().is_none())
                .then(|| Self::Holder(holder.clone()));
        }
        let paned = parent.downcast::<gtk::Paned>().ok()?;
        let slot = if paned.start_child().as_ref() == Some(leaf) {
            Self::PanedStart(paned.clone())
        } else if paned.end_child().as_ref() == Some(leaf) {
            Self::PanedEnd(paned.clone())
        } else {
            return None;
        };

        // The immediate Paned slot is not sufficient: a stale pane can still
        // be attached to an entirely different tree. Walk every ancestor edge
        // and require the target holder's sole child to be the exact root.
        let mut child = paned.upcast::<gtk::Widget>();
        loop {
            let ancestor = child.parent()?;
            if ancestor == holder_widget {
                return (holder.first_child().as_ref() == Some(&child)
                    && child.next_sibling().is_none())
                .then_some(slot);
            }
            let ancestor = ancestor.downcast::<gtk::Paned>().ok()?;
            if ancestor.start_child().as_ref() != Some(&child)
                && ancestor.end_child().as_ref() != Some(&child)
            {
                return None;
            }
            child = ancestor.upcast();
        }
    }

    fn replace_with_split(
        self,
        target: &gtk::Widget,
        moved: &gtk::Widget,
        edge: pane_header::PaneDropEdge,
    ) {
        let orientation = match edge {
            pane_header::PaneDropEdge::Left | pane_header::PaneDropEdge::Right => {
                gtk::Orientation::Horizontal
            }
            pane_header::PaneDropEdge::Top | pane_header::PaneDropEdge::Bottom => {
                gtk::Orientation::Vertical
            }
        };
        let moved_first = matches!(
            edge,
            pane_header::PaneDropEdge::Left | pane_header::PaneDropEdge::Top
        );
        let paned = gtk::Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(true);
        let target_extent = if orientation == gtk::Orientation::Horizontal {
            target.width()
        } else {
            target.height()
        };
        if let Some(position) = balanced_split_position(target_extent, 1, 1) {
            paned.set_position(position);
        }

        clear_root_focus_before_reparent(target);
        match &self {
            Self::PanedStart(parent) => parent.set_start_child(None::<&gtk::Widget>),
            Self::PanedEnd(parent) => parent.set_end_child(None::<&gtk::Widget>),
            Self::Holder(holder) => holder.remove(target),
        }
        if moved_first {
            paned.set_start_child(Some(moved));
            paned.set_end_child(Some(target));
        } else {
            paned.set_start_child(Some(target));
            paned.set_end_child(Some(moved));
        }
        match self {
            Self::PanedStart(parent) => parent.set_start_child(Some(&paned)),
            Self::PanedEnd(parent) => parent.set_end_child(Some(&paned)),
            Self::Holder(holder) => holder.append(&paned),
        }
    }
}

/// Pane indices in visual order: depth-first through the `Paned` tree, start
/// child before end child.
///
/// The `panes` Vec keeps creation order, which stops describing the layout as
/// soon as two panes are swapped. Headers number themselves from the widget
/// tree so "pane 2" is always the second one the user sees.
fn visual_pane_order(tab: &Tab) -> Vec<usize> {
    fn walk(widget: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        match widget.clone().downcast::<gtk::Paned>() {
            Ok(paned) => {
                if let Some(start) = paned.start_child() {
                    walk(&start, out);
                }
                if let Some(end) = paned.end_child() {
                    walk(&end, out);
                }
            }
            Err(leaf) => out.push(leaf),
        }
    }

    let mut leaves = Vec::new();
    if let Some(root) = tab.holder.first_child() {
        walk(&root, &mut leaves);
    }
    let mut order: Vec<usize> = leaves
        .iter()
        .filter_map(|leaf| tab.panes.iter().position(|pane| pane.widget() == *leaf))
        .collect();
    // A zoomed tab has only the focused leaf in the tree, and a pane detached
    // mid-operation has none. Append whatever the walk missed so every pane
    // still gets a stable number rather than none at all.
    for index in 0..tab.panes.len() {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
}

/// Working directory with `$HOME` collapsed to `~`, for the pane header.
fn abbreviate_home(path: &str) -> String {
    match std::env::var_os("HOME") {
        Some(home) => abbreviate_prefix(path, &home.to_string_lossy()),
        None => path.to_string(),
    }
}

/// The substitution itself, with `home` supplied rather than read from the
/// environment so it is testable without mutating process-wide state.
fn abbreviate_prefix(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    // Only at a component boundary: `/home/user2` merely shares a prefix with
    // `/home/user` and is a different directory.
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Header title for one pane: its OSC title, else its directory's last
/// component, else a positional fallback.
fn pane_header_title(osc_title: Option<&str>, cwd: Option<&str>, position: usize) -> String {
    if let Some(title) = osc_title.map(str::trim) {
        if !title.is_empty() {
            return title.to_string();
        }
    }
    cwd.map(abbreviate_home)
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| {
            // `~` and `/` have no last component worth showing on their own.
            std::path::Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or(cwd)
        })
        .unwrap_or_else(|| format!("Pane {}", position + 1))
}

fn active_index_after_remove(active: usize, removed: usize, remaining: usize) -> usize {
    debug_assert!(remaining > 0);
    if active > removed {
        active - 1
    } else {
        active.min(remaining - 1)
    }
}

/// Resolve a tab reorder without allowing either side of the pinned prefix to
/// cross the boundary. `requested` is the target's index before source
/// removal, matching the tab-row drop contract.
fn pinned_reorder_destination(pinned: &[bool], from: usize, requested: usize) -> Option<usize> {
    if pinned.len() < 2 || from >= pinned.len() {
        return None;
    }
    let requested = requested.min(pinned.len() - 1);
    if from == requested {
        return None;
    }
    let moved_is_pinned = pinned[from];
    let pinned_boundary = pinned
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != from)
        .take_while(|(_, is_pinned)| **is_pinned)
        .count();
    let destination = if moved_is_pinned {
        requested.min(pinned_boundary)
    } else {
        requested.max(pinned_boundary)
    };
    (destination != from).then_some(destination)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DropTabIdentity {
    id: u64,
    pane_ids: Vec<u64>,
    zoomed: bool,
    remote_pane_id: Option<u64>,
    remote_status: Option<ConnStatus>,
}

impl DropTabIdentity {
    fn from_tab(tab: &Tab) -> Self {
        Self {
            id: tab.id,
            pane_ids: tab.panes.iter().map(|pane| pane.id).collect(),
            zoomed: tab.zoom.is_some(),
            remote_pane_id: tab.remote.as_ref().map(|remote| remote.pane_id),
            remote_status: tab.remote.as_ref().map(|remote| remote.status),
        }
    }
}

fn tab_drop_preview_is_valid(
    tabs: &[DropTabIdentity],
    source_tab_id: u64,
    target_tab_id: u64,
) -> bool {
    let Some(source) = tabs.iter().find(|tab| tab.id == source_tab_id) else {
        return false;
    };
    let Some(target) = tabs.iter().find(|tab| tab.id == target_tab_id) else {
        return false;
    };
    source.id != target.id
        && source.pane_ids.len() == 1
        && !source.zoomed
        && !target.zoomed
        && source.remote_status != Some(ConnStatus::Disconnected)
        && target.remote_status != Some(ConnStatus::Disconnected)
        && !(source.remote_pane_id.is_some() && target.remote_pane_id.is_some())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabIntoPanePlan {
    source_tab_id: u64,
    moved_pane_id: u64,
    target_tab_id: u64,
    target_pane_id: u64,
    moves_remote: bool,
}

fn plan_tab_into_pane(
    tabs: &[DropTabIdentity],
    source_tab_id: u64,
    target_pane_id: u64,
) -> Option<TabIntoPanePlan> {
    let source = tabs.iter().find(|tab| tab.id == source_tab_id)?;
    let target = tabs
        .iter()
        .find(|tab| tab.pane_ids.contains(&target_pane_id))?;
    let [moved_pane_id] = source.pane_ids.as_slice() else {
        return None;
    };
    if source.id == target.id
        || source.zoomed
        || target.zoomed
        || source
            .remote_pane_id
            .is_some_and(|pane_id| pane_id != *moved_pane_id)
        || target
            .remote_pane_id
            .is_some_and(|pane_id| !target.pane_ids.contains(&pane_id))
        || source.remote_status.is_some() != source.remote_pane_id.is_some()
        || target.remote_status.is_some() != target.remote_pane_id.is_some()
        || source.remote_status == Some(ConnStatus::Disconnected)
        || target.remote_status == Some(ConnStatus::Disconnected)
        || (source.remote_pane_id.is_some() && target.remote_pane_id.is_some())
    {
        return None;
    }
    Some(TabIntoPanePlan {
        source_tab_id,
        moved_pane_id: *moved_pane_id,
        target_tab_id: target.id,
        target_pane_id,
        moves_remote: source.remote_pane_id.is_some(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneIntoTabPlan {
    source_tab_id: u64,
    pane_id: u64,
    anchor_tab_id: Option<u64>,
    after: bool,
    moves_remote: bool,
}

fn plan_pane_into_tab(
    tabs: &[DropTabIdentity],
    pane_id: u64,
    anchor_tab_id: Option<u64>,
    after: bool,
) -> Option<PaneIntoTabPlan> {
    let source = tabs.iter().find(|tab| tab.pane_ids.contains(&pane_id))?;
    if source.pane_ids.len() <= 1
        || source.zoomed
        || source
            .remote_pane_id
            .is_some_and(|remote| !source.pane_ids.contains(&remote))
    {
        return None;
    }
    if let Some(anchor) = anchor_tab_id {
        if anchor == source.id || !tabs.iter().any(|tab| tab.id == anchor) {
            return None;
        }
    }
    Some(PaneIntoTabPlan {
        source_tab_id: source.id,
        pane_id,
        anchor_tab_id,
        after,
        moves_remote: source.remote_pane_id == Some(pane_id),
    })
}

fn automatic_tab_title(tab: &Tab, index: usize) -> Option<String> {
    if tab.custom_title {
        return None;
    }
    let pane = tab.panes.get(tab.active_pane)?;
    Some(
        pane.title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| default_tab_title(index as u32 + 1, pane.cwd.as_deref())),
    )
}

fn restored_leaf_mode(configured: TerminalMode, remote_integrated: bool) -> TerminalMode {
    if remote_integrated {
        TerminalMode::Block
    } else {
        configured
    }
}

fn managed_remote_host_for_restore(
    hosts: &[config::RemoteHost],
    name: &str,
) -> Option<config::RemoteHost> {
    hosts
        .iter()
        .take(config::MAX_REMOTE_HOSTS)
        .find(|host| host.name == name)
        .cloned()
}

fn snapshot_restorable_command(
    managed_remote: bool,
    detected: Option<Vec<String>>,
) -> Option<Vec<String>> {
    (!managed_remote).then_some(detected).flatten()
}

/// This path is reached only after a managed profile lookup failed. A snapshot
/// from an older build may still contain its expanded SSH argv; do not execute
/// that stale command after the user removed or renamed the authoritative
/// profile.
fn replay_argv_for_unmanaged_leaf<'a>(
    remote_name: Option<&str>,
    commands: Option<&'a [String]>,
) -> Option<&'a [String]> {
    remote_name.is_none().then_some(commands).flatten()
}

fn format_running_process_summary(mut running: Vec<String>) -> Option<String> {
    if running.is_empty() {
        return None;
    }
    const MAX_SHOWN: usize = 8;
    let hidden = running.len().saturating_sub(MAX_SHOWN);
    running.truncate(MAX_SHOWN);
    let mut summary = running.join("\n");
    if hidden > 0 {
        summary.push_str(&format!("\n…and {hidden} more"));
    }
    Some(summary)
}

fn running_process_summary_for_tabs<'a>(tabs: impl IntoIterator<Item = &'a Tab>) -> Option<String> {
    let mut running = Vec::new();
    for tab in tabs {
        for (pane_index, pane) in tab.panes.iter().enumerate() {
            if let Some(process) = pane.foreground_process() {
                let location = if tab.panes.len() > 1 {
                    format!("{} (pane {})", tab.display_title(), pane_index + 1)
                } else {
                    tab.display_title().to_string()
                };
                running.push(format!("{location} — {process}"));
            }
        }
    }
    format_running_process_summary(running)
}

impl AppModel {
    fn current_pane_count(&self) -> usize {
        self.tabs.iter().map(|tab| tab.panes.len()).sum()
    }

    fn ensure_persisted_tab_capacity(&self, adds_pane: bool) -> bool {
        if !self.session_persistence
            || session::can_add_persisted_tab(self.tabs.len(), self.current_pane_count(), adds_pane)
        {
            return true;
        }
        self.show_toast(format!(
            "Session capacity reached ({} tabs, {} panes total). Close a tab or pane before opening another.",
            session::MAX_RESTORED_TABS,
            session::MAX_RESTORED_PANES_TOTAL,
        ));
        false
    }

    pub(crate) fn can_preview_tab_drop(&self, source_tab_id: u64, target_tab_id: u64) -> bool {
        let identities: Vec<_> = self.tabs.iter().map(DropTabIdentity::from_tab).collect();
        tab_drop_preview_is_valid(&identities, source_tab_id, target_tab_id)
    }

    pub(crate) fn add_tab(
        &mut self,
        initial_commands: Option<String>,
        sender: &ComponentSender<AppModel>,
    ) {
        // New tabs inherit the active pane's working directory (matches
        // DuplicateTab), so Ctrl+Shift+T opens where the user already is.
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .and_then(Pane::local_cwd)
            .map(str::to_string);
        self.add_tab_with(
            InitialCommands::from_config(initial_commands.as_deref()),
            cwd,
            self.shell_argv.clone(),
            sender,
        );
    }

    pub(crate) fn add_tab_with(
        &mut self,
        initial_commands: InitialCommands,
        working_directory: Option<String>,
        shell_argv: Rc<Vec<String>>,
        sender: &ComponentSender<AppModel>,
    ) {
        self.add_tab_full(
            initial_commands,
            working_directory,
            shell_argv,
            None,
            sender,
        );
    }

    /// Launch an explicit argv in its own named tab, in conventional VTE mode.
    ///
    /// Used for one-shot helpers such as the jsh installer: they emit no
    /// shell-integration sequences, so Block mode would have nothing to build
    /// blocks from, and their prompts expect a plain terminal to type into.
    pub(crate) fn add_command_tab(
        &mut self,
        title: &str,
        argv: Vec<String>,
        sender: &ComponentSender<AppModel>,
    ) {
        self.add_tab_full(
            InitialCommands::default(),
            None,
            Rc::new(argv),
            Some((TerminalMode::Vte, title.to_string())),
            sender,
        );
    }

    /// Shared body: `command` forces a terminal mode and a fixed tab title,
    /// which is what one-shot helper tabs need and ordinary tabs must not have.
    fn add_tab_full(
        &mut self,
        initial_commands: InitialCommands,
        working_directory: Option<String>,
        shell_argv: Rc<Vec<String>>,
        command: Option<(TerminalMode, String)>,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.ensure_persisted_tab_capacity(true) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let number = self.tabs.len() as u32 + 1;
        let mode = command
            .as_ref()
            .map(|(mode, _)| *mode)
            .unwrap_or_else(|| self.config.borrow().terminal_mode);
        let title_cwd = working_directory.clone();
        let pane = create_pane(
            &self.config,
            &self.organism_hub,
            &shell_argv,
            id,
            pane_id,
            mode,
            initial_commands,
            working_directory,
            None,
            false,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: command
                .as_ref()
                .map(|(_, title)| title.clone())
                .unwrap_or_else(|| default_tab_title(number, title_cwd.as_deref())),
            custom_title: command.is_some(),
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            private_title: false,
            id,
            zoom: None,
            remote: None,
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Insert a newly-created tab immediately after the active tab. Session
    /// restoration intentionally bypasses this so its saved tab order remains
    /// unchanged.
    pub(crate) fn insert_tab_after_active(&mut self, tab: Tab) {
        let insert_at = self.active.saturating_add(1).min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
    }

    /// Recreate a tab from a persisted snapshot, rebuilding the full nested
    /// `Paned` split tree and replaying any restorable command per pane.
    pub(crate) fn restore_tab(
        &mut self,
        saved: &session::SavedTab,
        sender: &ComponentSender<AppModel>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let mut panes = Vec::new();
        let mut restored_remote = None;
        let root_widget =
            self.build_pane_layout(&saved.layout, id, &mut panes, &mut restored_remote, sender);
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&root_widget);
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes,
            active_pane: 0,
            title: saved.title.clone(),
            custom_title: saved.custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: saved.pinned,
            private_title: saved.private_title,
            id,
            zoom: None,
            remote: restored_remote,
        };
        self.tabs.push(tab);
    }

    /// Recursively build the GTK widget tree for a persisted `PaneLayout`,
    /// pushing each created leaf into `panes` in tree order.
    ///
    /// Pane mode used to be persisted with the session.  That made a mode
    /// change in config appear to have no effect: restoring an old VTE pane
    /// recreated it as VTE even when `terminal_mode = "block"`.  The current
    /// configuration is the authority for every newly-created local backend,
    /// including restored panes; remote-integrated restores keep Block mode so
    /// OSC session metadata remains available. The snapshot otherwise restores
    /// only layout and shell state.
    pub(crate) fn build_pane_layout(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        restored_remote: &mut Option<RemoteConn>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        self.build_pane_layout_node(node, tab_id, panes, restored_remote, sender)
    }

    fn build_pane_layout_node(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        restored_remote: &mut Option<RemoteConn>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        match node {
            session::PaneLayout::Leaf {
                cwd,
                cwd_external,
                remote_name,
                sid,
                cmds,
                ..
            } => {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let restored_sid = sid
                    .as_deref()
                    .filter(|value| config::valid_session_id(value))
                    .map(str::to_string);
                if sid.is_some() && restored_sid.is_none() {
                    log::warn!("Ignoring invalid session id in pane snapshot");
                }
                let managed_host = remote_name.as_deref().and_then(|name| {
                    managed_remote_host_for_restore(&self.config.borrow().remote_hosts, name)
                });
                if let Some(mut host) = managed_host {
                    if let Some(restored_sid) = restored_sid.as_ref() {
                        host.session = Some(restored_sid.clone());
                    }
                    match config::checked_remote_argv(&host) {
                        Ok(shell_argv) => {
                            let shell_argv = Rc::new(shell_argv);
                            let pane = create_pane(
                                &self.config,
                                &self.organism_hub,
                                &shell_argv,
                                tab_id,
                                pane_id,
                                TerminalMode::Block,
                                InitialCommands::default(),
                                None,
                                restored_sid.clone(),
                                true,
                                sender,
                            );
                            if restored_remote.is_none() {
                                *restored_remote = Some(RemoteConn {
                                    host,
                                    pane_id,
                                    status: ConnStatus::Connecting,
                                    attempt: 0,
                                    spawn_at: std::time::Instant::now(),
                                });
                            }
                            let widget = pane.widget();
                            panes.push(pane);
                            return widget;
                        }
                        Err(message) => {
                            log::warn!(
                                "Managed remote restore rejected by execution gate: {message}"
                            );
                            self.show_toast(message);
                        }
                    }
                } else if let Some(name) = remote_name {
                    let safe_name = jterm_core::review_input::safe_inline_display(name, 256);
                    log::warn!(
                        "Managed remote '{safe_name}' is no longer configured; restoring a local shell without replaying stale connection data"
                    );
                    self.show_toast(format!(
                        "Remote profile “{safe_name}” was removed or renamed; its saved connection was not restored."
                    ));
                }
                let missing_managed_remote = remote_name.is_some();
                let replay_argv =
                    replay_argv_for_unmanaged_leaf(remote_name.as_deref(), cmds.as_deref());
                // The current configuration remains authoritative for restored
                // local backends. Remote-integrated panes stay on Block because
                // their OSC cwd/session/reconnect signals are part of the
                // restore contract even in VTE compatibility mode.
                let external_cwd = missing_managed_remote
                    || *cwd_external
                    || replay_argv.is_some_and(process::command_uses_external_cwd);
                let remote_integrated = !missing_managed_remote
                    && (restored_sid.is_some()
                        || replay_argv.is_some_and(process::command_requires_block_integration));
                let mode =
                    restored_leaf_mode(self.config.borrow().terminal_mode, remote_integrated);
                // OSC 7 from ssh/mosh/container shells reports a path in that
                // remote namespace. It must neither be passed as a local spawn
                // cwd nor suppress safe argv replay when absent on this host.
                let cwd_available = external_cwd
                    || cwd
                        .as_deref()
                        .is_none_or(crate::host::working_directory_available);
                if !cwd_available {
                    log::warn!(
                        "Restored working directory is unavailable; skipping its command replay"
                    );
                }
                let pane = create_pane(
                    &self.config,
                    &self.organism_hub,
                    &self.shell_argv,
                    tab_id,
                    pane_id,
                    mode,
                    if cwd_available {
                        InitialCommands::from_restored_argv(replay_argv, self.shell_argv.as_ref())
                    } else {
                        InitialCommands::default()
                    },
                    if external_cwd || !cwd_available {
                        None
                    } else {
                        cwd.clone()
                    },
                    if missing_managed_remote {
                        None
                    } else {
                        restored_sid
                    },
                    external_cwd,
                    sender,
                );
                let widget = pane.widget();
                panes.push(pane);
                widget
            }
            session::PaneLayout::Split {
                orientation,
                position,
                start,
                end,
            } => {
                let o = if *orientation == 'v' {
                    gtk::Orientation::Vertical
                } else {
                    gtk::Orientation::Horizontal
                };
                let paned = gtk::Paned::new(o);
                paned.set_hexpand(true);
                paned.set_vexpand(true);
                let start_w =
                    self.build_pane_layout_node(start, tab_id, panes, restored_remote, sender);
                let end_w =
                    self.build_pane_layout_node(end, tab_id, panes, restored_remote, sender);
                paned.set_start_child(Some(&start_w));
                paned.set_end_child(Some(&end_w));
                paned.set_position(*position);
                paned.upcast()
            }
        }
    }

    /// Serialize a tab's live `Paned` widget tree into a persistable `PaneLayout`.
    /// When the tab is pane-zoomed the real tree is detached into `ZoomState`, so
    /// we serialize from there and refill the removed pane's slot.
    pub(crate) fn serialize_layout(&self, tab: &Tab) -> session::PaneLayout {
        let root = tab
            .zoom
            .as_ref()
            .map(|z| z.tree_root.clone())
            .or_else(|| tab.holder.first_child());
        match root {
            Some(w) => self.serialize_widget(tab, &w),
            None => session::PaneLayout::empty_leaf(),
        }
    }

    pub(crate) fn serialize_widget(&self, tab: &Tab, widget: &gtk::Widget) -> session::PaneLayout {
        if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
            let orientation = match paned.orientation() {
                gtk::Orientation::Vertical => 'v',
                _ => 'h',
            };
            let start = self.resolve_child(tab, paned, paned.start_child(), true);
            let end = self.resolve_child(tab, paned, paned.end_child(), false);
            session::PaneLayout::Split {
                orientation,
                position: paned.position(),
                start: Box::new(start),
                end: Box::new(end),
            }
        } else {
            let pane = tab.panes.iter().find(|p| p.widget() == *widget);
            let (mode, cwd, cwd_external, remote_name, sid, cmds) = match pane {
                Some(p) => {
                    let managed_remote =
                        tab.remote.as_ref().filter(|remote| remote.pane_id == p.id);
                    let cmds = snapshot_restorable_command(
                        managed_remote.is_some(),
                        if managed_remote.is_some() {
                            None
                        } else {
                            p.restorable_command()
                        },
                    );
                    let cwd_external = p.cwd_external
                        || cmds
                            .as_deref()
                            .is_some_and(process::command_uses_external_cwd);
                    (
                        match p.mode {
                            TerminalMode::Vte => "vte",
                            TerminalMode::Block => "block",
                            TerminalMode::Unified => "unified",
                        }
                        .to_string(),
                        p.cwd.clone(),
                        cwd_external,
                        managed_remote.map(|remote| remote.host.name.clone()),
                        p.session_id.clone(),
                        cmds,
                    )
                }
                None => ("block".to_string(), None, false, None, None, None),
            };
            session::PaneLayout::captured_leaf(mode, cwd, cwd_external, remote_name, sid, cmds)
        }
    }

    /// A `Paned` child, substituting the zoomed-out pane when its slot is empty.
    pub(crate) fn resolve_child(
        &self,
        tab: &Tab,
        paned: &gtk::Paned,
        child: Option<gtk::Widget>,
        want_start: bool,
    ) -> session::PaneLayout {
        if let Some(c) = child {
            return self.serialize_widget(tab, &c);
        }
        if let Some(z) = &tab.zoom {
            if &z.parent == paned && z.was_start == want_start {
                return self.serialize_widget(tab, &z.pane_widget);
            }
        }
        session::PaneLayout::empty_leaf()
    }

    /// Capture the current tab list as a persistable snapshot, including each
    /// tab's full split layout.
    pub(crate) fn snapshot_session(&self) -> session::SavedSession {
        let tabs = self
            .tabs
            .iter()
            .map(|t| {
                session::SavedTab::captured(
                    t.title.clone(),
                    t.custom_title,
                    t.pinned,
                    t.private_title,
                    self.serialize_layout(t),
                )
            })
            .collect();
        session::SavedSession::captured(self.active, tabs, self.ai_conversation.clone())
    }

    pub(crate) fn persist_session(&self) {
        if self.session_persistence {
            session::save_session(self.snapshot_session());
        }
    }

    pub(crate) fn show_toast(&self, message: impl AsRef<str>) {
        self.toast_overlay
            .add_toast(adw::Toast::new(message.as_ref()));
    }

    /// A toast that offers to take back what it reports.
    ///
    /// The click sends the recovery to `pane_id`, not to the focused pane: the
    /// toast outlives a tab switch, and `Action::UndoClearBlocks` would land
    /// wherever focus happened to be when the button was pressed. Longer than
    /// the default timeout, because an offer nobody has time to read is not an
    /// offer.
    pub(crate) fn show_undo_toast(
        &self,
        pane_id: u64,
        message: &str,
        button: &str,
        undo: crate::terminal::NoticeUndo,
        sender: &ComponentSender<AppModel>,
    ) {
        let toast = adw::Toast::new(message);
        toast.set_button_label(Some(button));
        toast.set_timeout(UNDO_TOAST_TIMEOUT);
        let sender = sender.clone();
        toast.connect_button_clicked(move |_| {
            sender.input(AppMsg::ApplyNoticeUndo { pane_id, undo });
        });
        self.toast_overlay.add_toast(toast);
    }

    /// Drain failures on the GTK thread and turn them into a bounded,
    /// rate-limited warning. The persistence workers already collapse failures
    /// by target until they are drained or a later save succeeds; the UI adds
    /// a short per-operation cooldown so a continuously failing mount does not
    /// make the application unusable with repeated notifications.
    pub(crate) fn report_persistence_failures(&mut self) {
        let failures = crate::persistence::drain_failures();
        if failures.is_empty() {
            return;
        }
        let Some(message) = persistence_failure_notice(
            failures,
            &mut self.persistence_failure_notices,
            std::time::Instant::now(),
        ) else {
            return;
        };
        let message = crate::review_input::safe_inline_display(&message, 1024);
        self.show_toast(message);
    }

    pub(crate) fn persist_config(&self) {
        if self.safe_mode {
            self.show_toast("Settings are temporary and are not saved in safe mode.");
            return;
        }
        let expected = self.config_revision.borrow().clone();
        let result = {
            let config = self.config.borrow();
            config_store::save_config(&config, expected.as_ref())
        };
        match result {
            Ok(revision) => {
                *self.config_revision.borrow_mut() = Some(revision);
            }
            Err(error) if error.is_conflict() => {
                log::warn!("settings save conflict: {error}");
                self.show_toast(
                    "Settings were not saved because the config changed elsewhere. The newer file will reload automatically; reapply your change.",
                );
            }
            Err(error) => {
                log::error!("{error}");
                self.show_toast(format!("Settings were not saved: {error}"));
            }
        }
    }

    pub(crate) fn running_process_summary(&self) -> Option<String> {
        running_process_summary_for_tabs(&self.tabs)
    }

    pub(crate) fn request_quit(&self, sender: &ComponentSender<AppModel>) {
        if let Some(running) = self.running_process_summary() {
            dialogs::confirm_close(&self.window, &running, AppMsg::ForceQuit, sender);
        } else {
            sender.input(AppMsg::ForceQuit);
        }
    }

    pub(crate) fn force_quit(&self) {
        if !self.safe_mode {
            let width = self.content_paned.position().clamp(120, 800) as u32;
            let ai_width = self
                .ai_panel_visible
                .get()
                .then(|| {
                    self.ai_paned
                        .width()
                        .saturating_sub(self.ai_paned.position())
                })
                .filter(|width| *width >= MIN_AI_PANEL_WIDTH as i32)
                .map(|width| (width as u32).clamp(MIN_AI_PANEL_WIDTH, MAX_AI_PANEL_WIDTH));
            let changed = {
                let config = self.config.borrow();
                config.sidebar_width != width
                    || config.sidebar_visible != self.sidebar_visible
                    || config.ai_panel_visible != self.ai_panel_visible.get()
                    || ai_width.is_some_and(|width| config.ai_panel_width != width)
            };
            if changed {
                let mut config = self.config.borrow_mut();
                config.sidebar_width = width;
                config.sidebar_visible = self.sidebar_visible;
                config.ai_panel_visible = self.ai_panel_visible.get();
                if let Some(width) = ai_width {
                    config.ai_panel_width = width;
                }
                drop(config);
                self.persist_config();
            }
        }
        self.persist_session();
        self.persist_agent_session();
        self.agent_close();
        self.close_command_suggestion();
        self.close_all_command_corrections();
        if let Err(error) = command_history::flush_pending(std::time::Duration::from_secs(3)) {
            log::warn!("flush command history on exit: {error}");
        }
        if let Err(error) =
            crate::organism_memory::flush_pending(std::time::Duration::from_millis(500))
        {
            log::warn!("ASCII organism memory could not be queued for shutdown: {error}");
        }
        if let Err(error) = crate::persistence::shutdown(std::time::Duration::from_secs(3)) {
            log::warn!("persistence worker did not flush before shutdown: {error}");
        }
        for failure in crate::persistence::drain_failures() {
            log::error!("persistence failure during shutdown: {failure}");
        }
        self.quit_allowed.set(true);
        self.window.close();
    }

    /// App-level diagnostics plus the active Block backend's PTY/viewport state.
    pub(crate) fn debug_info_snapshot(&self) -> Vec<(String, Vec<(String, String)>)> {
        let cfg = self.config.borrow();
        let total_panes: usize = self.tabs.iter().map(|t| t.panes.len()).sum();
        let active_tab = self.tabs.get(self.active);
        let session = vec![
            ("Tabs".to_string(), self.tabs.len().to_string()),
            ("Total panes".to_string(), total_panes.to_string()),
            (
                "Active tab".to_string(),
                active_tab
                    .map(|t| t.display_title().to_string())
                    .unwrap_or_default(),
            ),
            (
                "Panes in active tab".to_string(),
                active_tab.map(|t| t.panes.len()).unwrap_or(0).to_string(),
            ),
            (
                "Zoomed".to_string(),
                active_tab
                    .map(|t| t.zoom.is_some().to_string())
                    .unwrap_or_else(|| "false".to_string()),
            ),
        ];
        let appearance = vec![
            ("Theme".to_string(), cfg.theme_name.clone()),
            ("Font".to_string(), cfg.font_desc.clone()),
            ("Font scale".to_string(), format!("{:.3}", self.font_scale)),
            ("Opacity".to_string(), format!("{:.2}", self.window_opacity)),
            (
                "Terminal mode".to_string(),
                match cfg.terminal_mode {
                    TerminalMode::Vte => "vte",
                    TerminalMode::Block => "block",
                    TerminalMode::Unified => "unified",
                }
                .to_string(),
            ),
            (
                "Scrollback".to_string(),
                cfg.terminal_scrollback_lines.to_string(),
            ),
        ];
        let config = vec![
            (
                "Keybindings".to_string(),
                self.kbmap.borrow().bindings.len().to_string(),
            ),
            (
                "Remote hosts".to_string(),
                cfg.remote_hosts.len().to_string(),
            ),
            (
                "Startup commands".to_string(),
                cfg.startup_commands.clone().unwrap_or_default(),
            ),
        ];
        let mut info = vec![
            ("Session".to_string(), session),
            ("Appearance".to_string(), appearance),
            ("Config".to_string(), config),
        ];
        if let Some(block_info) = self.active_terminal().and_then(TermCtl::block_debug_info) {
            info.extend(
                block_info
                    .into_iter()
                    .map(|(section, rows)| (format!("Block · {section}"), rows)),
            );
        }
        info
    }

    /// Open a new tab that connects to a remote host via ssh. Uses block mode
    /// so OSC 133 / 7 / 7770 from the remote jsh drive the block UI; for a remote
    /// shell without OSC 133, block.rs falls back to a streaming raw view, which
    /// is no worse than the bare-VTE path this used to take.
    pub(crate) fn add_remote_tab(
        &mut self,
        host: &config::RemoteHost,
        sender: &ComponentSender<AppModel>,
    ) {
        let argv = match config::checked_remote_argv(host) {
            Ok(argv) => Rc::new(argv),
            Err(message) => {
                log::warn!("Remote connection rejected by execution gate: {message}");
                self.show_toast(message);
                return;
            }
        };
        if !self.ensure_persisted_tab_capacity(true) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        // Remote sessions need OSC 133/7/7770 parsing for blocks, cwd updates,
        // resumable session ids, and Agent observations. Keep them on the Block
        // backend even when the local compatibility backend is configured.
        let mode = TerminalMode::Block;
        let pane = create_pane(
            &self.config,
            &self.organism_hub,
            &argv,
            id,
            pane_id,
            mode,
            InitialCommands::default(),
            None,
            None,
            true,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: host.name.clone(),
            custom_title: true,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            private_title: false,
            id,
            zoom: None,
            remote: Some(RemoteConn {
                host: host.clone(),
                pane_id,
                status: ConnStatus::Connecting,
                attempt: 0,
                spawn_at: std::time::Instant::now(),
            }),
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Flip a Connecting remote tab to Connected (first output/cwd seen).
    pub(crate) fn mark_remote_connected(&mut self, idx: usize, pane_id: u64) -> bool {
        if let Some(conn) = self.tabs[idx]
            .remote
            .as_mut()
            .filter(|conn| conn.pane_id == pane_id)
        {
            if conn.status != ConnStatus::Connected {
                conn.status = ConnStatus::Connected;
                return true;
            }
        }
        false
    }

    /// If `tab_id` is a single-pane remote tab that died abnormally, start a
    /// backoff countdown and reconnect in place; returns true when handled (the
    /// caller should NOT close the tab). A clean exit (code 0) returns false so
    /// the tab closes normally.
    pub(crate) fn schedule_remote_reconnect(
        &mut self,
        pane_id: u64,
        code: i32,
        sender: &ComponentSender<AppModel>,
    ) -> bool {
        const MAX_ATTEMPT: u32 = 6;
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return false;
        };
        if !reconnect_target_is_valid(
            self.tabs[idx].panes.len(),
            self.tabs[idx].zoom.is_some(),
            self.tabs[idx].remote.as_ref().map(|conn| conn.pane_id),
            pane_id,
        ) {
            return false;
        }
        let Some(conn) = self.tabs[idx].remote.clone() else {
            return false;
        };
        if code == 0 {
            // User logged out cleanly — drop the connection record, close normally.
            self.tabs[idx].remote = None;
            return false;
        }
        // A link that stayed up a while is treated as a healthy drop (reset
        // backoff); a short-lived one (failed handshake/auth) grows it.
        let stable = conn.spawn_at.elapsed() >= std::time::Duration::from_secs(10);
        let next_attempt = if stable { 0 } else { conn.attempt + 1 };
        if next_attempt > MAX_ATTEMPT {
            log::warn!(
                "[remote] giving up reconnect for '{}' after {} attempts",
                conn.host.name,
                conn.attempt
            );
            if let Some(c) = self.tabs[idx].remote.as_mut() {
                c.status = ConnStatus::Disconnected;
            }
            self.tabs[idx].title = format!("{} — disconnected", conn.host.name);
            self.rebuild_tab_strip(sender);
            return true;
        }
        let delay = if next_attempt == 0 {
            1u64
        } else {
            (1u64 << next_attempt.min(5)).min(30)
        };
        if let Some(c) = self.tabs[idx].remote.as_mut() {
            c.status = ConnStatus::Disconnected;
            c.attempt = next_attempt;
        }
        self.tabs[idx].title = format!("{} — reconnect {delay}s", conn.host.name);
        self.rebuild_tab_strip(sender);
        log::info!(
            "[remote] '{}' disconnected (exit {code}); reconnecting in {delay}s (attempt {next_attempt})",
            conn.host.name
        );

        let remaining = Rc::new(std::cell::Cell::new(delay));
        let s = sender.clone();
        glib::timeout_add_seconds_local(1, move || {
            let left = remaining.get();
            if left > 1 {
                remaining.set(left - 1);
                s.input(AppMsg::RemoteReconnectTick(pane_id, left - 1));
                glib::ControlFlow::Continue
            } else {
                s.input(AppMsg::RemoteReconnectNow(pane_id, next_attempt));
                glib::ControlFlow::Break
            }
        });
        true
    }

    /// Respawn a dead remote tab's connection in place (same tab id / position).
    pub(crate) fn do_remote_reconnect(
        &mut self,
        pane_id: u64,
        attempt: u32,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.remote_reconnect_target_is_valid(pane_id) {
            self.cancel_remote_reconnect(pane_id, sender);
            return;
        }
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(conn) = self.tabs[idx].remote.clone() else {
            return;
        };
        // Validate and build before removing the old widget. A runtime-mutated
        // reconnect target must fail closed without destroying the dead pane.
        let host_now = self.tabs[idx]
            .remote
            .as_ref()
            .map(|c| c.host.clone())
            .unwrap_or(conn.host.clone());
        let argv = match config::checked_remote_argv(&host_now) {
            Ok(argv) => Rc::new(argv),
            Err(message) => {
                log::warn!("Remote reconnect rejected by execution gate: {message}");
                self.show_toast(message);
                self.cancel_remote_reconnect(pane_id, sender);
                return;
            }
        };
        // Swap the dead pane widget for a fresh remote pane.
        let focus_transfer = if self.active == idx {
            self.begin_organism_focus_transfer(None, true)
        } else {
            false
        };
        let old_widget = self.tabs[idx].panes[0].widget();
        self.tabs[idx].holder.remove(&old_widget);
        let new_pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let mode = TerminalMode::Block;
        let pane = create_pane(
            &self.config,
            &self.organism_hub,
            &argv,
            self.tabs[idx].id,
            new_pane_id,
            mode,
            InitialCommands::default(),
            None,
            None,
            true,
            sender,
        );
        self.tabs[idx].holder.append(&pane.widget());
        self.tabs[idx].panes = vec![pane];
        self.tabs[idx].active_pane = 0;
        self.tabs[idx].title = host_now.name.clone();
        if let Some(c) = self.tabs[idx].remote.as_mut() {
            c.pane_id = new_pane_id;
            c.status = ConnStatus::Connecting;
            c.attempt = attempt;
            c.spawn_at = std::time::Instant::now();
        }
        self.finish_organism_focus_transfer(focus_transfer);
        if self.active == idx {
            self.tabs[idx].panes[0].terminal.emit(VteInput::GrabFocus);
        }
        self.rebuild_tab_strip(sender);
    }

    /// Revalidate the reconnect ownership at every timer tick and immediately
    /// before respawn. A split or moved/replaced leaf must never be overwritten
    /// by a stale reconnect timer.
    pub(crate) fn remote_reconnect_target_is_valid(&self, pane_id: u64) -> bool {
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return false;
        };
        reconnect_target_is_valid(
            self.tabs[idx].panes.len(),
            self.tabs[idx].zoom.is_some(),
            self.tabs[idx].remote.as_ref().map(|conn| conn.pane_id),
            pane_id,
        )
    }

    /// Cancel a stale reconnect and remove only its dead remote leaf. Live
    /// siblings created while the countdown was running remain untouched.
    pub(crate) fn cancel_remote_reconnect(
        &mut self,
        pane_id: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        if self.tabs[idx]
            .remote
            .as_ref()
            .is_some_and(|conn| conn.pane_id == pane_id)
        {
            self.tabs[idx].remote = None;
        }
        self.close_pane(pane_id, sender);
    }

    /// Stable-partition the tab list so pinned tabs sort to the front, keeping
    /// `self.active` pointing at the same tab.
    pub(crate) fn reorder_pinned_first(&mut self) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs.sort_by_key(|t| !t.pinned);
        if let Some(id) = active_id {
            if let Some(idx) = self.index_of(id) {
                self.active = idx;
            }
        }
    }

    pub(crate) fn select_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        let previous_pane_id = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .map(|pane| pane.id);
        let next_pane_id = self.tabs[idx]
            .panes
            .get(self.tabs[idx].active_pane)
            .map(|pane| pane.id);
        let hides_previous = self.tabs.get(self.active).map(|tab| tab.id) != Some(id);
        let focus_transfer = self.begin_organism_focus_transfer(next_pane_id, hides_previous);
        let active_pane_changed = search::active_pane_changed(previous_pane_id, next_pane_id);
        if active_pane_changed {
            // Search regexes and highlights live inside a terminal backend.
            // Clear the old owner before changing `self.active`; its delayed
            // Idle response is pane-tagged and therefore cannot reset the new
            // pane's replayed status.
            if let Some(terminal) = self.active_terminal() {
                terminal.emit(VteInput::SearchClear);
            }
        }
        self.active = idx;
        self.stack.set_visible_child_name(&id.to_string());
        {
            let tab = &mut self.tabs[idx];
            tab.bell = false;
            tab.activity = false;
        }
        // Model selection and Stack visibility are now authoritative. Resolve
        // the organism directly; GrabFocus below is only keyboard focus, not
        // the ownership commit signal.
        self.finish_organism_focus_transfer(focus_transfer);
        if !focus_transfer {
            // Callers such as active-tab removal may already have revoked the
            // old owner before indices shifted and made the destination look
            // identity-stable here.
            self.sync_organism_focus();
        }
        let tab = &self.tabs[idx];
        if let Some(pane) = tab.panes.get(tab.active_pane) {
            pane.terminal.emit(VteInput::GrabFocus);
        }
        if active_pane_changed {
            self.search.emit(search::SearchMsg::ActivePaneChanged);
        }
        self.file_tree_goto_current_cwd();
        self.refresh_pane_headers(idx);
        self.rebuild_tab_strip(sender);
        self.refresh_bottom_bar();
    }

    pub(crate) fn close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        if idx == self.active {
            self.begin_organism_focus_transfer(None, true);
        }
        let pane_ids = self.tabs[idx]
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            self.pending_split_spawns.remove(&pane_id);
            self.close_command_suggestion_for_pane(pane_id);
            self.close_command_correction_for_pane(pane_id);
        }
        let closes_agent = self
            .active_agent
            .borrow()
            .as_ref()
            .is_some_and(|session| session.bound_tab == id);
        if closes_agent {
            self.agent_close();
        }
        let tab = self.tabs.remove(idx);
        self.stack.remove(&tab.holder);
        drop(tab);

        if self.tabs.is_empty() {
            self.force_quit();
            return;
        }
        let new_idx = if idx >= self.tabs.len() {
            self.tabs.len() - 1
        } else {
            idx
        };
        let new_id = self.tabs[new_idx].id;
        self.select_tab(new_id, sender);
    }

    /// Re-label a tab from the pane it currently has selected.
    ///
    /// A tab shows its selected pane, so moving focus between the panes of a
    /// split has to move the label with it. Without this the strip kept naming
    /// whichever pane last reported an OSC title. A tab the user renamed keeps
    /// that name.
    pub(crate) fn retitle_tab_from_active_pane(
        &mut self,
        ti: usize,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(tab) = self.tabs.get(ti) else {
            return;
        };
        if tab.custom_title {
            return;
        }
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            return;
        };
        let title = pane
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| default_tab_title(ti as u32 + 1, pane.cwd.as_deref()));
        if tab.title == title {
            return;
        }
        let id = tab.id;
        self.tabs[ti].title = title;
        // A filter is matched against the label, so a changed label can move
        // the row in or out of the filtered set; that needs a full rebuild.
        let filter = self.tab_filter.to_lowercase();
        let visible = filter.is_empty()
            || self.tabs[ti]
                .display_title()
                .to_lowercase()
                .contains(&filter);
        if !visible || !self.update_tab_title_widget(id) {
            self.rebuild_tab_strip(sender);
        }
    }

    /// Bring one tab's pane headers up to date: visibility, numbering, focus
    /// highlight, and the title / directory / running-command line.
    ///
    /// A tab with a single pane hides its header entirely — the tab strip and
    /// window title already name it, and the strip would only cost a row.
    pub(crate) fn refresh_pane_headers(&self, ti: usize) {
        let Some(tab) = self.tabs.get(ti) else {
            return;
        };
        let split = tab.panes.len() > 1;
        for (position, &pane_index) in visual_pane_order(tab).iter().enumerate() {
            let Some(pane) = tab.panes.get(pane_index) else {
                continue;
            };
            pane.frame.set_header_visible(split);
            pane.frame.set_focused(pane_index == tab.active_pane);
            if !split {
                continue;
            }
            let title = pane_header_title(pane.title.as_deref(), pane.cwd.as_deref(), position);
            let cwd = pane.cwd.as_deref().map(abbreviate_home);
            pane.frame.set_status(
                position,
                &title,
                cwd.as_deref(),
                pane.foreground_process().as_deref(),
            );
        }
    }

    /// Refresh the headers of the tab the user is looking at. Background tabs
    /// are not rendered, so polling their PTYs would be pure waste.
    pub(crate) fn refresh_active_pane_headers(&self) {
        self.refresh_pane_headers(self.active);
    }

    /// Exchange two panes' positions in the split tree after a header drag.
    ///
    /// Only the panes move: the tree shape and every divider position the user
    /// arranged stay exactly as they were, and focus follows the dragged pane
    /// into its new slot.
    pub(crate) fn swap_panes(&mut self, dragged: u64, target: u64) {
        let (Some((ti, di)), Some((tj, tj_index))) =
            (self.find_pane(dragged), self.find_pane(target))
        else {
            return;
        };
        // A cross-tab drop would have to move a pane between two widget trees
        // and two tab identities; refuse rather than half-apply it.
        if ti != tj || di == tj_index || self.tabs[ti].zoom.is_some() {
            return;
        }
        if self.tabs[ti]
            .panes
            .iter()
            .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
        {
            return;
        }
        let dragged_widget = self.tabs[ti].panes[di].widget();
        let target_widget = self.tabs[ti].panes[tj_index].widget();
        let focus_transfer = if ti == self.active {
            self.begin_organism_focus_transfer(Some(dragged), true)
        } else {
            false
        };
        if !crate::pane_header::swap_pane_widgets(&dragged_widget, &target_widget) {
            self.finish_organism_focus_transfer(focus_transfer);
            return;
        }
        self.tabs[ti].active_pane = di;
        self.finish_organism_focus_transfer(focus_transfer);
        self.tabs[ti].panes[di].terminal.emit(VteInput::GrabFocus);
        self.refresh_pane_headers(ti);
    }

    /// Move an ordinary one-pane tab into another tab as a directional split.
    /// All identities and both GTK ancestry slots are validated before the
    /// source holder is detached, so an illegal or stale drop is a no-op.
    pub(crate) fn move_tab_to_pane(
        &mut self,
        source_tab_id: u64,
        target_pane_id: u64,
        edge: pane_header::PaneDropEdge,
        sender: &ComponentSender<AppModel>,
    ) {
        let identities: Vec<_> = self.tabs.iter().map(DropTabIdentity::from_tab).collect();
        let Some(plan) = plan_tab_into_pane(&identities, source_tab_id, target_pane_id) else {
            return;
        };
        let (Some(source_index), Some((target_index, target_pane_index))) = (
            self.index_of(plan.source_tab_id),
            self.find_pane(plan.target_pane_id),
        ) else {
            return;
        };
        if self.tabs[target_index].id != plan.target_tab_id {
            return;
        }
        if self.session_persistence
            && self.tabs[target_index].panes.len() >= session::MAX_RESTORED_PANES_PER_TAB
        {
            self.show_toast(format!(
                "A tab can contain at most {} restorable panes.",
                session::MAX_RESTORED_PANES_PER_TAB
            ));
            return;
        }
        if self.tabs[target_index]
            .panes
            .iter()
            .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
            || self.tabs[source_index]
                .panes
                .iter()
                .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
        {
            return;
        }

        let source_widget = self.tabs[source_index].panes[0].widget();
        let source_holder = self.tabs[source_index].holder.clone();
        if source_widget.parent().as_ref() != Some(&source_holder.clone().upcast::<gtk::Widget>())
            || source_holder.first_child().as_ref() != Some(&source_widget)
            || source_widget.next_sibling().is_some()
        {
            return;
        }
        let target_widget = self.tabs[target_index].panes[target_pane_index].widget();
        let Some(target_slot) = LeafSlot::of(&self.tabs[target_index].holder, &target_widget)
        else {
            return;
        };

        // The source tab may currently own the body, and the target becomes
        // selected at the end. End old ownership before either widget tree is
        // detached; `select_tab` performs the final gated reclaim.
        self.begin_organism_focus_transfer(Some(plan.moved_pane_id), true);
        clear_root_focus_before_reparent(&source_widget);
        let mut source_tab = self.tabs.remove(source_index);
        self.stack.remove(&source_tab.holder);
        source_tab.holder.remove(&source_widget);
        let moved = source_tab
            .panes
            .pop()
            .expect("validated ordinary tab owns exactly one pane");
        debug_assert_eq!(moved.id, plan.moved_pane_id);
        let moved_remote = plan
            .moves_remote
            .then(|| source_tab.remote.take())
            .flatten();
        let moved_widget = moved.widget();
        drop(source_tab);

        target_slot.replace_with_split(&target_widget, &moved_widget, edge);
        let Some(target_index) = self.index_of(plan.target_tab_id) else {
            unreachable!("validated target tab remains after removing a different source tab");
        };
        {
            let target = &mut self.tabs[target_index];
            if let Some(remote) = moved_remote {
                debug_assert!(target.remote.is_none());
                target.remote = Some(remote);
            }
            target.panes.push(moved);
            target.active_pane = target.panes.len() - 1;
        }
        if let Some(title) = automatic_tab_title(&self.tabs[target_index], target_index) {
            self.tabs[target_index].title = title;
        }
        if let Some(session) = self.active_agent.borrow_mut().as_mut() {
            if session.bound_pane == plan.moved_pane_id {
                session.bound_tab = plan.target_tab_id;
            }
        }
        self.select_tab(plan.target_tab_id, sender);
    }

    /// Foreground processes running in a tab's panes, formatted for one
    /// confirmation dialog. Ordinary shells are omitted by the PTY probe.
    pub(crate) fn tab_running_process_summary(&self, idx: usize) -> Option<String> {
        running_process_summary_for_tabs(std::iter::once(self.tabs.get(idx)?))
    }

    /// Close a tab, first confirming if a process is still running in it.
    pub(crate) fn request_close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        if let Some(idx) = self.index_of(id) {
            if let Some(running) = self.tab_running_process_summary(idx) {
                dialogs::confirm_close(&self.window, &running, AppMsg::ForceCloseTab(id), sender);
                return;
            }
        }
        self.close_tab(id, sender);
    }

    /// Close a pane, first confirming if a process is still running in it.
    pub(crate) fn request_close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        if let Some((ti, pi)) = self.find_pane(pane_id) {
            if let Some(process) = self.tabs[ti].panes[pi].foreground_process() {
                dialogs::confirm_close(
                    &self.window,
                    &process,
                    AppMsg::ForceClosePane(pane_id),
                    sender,
                );
                return;
            }
        }
        self.close_pane(pane_id, sender);
    }

    /// Move the tab with `src_id` to `to_idx`, preserving which tab is active.
    pub(crate) fn reorder_tab(
        &mut self,
        src_id: u64,
        to_idx: usize,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(from) = self.index_of(src_id) else {
            return;
        };
        let pinned: Vec<_> = self.tabs.iter().map(|tab| tab.pinned).collect();
        let Some(to) = pinned_reorder_destination(&pinned, from, to_idx) else {
            return;
        };
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(aid) = active_id {
            self.active = self.index_of(aid).unwrap_or(0);
        }
        self.rebuild_tab_strip(sender);
    }

    pub(crate) fn switch_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i32;
        let idx = ((self.active as i32 + delta) % len + len) % len;
        let id = self.tabs[idx as usize].id;
        self.select_tab(id, sender);
    }

    /// Reorder the active tab one slot left (-1) or right (+1) and keep it active.
    pub(crate) fn move_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.len() < 2 {
            return;
        }
        let from = self.active as i32;
        let to = from + delta;
        if to < 0 || to >= self.tabs.len() as i32 {
            return;
        }
        let Some(id) = self.tabs.get(self.active).map(|tab| tab.id) else {
            return;
        };
        self.reorder_tab(id, to as usize, sender);
    }

    /// Open a new tab inheriting the active tab's mode, cwd and (custom) title.
    pub(crate) fn duplicate_active_tab(&mut self, sender: &ComponentSender<AppModel>) {
        if !self.ensure_persisted_tab_capacity(true) {
            return;
        }
        let Some(src) = self.tabs.get(self.active) else {
            return;
        };
        let cwd = src
            .panes
            .get(src.active_pane)
            .and_then(Pane::local_cwd)
            .map(str::to_string);
        let title = src.title.clone();
        let custom_title = src.custom_title;
        let private_title = src.private_title;

        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let mode = self.config.borrow().terminal_mode;
        let pane = create_pane(
            &self.config,
            &self.organism_hub,
            &self.shell_argv,
            id,
            pane_id,
            mode,
            InitialCommands::default(),
            cwd,
            None,
            false,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title,
            custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            private_title,
            id,
            zoom: None,
            remote: None,
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Close every marked tab (marking is the multi-select model in anvil).
    pub(crate) fn close_marked_tabs(&mut self, sender: &ComponentSender<AppModel>) {
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.marked)
            .map(|t| t.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let selected: std::collections::HashSet<u64> = ids.iter().copied().collect();
        if let Some(running) = running_process_summary_for_tabs(
            self.tabs.iter().filter(|tab| selected.contains(&tab.id)),
        ) {
            // Capture the current selection in the confirmation message. A
            // cancellation closes nothing and leaves every mark intact; one
            // confirmation closes the whole captured set without modal spam.
            dialogs::confirm_close(
                &self.window,
                &running,
                AppMsg::ForceCloseMarked(ids),
                sender,
            );
        } else {
            self.close_tabs(ids, sender);
        }
    }

    /// Remove a captured set of tab ids as one workspace mutation. This keeps
    /// the current tab selected when possible and rebuilds/persists once.
    pub(crate) fn close_tabs(&mut self, ids: Vec<u64>, sender: &ComponentSender<AppModel>) {
        if ids.is_empty() {
            return;
        }
        let selected: std::collections::HashSet<u64> = ids.into_iter().collect();
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| selected.contains(&tab.id))
        {
            self.begin_organism_focus_transfer(None, true);
        }
        let pane_ids = self
            .tabs
            .iter()
            .filter(|tab| selected.contains(&tab.id))
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.id))
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            self.pending_split_spawns.remove(&pane_id);
            self.close_command_suggestion_for_pane(pane_id);
            self.close_command_correction_for_pane(pane_id);
        }
        let closes_agent = self
            .active_agent
            .borrow()
            .as_ref()
            .is_some_and(|session| selected.contains(&session.bound_tab));
        if closes_agent {
            self.agent_close();
        }
        let Some(first_removed) = self.tabs.iter().position(|tab| selected.contains(&tab.id))
        else {
            return;
        };
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let mut index = 0;
        while index < self.tabs.len() {
            if selected.contains(&self.tabs[index].id) {
                let tab = self.tabs.remove(index);
                self.stack.remove(&tab.holder);
                drop(tab);
            } else {
                index += 1;
            }
        }
        if self.tabs.is_empty() {
            self.force_quit();
            return;
        }
        let new_id = active_id
            .filter(|id| self.index_of(*id).is_some())
            .unwrap_or_else(|| self.tabs[first_removed.min(self.tabs.len() - 1)].id);
        self.select_tab(new_id, sender);
    }

    pub(crate) fn find_pane(&self, pane_id: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            if let Some(pi) = tab.panes.iter().position(|p| p.id == pane_id) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Split the active pane using that pane's backend. The setting controls
    /// future tabs; it must not silently turn a Block split into VTE (or vice
    /// versa) inside an existing mixed-backend workspace.
    pub(crate) fn split_active(
        &mut self,
        orientation: gtk::Orientation,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if self.session_persistence
            && !session::can_add_persisted_pane(tab.panes.len(), self.current_pane_count())
        {
            self.show_toast(format!(
                "Session capacity reached ({} panes per tab, {} panes total). Close a pane before splitting again.",
                session::MAX_RESTORED_PANES_PER_TAB,
                session::MAX_RESTORED_PANES_TOTAL,
            ));
            return;
        }
        if tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let tab_id = tab.id;
        let api = tab.active_pane;
        let source_pane_id = tab.panes[api].id;
        if tab
            .panes
            .iter()
            .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
        {
            // Do not nest another transaction below a leaf whose asynchronous
            // VTE spawn may still require exact structural rollback.
            return;
        }
        let cur_widget = tab.panes[api].widget();
        let wd = tab.panes[api].local_cwd().map(str::to_string);
        let mode = tab.panes[api].terminal.mode();
        // Validate the exact holder ancestry before even launching the new
        // component. A stale/malformed leaf must not create an unattached pane
        // in the model or mutate a foreign tab tree.
        let Some(slot) = LeafSlot::of(&tab.holder, &cur_widget) else {
            log::error!("Cannot split pane {source_pane_id}: invalid pane-tree ancestry");
            return;
        };

        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let config = self.config.clone();
        let organism_hub = self.organism_hub.clone();
        let shell_argv = self.shell_argv.clone();
        let split = prepare_then_commit(
            || {
                let pane = create_pane(
                    &config,
                    &organism_hub,
                    &shell_argv,
                    tab_id,
                    pane_id,
                    mode,
                    InitialCommands::default(),
                    wd,
                    None,
                    false,
                    sender,
                );
                if let Some(error) = pane.terminal.synchronous_launch_error() {
                    Err(error)
                } else {
                    Ok(pane)
                }
            },
            |new_pane| {
                let new_widget = new_pane.widget();
                let edge = if orientation == gtk::Orientation::Horizontal {
                    pane_header::PaneDropEdge::Right
                } else {
                    pane_header::PaneDropEdge::Bottom
                };
                let focus_transfer = self.begin_organism_focus_transfer(Some(pane_id), true);
                slot.replace_with_split(&cur_widget, &new_widget, edge);

                {
                    let tab = &mut self.tabs[ti];
                    tab.panes.push(new_pane);
                    tab.active_pane = tab.panes.len() - 1;
                    // libvte reports spawn completion asynchronously. Until its
                    // success event arrives, this stable id can roll back exactly
                    // the new leaf without guessing from a shifted pane index.
                    if matches!(mode, TerminalMode::Vte) {
                        self.pending_split_spawns
                            .insert(pane_id, (tab_id, source_pane_id));
                    } else if let Some(root) = tab.holder.first_child() {
                        schedule_pane_rebalance(root);
                    }
                }
                self.finish_organism_focus_transfer(focus_transfer);
                self.tabs[ti].panes[self.tabs[ti].active_pane]
                    .terminal
                    .emit(VteInput::GrabFocus);
                // The tab just became split, so every pane's header appears now.
                self.refresh_pane_headers(ti);
            },
        );
        if let Err(error) = split {
            log::error!("Terminal spawn failed while preparing split pane {pane_id}: {error}");
            self.show_toast(format!(
                "Terminal failed to start: {error}. The existing pane layout was left unchanged."
            ));
        }
    }

    /// Remove exactly the asynchronously spawned split leaf and promote its
    /// sibling back into the validated slot. The stable pane id, not a shifted
    /// vector index, is the rollback authority.
    pub(crate) fn rollback_failed_split(
        &mut self,
        pane_id: u64,
        source_tab_id: u64,
        source_pane_id: u64,
    ) -> bool {
        let Some((tab_index, pane_index)) = self.find_pane(pane_id) else {
            return false;
        };
        if self.tabs[tab_index].id != source_tab_id
            || self.tabs[tab_index].panes.len() <= 1
            || self.tabs[tab_index].zoom.is_some()
        {
            return false;
        }
        let was_active_tab = self.active == tab_index;
        let failed_was_active = self.tabs[tab_index].active_pane == pane_index;
        let focus_transfer = if was_active_tab && failed_was_active {
            self.begin_organism_focus_transfer(Some(source_pane_id), true)
        } else {
            false
        };
        let Some(removed) = self.detach_pane_from_tab(tab_index, pane_index) else {
            self.finish_organism_focus_transfer(focus_transfer);
            return false;
        };
        drop(removed);

        let Some(source_index) = self.tabs[tab_index]
            .panes
            .iter()
            .position(|pane| pane.id == source_pane_id)
        else {
            log::error!(
                "Split rollback removed pane {pane_id}, but source pane {source_pane_id} disappeared"
            );
            self.refresh_pane_headers(tab_index);
            self.finish_organism_focus_transfer(focus_transfer);
            return true;
        };
        if failed_was_active {
            self.tabs[tab_index].active_pane = source_index;
        }
        self.refresh_pane_headers(tab_index);
        self.finish_organism_focus_transfer(focus_transfer);
        if was_active_tab && failed_was_active {
            self.tabs[tab_index].panes[source_index]
                .terminal
                .emit(VteInput::GrabFocus);
        }
        true
    }

    /// Remove a pane from its tab, collapsing the Paned tree and promoting the
    /// sibling. Closes the whole tab if it was the last pane.
    pub(crate) fn close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        let Some((mut ti, mut pi)) = self.find_pane(pane_id) else {
            self.pending_split_spawns.remove(&pane_id);
            return;
        };
        let pending_in_tab = self.tabs[ti].panes.iter().find_map(|pane| {
            self.pending_split_spawns
                .get(&pane.id)
                .copied()
                .map(|metadata| (pane.id, metadata))
        });
        if let Some((pending_pane_id, (source_tab_id, source_pane_id))) = pending_in_tab {
            self.pending_split_spawns.remove(&pending_pane_id);
            if pending_pane_id != pane_id {
                // Closing another leaf while a VTE split is still preparing
                // would destroy the exact sibling slot needed on failure.
                // Abort that split first, then honor the requested close on
                // the restored tree.
                if !self.rollback_failed_split(pending_pane_id, source_tab_id, source_pane_id) {
                    self.pending_split_spawns
                        .insert(pending_pane_id, (source_tab_id, source_pane_id));
                    log::error!(
                        "Refusing to close pane {pane_id}: pending split {pending_pane_id} could not be rolled back safely"
                    );
                    return;
                }
                let Some((new_ti, new_pi)) = self.find_pane(pane_id) else {
                    return;
                };
                ti = new_ti;
                pi = new_pi;
            }
        }
        self.close_command_suggestion_for_pane(pane_id);
        self.close_command_correction_for_pane(pane_id);
        let closes_agent = self
            .active_agent
            .borrow()
            .as_ref()
            .is_some_and(|session| session.bound_pane == pane_id);
        if closes_agent {
            self.agent_close();
        }
        let focus_transfer = if ti == self.active {
            self.begin_organism_focus_transfer(None, true)
        } else {
            false
        };
        if self.tabs[ti].zoom.is_some() {
            self.toggle_pane_zoom_for(ti);
        }
        if self.tabs[ti].panes.len() == 1 {
            let tab_id = self.tabs[ti].id;
            self.close_tab(tab_id, sender);
            return;
        }
        let was_remote = self.tabs[ti]
            .remote
            .as_ref()
            .is_some_and(|conn| conn.pane_id == pane_id);
        let Some(removed) = self.detach_pane_from_tab(ti, pi) else {
            log::error!("Failed to detach pane {pane_id} from tab widget tree");
            self.finish_organism_focus_transfer(focus_transfer);
            return;
        };
        {
            let tab = &mut self.tabs[ti];
            if was_remote {
                tab.remote = None;
            }
        }
        drop(removed);
        // Numbering shifted, and dropping back to one pane hides the headers.
        self.refresh_pane_headers(ti);
        self.finish_organism_focus_transfer(focus_transfer);
        if ti == self.active {
            let ap = self.tabs[ti].active_pane;
            self.tabs[ti].panes[ap].terminal.emit(VteInput::GrabFocus);
        }
    }

    pub(crate) fn cycle_pane_focus(&mut self, delta: i32) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let n = tab.panes.len() as i32;
        if n <= 1 {
            return;
        }
        let cur = tab.active_pane as i32;
        let next = ((cur + delta) % n + n) % n;
        let next = next as usize;
        let next_pane_id = tab.panes[next].id;
        let focus_transfer = self.begin_organism_focus_transfer(Some(next_pane_id), false);
        self.tabs[self.active].active_pane = next;
        self.finish_organism_focus_transfer(focus_transfer);
        self.tabs[self.active].panes[next]
            .terminal
            .emit(VteInput::GrabFocus);
        self.refresh_active_pane_headers();
    }

    pub(crate) fn focus_pane_directional(&mut self, direction: Direction) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() <= 1 {
            return;
        }
        let holder: gtk::Widget = tab.holder.clone().upcast();
        let api = tab.active_pane;
        let focused_widget = tab.panes[api].widget();
        let Some(fb) = focused_widget.compute_bounds(&holder) else {
            return;
        };
        let fcx = fb.x() + fb.width() / 2.0;
        let fcy = fb.y() + fb.height() / 2.0;

        let mut best: Option<(f32, usize)> = None;
        for (i, pane) in tab.panes.iter().enumerate() {
            if i == api {
                continue;
            }
            let w = pane.widget();
            let Some(b) = w.compute_bounds(&holder) else {
                continue;
            };
            let cx = b.x() + b.width() / 2.0;
            let cy = b.y() + b.height() / 2.0;
            let dx = cx - fcx;
            let dy = cy - fcy;
            let in_dir = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };
            if !in_dir {
                continue;
            }
            let dist = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };
            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, i));
            }
        }

        if let Some((_, i)) = best {
            let next_pane_id = self.tabs[self.active].panes[i].id;
            let focus_transfer = self.begin_organism_focus_transfer(Some(next_pane_id), false);
            self.tabs[self.active].active_pane = i;
            self.finish_organism_focus_transfer(focus_transfer);
            self.tabs[self.active].panes[i]
                .terminal
                .emit(VteInput::GrabFocus);
        }
    }

    pub(crate) fn resize_pane(&mut self, target: gtk::Orientation, delta: i32) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let api = tab.active_pane;
        let mut widget = tab.panes[api].widget().parent();
        while let Some(cur) = widget {
            if let Ok(paned) = cur.clone().downcast::<gtk::Paned>() {
                if paned.orientation() == target {
                    let new_pos = (paned.position() + delta).max(0);
                    paned.set_position(new_pos);
                    return;
                }
            }
            widget = cur.parent();
        }
    }

    pub(crate) fn toggle_pane_zoom(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab
            .panes
            .iter()
            .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
            || (tab.zoom.is_none() && tab.panes.len() <= 1)
        {
            return;
        }
        let next_pane = self.active_pane_id();
        let focus_transfer = self.begin_organism_focus_transfer(next_pane, true);
        self.toggle_pane_zoom_for(self.active);
        self.finish_organism_focus_transfer(focus_transfer);
    }

    pub(crate) fn toggle_pane_zoom_for(&mut self, ti: usize) {
        if self.tabs.get(ti).is_some_and(|tab| {
            tab.panes
                .iter()
                .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
        }) {
            return;
        }
        let Some(tab) = self.tabs.get_mut(ti) else {
            return;
        };
        if let Some(z) = tab.zoom.take() {
            tab.holder.remove(&z.pane_widget);
            if z.was_start {
                z.parent.set_start_child(Some(&z.pane_widget));
            } else {
                z.parent.set_end_child(Some(&z.pane_widget));
            }
            tab.holder.append(&z.tree_root);
            let ap = tab.active_pane;
            tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        } else {
            if tab.panes.len() <= 1 {
                return;
            }
            let api = tab.active_pane;
            let pane_widget = tab.panes[api].widget();
            let Some(parent) = pane_widget.parent() else {
                return;
            };
            let Ok(parent_paned) = parent.downcast::<gtk::Paned>() else {
                return;
            };
            let was_start = parent_paned.start_child().as_ref() == Some(&pane_widget);
            let Some(tree_root) = tab.holder.first_child() else {
                return;
            };
            if was_start {
                parent_paned.set_start_child(None::<&gtk::Widget>);
            } else {
                parent_paned.set_end_child(None::<&gtk::Widget>);
            }
            tab.holder.remove(&tree_root);
            tab.holder.append(&pane_widget);
            tab.zoom = Some(ZoomState {
                tree_root,
                pane_widget: pane_widget.clone(),
                parent: parent_paned,
                was_start,
            });
            tab.panes[api].terminal.emit(VteInput::GrabFocus);
        }
        // Zooming leaves only one leaf in the tree, so pane numbering has to
        // be recomputed in both directions.
        self.refresh_pane_headers(ti);
    }

    /// Detach the active pane from a split tab and host it in a brand-new tab.
    pub(crate) fn move_pane_to_new_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(pane_id) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .map(|pane| pane.id)
        else {
            return;
        };
        self.promote_pane_to_tab(pane_id, None, true, sender);
    }

    /// Promote any stable pane id from a split into a new ordinary tab. A row
    /// drop can anchor the insertion beside another stable tab id; blank tab
    /// bar space inserts beside the source tab.
    pub(crate) fn promote_pane_to_tab(
        &mut self,
        pane_id: u64,
        anchor_tab_id: Option<u64>,
        after: bool,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.ensure_persisted_tab_capacity(false) {
            return;
        }
        let identities: Vec<_> = self.tabs.iter().map(DropTabIdentity::from_tab).collect();
        let Some(plan) = plan_pane_into_tab(&identities, pane_id, anchor_tab_id, after) else {
            return;
        };
        let Some((source_index, pane_index)) = self.find_pane(plan.pane_id) else {
            return;
        };
        if self.tabs[source_index].id != plan.source_tab_id {
            return;
        }
        let private_title = self.tabs[source_index].private_title;
        if self.tabs[source_index]
            .panes
            .iter()
            .any(|pane| self.pending_split_spawns.contains_key(&pane.id))
        {
            return;
        }
        let focus_transfer = self.begin_organism_focus_transfer(Some(plan.pane_id), true);
        let Some(moved) = self.detach_pane_from_tab(source_index, pane_index) else {
            log::error!("Failed to detach pane {pane_id} into a new tab");
            self.finish_organism_focus_transfer(focus_transfer);
            return;
        };

        let remote = plan
            .moves_remote
            .then(|| self.tabs[source_index].remote.take())
            .flatten();
        let new_id = self.next_id;
        self.next_id += 1;
        let mw = moved.widget();
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&mw);
        self.stack.add_named(&holder, Some(&new_id.to_string()));
        let number = self.tabs.len() as u32 + 1;
        let (title, custom_title) = remote.as_ref().map_or_else(
            || (default_tab_title(number, moved.cwd.as_deref()), false),
            |conn| (conn.host.name.clone(), true),
        );
        let new_tab = Tab {
            holder,
            panes: vec![moved],
            active_pane: 0,
            title,
            custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            private_title,
            id: new_id,
            zoom: None,
            remote,
        };
        if let Some(session) = self.active_agent.borrow_mut().as_mut() {
            if session.bound_pane == plan.pane_id {
                session.bound_tab = new_id;
            }
        }
        let insert_at = match plan.anchor_tab_id {
            Some(anchor_id) => {
                let Some(anchor_index) = self.index_of(anchor_id) else {
                    unreachable!("validated tab-row anchor remains during pane detach");
                };
                anchor_index + usize::from(plan.after)
            }
            None => {
                let Some(source_index) = self.index_of(plan.source_tab_id) else {
                    unreachable!("detaching a non-final pane keeps its source tab");
                };
                source_index + 1
            }
        }
        .min(self.tabs.len());
        self.tabs.insert(insert_at, new_tab);
        // The source tab lost a pane and may be back to one; `select_tab`
        // below only refreshes the destination.
        if let Some(source_index) = self.index_of(plan.source_tab_id) {
            if let Some(title) = automatic_tab_title(&self.tabs[source_index], source_index) {
                self.tabs[source_index].title = title;
            }
            self.refresh_pane_headers(source_index);
        }
        self.select_tab(new_id, sender);
    }

    /// Remove a non-final pane from both the GTK split tree and the tab model.
    /// Keeping these mutations together prevents either representation from
    /// advancing when structural validation fails.
    fn detach_pane_from_tab(&mut self, tab_index: usize, pane_index: usize) -> Option<Pane> {
        let tab = self.tabs.get(tab_index)?;
        if tab.panes.len() <= 1 || pane_index >= tab.panes.len() {
            return None;
        }

        let leaf = tab.panes[pane_index].widget();
        detach_leaf_and_promote(&tab.holder, &leaf)?;

        let tab = self.tabs.get_mut(tab_index)?;
        let removed = tab.panes.remove(pane_index);
        tab.active_pane = active_index_after_remove(tab.active_pane, pane_index, tab.panes.len());
        Some(removed)
    }
}

fn reconnect_target_is_valid(
    panes_len: usize,
    zoomed: bool,
    remote_pane_id: Option<u64>,
    event_pane_id: u64,
) -> bool {
    panes_len == 1 && !zoomed && remote_pane_id == Some(event_pane_id)
}

fn persistence_failure_notice(
    failures: Vec<crate::persistence::PersistenceFailure>,
    reported: &mut std::collections::HashMap<String, std::time::Instant>,
    now: std::time::Instant,
) -> Option<String> {
    reported.retain(|_, last_reported| {
        now.saturating_duration_since(*last_reported) < PERSISTENCE_FAILURE_NOTICE_COOLDOWN
    });

    let mut reportable = Vec::new();
    for failure in failures {
        let is_recent = reported
            .get(&failure.operation)
            .is_some_and(|last_reported| {
                now.saturating_duration_since(*last_reported) < PERSISTENCE_FAILURE_NOTICE_COOLDOWN
            });
        if is_recent {
            continue;
        }
        reported.insert(failure.operation.clone(), now);
        reportable.push(failure);
    }

    match reportable.as_slice() {
        [] => None,
        [failure] => Some(format!(
            "Background save failed — {}: {}. Recent state may not be saved.",
            crate::review_input::safe_inline_display(&failure.operation, 160),
            crate::review_input::safe_inline_display(&failure.error, 512)
        )),
        failures => {
            let operations = failures
                .iter()
                .take(4)
                .map(|failure| crate::review_input::safe_inline_display(&failure.operation, 96))
                .collect::<Vec<_>>()
                .join(", ");
            let remainder = failures.len().saturating_sub(4);
            let remainder = (remainder > 0).then(|| format!(" (+{remainder} more)"));
            Some(format!(
                "Background saves failed for {} operations: {operations}{}. Recent state may not be saved.",
                failures.len(),
                remainder.as_deref().unwrap_or("")
            ))
        }
    }
}

#[cfg(test)]
mod pane_tree_tests {
    use super::{
        abbreviate_prefix, active_index_after_remove, balanced_split_position, combined_axis_span,
        detach_leaf_and_promote, format_running_process_summary, pane_header_title,
        persistence_failure_notice, pinned_reorder_destination, plan_pane_into_tab,
        plan_tab_into_pane, prepare_then_commit, reconnect_target_is_valid,
        replay_argv_for_unmanaged_leaf, restored_leaf_mode, snapshot_restorable_command,
        tab_drop_preview_is_valid, DropTabIdentity, LeafSlot, PaneIntoTabPlan, TabIntoPanePlan,
        PERSISTENCE_FAILURE_NOTICE_COOLDOWN,
    };
    use crate::config::TerminalMode;
    use crate::workspace::ConnStatus;
    use relm4::gtk;
    use relm4::gtk::prelude::*;
    use std::cell::Cell;

    fn persistence_failure(operation: &str, error: &str) -> crate::persistence::PersistenceFailure {
        crate::persistence::PersistenceFailure {
            operation: operation.to_string(),
            error: error.to_string(),
        }
    }

    #[test]
    fn persistence_failure_notices_deduplicate_and_rearm_after_the_cooldown() {
        let start = std::time::Instant::now();
        let mut reported = std::collections::HashMap::new();
        let repeated = vec![
            persistence_failure("save session snapshot", "disk full"),
            persistence_failure("save session snapshot", "disk full again"),
        ];

        let first = persistence_failure_notice(repeated.clone(), &mut reported, start)
            .expect("first failure is visible");
        assert!(first.contains("save session snapshot"), "{first}");
        assert_eq!(reported.len(), 1);
        assert!(
            persistence_failure_notice(
                repeated.clone(),
                &mut reported,
                start + std::time::Duration::from_secs(1),
            )
            .is_none(),
            "the same operation must not create a toast storm"
        );

        let rearmed = persistence_failure_notice(
            repeated,
            &mut reported,
            start + PERSISTENCE_FAILURE_NOTICE_COOLDOWN,
        )
        .expect("the operation is reportable after its cooldown");
        assert!(rearmed.contains("disk full"), "{rearmed}");
    }

    #[test]
    fn persistence_failure_notice_combines_distinct_operations() {
        let mut reported = std::collections::HashMap::new();
        let notice = persistence_failure_notice(
            vec![
                persistence_failure("save session snapshot", "disk full"),
                persistence_failure("Save ASCII organism memory", "permission denied"),
            ],
            &mut reported,
            std::time::Instant::now(),
        )
        .expect("distinct failures should be visible");

        assert!(notice.contains("2 operations"), "{notice}");
        assert!(notice.contains("save session snapshot"), "{notice}");
        assert!(notice.contains("Save ASCII organism memory"), "{notice}");
    }

    fn drop_tab(
        id: u64,
        pane_ids: &[u64],
        zoomed: bool,
        remote_pane_id: Option<u64>,
    ) -> DropTabIdentity {
        drop_tab_with_status(
            id,
            pane_ids,
            zoomed,
            remote_pane_id,
            remote_pane_id.map(|_| ConnStatus::Connected),
        )
    }

    fn drop_tab_with_status(
        id: u64,
        pane_ids: &[u64],
        zoomed: bool,
        remote_pane_id: Option<u64>,
        remote_status: Option<ConnStatus>,
    ) -> DropTabIdentity {
        DropTabIdentity {
            id,
            pane_ids: pane_ids.to_vec(),
            zoomed,
            remote_pane_id,
            remote_status,
        }
    }

    #[test]
    fn failed_split_preparation_never_runs_the_structural_commit() {
        let committed = Cell::new(false);
        let result = prepare_then_commit(
            || -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "injected terminal spawn failure",
                ))
            },
            |()| committed.set(true),
        );

        assert_eq!(
            result.expect_err("preparation must fail").kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!committed.get());
    }

    #[test]
    fn repeated_same_axis_splits_rebalance_to_equal_leaf_slots() {
        assert_eq!(balanced_split_position(1_200, 1, 1), Some(600));
        assert_eq!(balanced_split_position(1_200, 1, 2), Some(400));
        assert_eq!(balanced_split_position(1_200, 2, 1), Some(800));
        assert_eq!(balanced_split_position(1_200, 3, 1), Some(900));

        let nested_right_span = combined_axis_span(true, 1, 1);
        assert_eq!(nested_right_span, 2);

        // Shape: A | (B | C). A receives 1/3 of the root and B/C split the
        // remaining 2/3 equally, producing three 400px leaves.
        let root_position = balanced_split_position(1_200, 1, nested_right_span).unwrap();
        let nested_position = balanced_split_position(1_200 - root_position, 1, 1).unwrap();
        assert_eq!(
            [
                root_position,
                nested_position,
                1_200 - root_position - nested_position,
            ],
            [400, 400, 400]
        );

        // Cross-axis children stack rather than consuming another slot on the
        // measured axis, which keeps mixed grids proportional.
        assert_eq!(combined_axis_span(false, 2, 1), 2);
    }

    #[test]
    fn balanced_split_position_rejects_unallocated_or_empty_inputs() {
        assert_eq!(balanced_split_position(0, 1, 1), None);
        assert_eq!(balanced_split_position(1, 1, 1), None);
        assert_eq!(balanced_split_position(100, 0, 1), None);
        assert_eq!(balanced_split_position(100, 1, 0), None);
    }

    #[test]
    fn ordinary_tab_drop_plan_keeps_stable_ids_across_index_shift() {
        let mut tabs = vec![
            drop_tab(10, &[100], false, None),
            drop_tab(20, &[200, 201], false, None),
            drop_tab(30, &[300], false, None),
        ];
        let plan = plan_tab_into_pane(&tabs, 10, 201).unwrap();
        assert_eq!(
            plan,
            TabIntoPanePlan {
                source_tab_id: 10,
                moved_pane_id: 100,
                target_tab_id: 20,
                target_pane_id: 201,
                moves_remote: false,
            }
        );

        // Removing the source shifts the target's index, but not anything the
        // mutation plan carries across the GTK reparenting boundary.
        tabs.remove(0);
        assert_eq!(
            tabs.iter().position(|tab| tab.id == plan.target_tab_id),
            Some(0)
        );
        assert!(tabs[0].pane_ids.contains(&plan.target_pane_id));
    }

    #[test]
    fn tab_drop_planner_rejects_self_multi_pane_zoom_and_remote_conflicts() {
        let base = vec![
            drop_tab(10, &[100], false, None),
            drop_tab(20, &[200, 201], false, None),
        ];
        assert!(plan_tab_into_pane(&base, 10, 100).is_none());
        assert!(plan_tab_into_pane(&base, 20, 100).is_none());

        let mut zoomed = base.clone();
        zoomed[1].zoomed = true;
        assert!(plan_tab_into_pane(&zoomed, 10, 200).is_none());

        let both_remote = vec![
            drop_tab(10, &[100], false, Some(100)),
            drop_tab(20, &[200], false, Some(200)),
        ];
        assert!(plan_tab_into_pane(&both_remote, 10, 200).is_none());
        assert_eq!(both_remote[0].pane_ids, vec![100]);
        assert_eq!(both_remote[1].pane_ids, vec![200]);
    }

    #[test]
    fn remote_ordinary_tab_moves_only_when_target_can_own_its_connection() {
        let tabs = vec![
            drop_tab(10, &[100], false, Some(100)),
            drop_tab(20, &[200], false, None),
        ];
        assert_eq!(
            plan_tab_into_pane(&tabs, 10, 200),
            Some(TabIntoPanePlan {
                source_tab_id: 10,
                moved_pane_id: 100,
                target_tab_id: 20,
                target_pane_id: 200,
                moves_remote: true,
            })
        );
    }

    #[test]
    fn tab_drop_rejects_a_remote_reconnect_countdown_on_either_side() {
        let disconnected_source = vec![
            drop_tab_with_status(10, &[100], false, Some(100), Some(ConnStatus::Disconnected)),
            drop_tab(20, &[200], false, None),
        ];
        assert!(plan_tab_into_pane(&disconnected_source, 10, 200).is_none());

        let disconnected_target = vec![
            drop_tab(10, &[100], false, None),
            drop_tab_with_status(20, &[200], false, Some(200), Some(ConnStatus::Disconnected)),
        ];
        assert!(plan_tab_into_pane(&disconnected_target, 10, 200).is_none());
        assert!(!tab_drop_preview_is_valid(&disconnected_target, 10, 20));
    }

    #[test]
    fn tab_drop_hover_requires_one_unzoomed_source_pane_and_viable_target() {
        let ordinary = vec![
            drop_tab(10, &[100], false, None),
            drop_tab(20, &[200, 201], false, None),
        ];
        assert!(tab_drop_preview_is_valid(&ordinary, 10, 20));
        assert!(!tab_drop_preview_is_valid(&ordinary, 20, 10));
        assert!(!tab_drop_preview_is_valid(&ordinary, 10, 10));

        let zoomed_target = vec![
            drop_tab(10, &[100], false, None),
            drop_tab(20, &[200], true, None),
        ];
        assert!(!tab_drop_preview_is_valid(&zoomed_target, 10, 20));
    }

    #[test]
    fn native_reorder_clamps_both_sides_of_the_pinned_prefix() {
        let mut tabs = vec![(10, true), (20, true), (30, false), (40, false)];

        let destination = pinned_reorder_destination(
            &tabs.iter().map(|(_, pinned)| *pinned).collect::<Vec<_>>(),
            3,
            0,
        )
        .unwrap();
        let moved = tabs.remove(3);
        tabs.insert(destination, moved);
        assert_eq!(tabs, vec![(10, true), (20, true), (40, false), (30, false)]);

        let destination = pinned_reorder_destination(
            &tabs.iter().map(|(_, pinned)| *pinned).collect::<Vec<_>>(),
            0,
            3,
        )
        .unwrap();
        let moved = tabs.remove(0);
        tabs.insert(destination, moved);
        assert_eq!(tabs, vec![(20, true), (10, true), (40, false), (30, false)]);
        assert!(tabs.iter().take(2).all(|(_, pinned)| *pinned));
        assert!(tabs.iter().skip(2).all(|(_, pinned)| !*pinned));
    }

    #[test]
    fn leaf_slot_requires_the_exact_holder_tree_and_clears_focus_before_reparent() {
        if gtk::init().is_err() {
            // Pure planner coverage still runs on headless builders; the live
            // GTK boundary is exercised whenever a display backend exists.
            return;
        }

        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let root_split = gtk::Paned::new(gtk::Orientation::Horizontal);
        let nested_split = gtk::Paned::new(gtk::Orientation::Vertical);
        let target = gtk::Button::with_label("target");
        let sibling = gtk::Button::with_label("sibling");
        nested_split.set_start_child(Some(&target));
        nested_split.set_end_child(Some(&sibling));
        root_split.set_start_child(Some(&nested_split));
        root_split.set_end_child(Some(&gtk::Button::with_label("outer sibling")));
        holder.append(&root_split);

        let target_widget = target.clone().upcast::<gtk::Widget>();
        assert!(matches!(
            LeafSlot::of(&holder, &target_widget),
            Some(LeafSlot::PanedStart(_))
        ));

        let foreign_holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let foreign_split = gtk::Paned::new(gtk::Orientation::Horizontal);
        let foreign_target = gtk::Button::with_label("foreign");
        foreign_split.set_start_child(Some(&foreign_target));
        foreign_split.set_end_child(Some(&gtk::Button::with_label("other")));
        foreign_holder.append(&foreign_split);
        assert!(LeafSlot::of(&holder, &foreign_target.upcast()).is_none());

        let window = gtk::Window::new();
        window.set_child(Some(&holder));
        gtk::prelude::RootExt::set_focus(&window, Some(&target));
        assert!(gtk::prelude::RootExt::focus(&window).is_some_and(|focus| focus == target_widget));

        let moved = gtk::Button::with_label("moved");
        let moved_widget = moved.clone().upcast::<gtk::Widget>();
        let slot = LeafSlot::of(&holder, &target_widget).expect("validated holder ancestry");
        slot.replace_with_split(
            &target_widget,
            &moved_widget,
            crate::pane_header::PaneDropEdge::Left,
        );

        assert!(gtk::prelude::RootExt::focus(&window).is_none());
        let inserted = nested_split
            .start_child()
            .and_then(|child| child.downcast::<gtk::Paned>().ok())
            .expect("target slot replaced by a split");
        assert_eq!(inserted.start_child().as_ref(), Some(&moved_widget));
        assert_eq!(inserted.end_child().as_ref(), Some(&target_widget));
        gtk::prelude::RootExt::set_focus(&window, Some(&moved));
        assert!(gtk::prelude::RootExt::focus(&window).is_some_and(|focus| focus == moved_widget));
        assert_eq!(
            detach_leaf_and_promote(&holder, &moved_widget),
            Some(target_widget.clone()),
            "failed split rollback promotes the exact original leaf"
        );
        assert_eq!(
            nested_split.start_child().as_ref(),
            Some(&target_widget),
            "rollback restores the original tree slot"
        );
        assert!(gtk::prelude::RootExt::focus(&window).is_none());
        window.set_child(None::<&gtk::Widget>);

        // A hostile nested tree under another holder used to pass the
        // immediate-grandparent check and get partially collapsed.
        let foreign_holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let foreign_root = gtk::Paned::new(gtk::Orientation::Horizontal);
        let foreign_parent = gtk::Paned::new(gtk::Orientation::Vertical);
        let foreign_leaf = gtk::Button::with_label("foreign leaf");
        let foreign_sibling = gtk::Button::with_label("foreign sibling");
        foreign_parent.set_start_child(Some(&foreign_leaf));
        foreign_parent.set_end_child(Some(&foreign_sibling));
        foreign_root.set_start_child(Some(&foreign_parent));
        foreign_root.set_end_child(Some(&gtk::Button::with_label("foreign root sibling")));
        foreign_holder.append(&foreign_root);
        let foreign_leaf_widget = foreign_leaf.clone().upcast::<gtk::Widget>();
        assert!(detach_leaf_and_promote(&holder, &foreign_leaf_widget).is_none());
        assert_eq!(
            foreign_parent.start_child().as_ref(),
            Some(&foreign_leaf_widget)
        );

        // Even the correct holder is rejected when it owns more than the one
        // exact split root; otherwise collapsing can silently discard a peer.
        let multi_holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let multi_root = gtk::Paned::new(gtk::Orientation::Horizontal);
        let multi_parent = gtk::Paned::new(gtk::Orientation::Vertical);
        let multi_leaf = gtk::Button::with_label("multi leaf");
        let multi_sibling = gtk::Button::with_label("multi sibling");
        multi_parent.set_start_child(Some(&multi_leaf));
        multi_parent.set_end_child(Some(&multi_sibling));
        multi_root.set_start_child(Some(&multi_parent));
        multi_root.set_end_child(Some(&gtk::Button::with_label("root sibling")));
        multi_holder.append(&multi_root);
        multi_holder.append(&gtk::Button::with_label("unexpected second root"));
        let multi_leaf_widget = multi_leaf.clone().upcast::<gtk::Widget>();
        assert!(detach_leaf_and_promote(&multi_holder, &multi_leaf_widget).is_none());
        assert_eq!(
            multi_parent.start_child().as_ref(),
            Some(&multi_leaf_widget)
        );

        let valid_holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let valid_root = gtk::Paned::new(gtk::Orientation::Horizontal);
        let valid_parent = gtk::Paned::new(gtk::Orientation::Vertical);
        let valid_leaf = gtk::Button::with_label("valid leaf");
        let valid_sibling = gtk::Button::with_label("valid sibling");
        let valid_sibling_widget = valid_sibling.clone().upcast::<gtk::Widget>();
        valid_parent.set_start_child(Some(&valid_leaf));
        valid_parent.set_end_child(Some(&valid_sibling));
        valid_root.set_start_child(Some(&valid_parent));
        valid_root.set_end_child(Some(&gtk::Button::with_label("valid root sibling")));
        valid_holder.append(&valid_root);
        assert_eq!(
            detach_leaf_and_promote(&valid_holder, &valid_leaf.upcast()),
            Some(valid_sibling_widget.clone())
        );
        assert_eq!(
            valid_root.start_child().as_ref(),
            Some(&valid_sibling_widget)
        );
    }

    #[test]
    fn pane_promotion_plan_uses_stable_source_and_optional_anchor_ids() {
        let tabs = vec![
            drop_tab(10, &[100, 101], false, Some(101)),
            drop_tab(20, &[200], false, None),
        ];
        assert_eq!(
            plan_pane_into_tab(&tabs, 101, Some(20), false),
            Some(PaneIntoTabPlan {
                source_tab_id: 10,
                pane_id: 101,
                anchor_tab_id: Some(20),
                after: false,
                moves_remote: true,
            })
        );
        assert_eq!(
            plan_pane_into_tab(&tabs, 100, None, true),
            Some(PaneIntoTabPlan {
                source_tab_id: 10,
                pane_id: 100,
                anchor_tab_id: None,
                after: true,
                moves_remote: false,
            })
        );
    }

    #[test]
    fn pane_promotion_rejects_ordinary_zoomed_missing_and_own_tab_drops() {
        let tabs = vec![
            drop_tab(10, &[100, 101], false, None),
            drop_tab(20, &[200], false, None),
        ];
        assert!(plan_pane_into_tab(&tabs, 200, None, true).is_none());
        assert!(plan_pane_into_tab(&tabs, 999, None, true).is_none());
        assert!(plan_pane_into_tab(&tabs, 100, Some(10), true).is_none());
        assert!(plan_pane_into_tab(&tabs, 100, Some(999), true).is_none());

        let zoomed = vec![
            drop_tab(10, &[100, 101], true, None),
            drop_tab(20, &[200], false, None),
        ];
        assert!(plan_pane_into_tab(&zoomed, 100, Some(20), true).is_none());
    }

    #[test]
    fn home_is_abbreviated_only_at_a_component_boundary() {
        assert_eq!(abbreviate_prefix("/home/user", "/home/user"), "~");
        assert_eq!(abbreviate_prefix("/home/user/src", "/home/user"), "~/src");
        // A sibling directory that merely shares the prefix must stay intact.
        assert_eq!(
            abbreviate_prefix("/home/user2/src", "/home/user"),
            "/home/user2/src"
        );
        assert_eq!(abbreviate_prefix("/etc", "/home/user"), "/etc");
        assert_eq!(abbreviate_prefix("/etc", ""), "/etc");
    }

    #[test]
    fn pane_header_title_prefers_osc_then_directory_then_position() {
        assert_eq!(
            pane_header_title(Some("vim README"), Some("/tmp"), 0),
            "vim README"
        );
        // Whitespace-only OSC titles must not blank the header.
        assert_eq!(pane_header_title(Some("   "), Some("/tmp/work"), 0), "work");
        assert_eq!(pane_header_title(None, Some("/tmp/work"), 0), "work");
        // A path with no last component keeps whatever it does have.
        assert_eq!(pane_header_title(None, Some("/"), 0), "/");
        assert_eq!(pane_header_title(None, None, 2), "Pane 3");
        assert_eq!(pane_header_title(Some(""), None, 0), "Pane 1");
    }

    #[test]
    fn restored_splits_use_the_configured_backend_for_every_leaf() {
        for _ in 0..3 {
            assert!(matches!(
                restored_leaf_mode(TerminalMode::Block, false),
                TerminalMode::Block
            ));
            assert!(matches!(
                restored_leaf_mode(TerminalMode::Vte, false),
                TerminalMode::Vte
            ));
            assert!(matches!(
                restored_leaf_mode(TerminalMode::Unified, false),
                TerminalMode::Unified
            ));
        }
    }

    #[test]
    fn remote_restore_keeps_block_mode_and_ignores_remote_cwd_namespace() {
        let ssh = vec!["/usr/bin/ssh".to_string(), "example.test".to_string()];
        let nix = vec!["nix".to_string(), "develop".to_string()];
        assert!(crate::process::command_uses_external_cwd(&ssh));
        assert!(!crate::process::command_uses_external_cwd(&nix));
        assert!(matches!(
            restored_leaf_mode(TerminalMode::Vte, true),
            TerminalMode::Block
        ));
        assert!(matches!(
            restored_leaf_mode(TerminalMode::Unified, true),
            TerminalMode::Block
        ));
    }

    #[test]
    fn managed_restore_never_reactivates_profile_129() {
        let host = |name: String| crate::config::RemoteHost {
            name,
            host: "example.test".to_string(),
            user: None,
            docker: false,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            ssh_args: Vec::new(),
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Off,
        };
        let mut hosts: Vec<_> = (0..crate::config::MAX_REMOTE_HOSTS)
            .map(|index| host(format!("active-{index}")))
            .collect();
        hosts.push(host("inactive-129".to_string()));

        assert!(super::managed_remote_host_for_restore(&hosts, "inactive-129").is_none());
    }

    #[test]
    fn managed_remote_snapshots_store_only_the_profile_identifier() {
        let stale = vec!["ssh".to_string(), "old.example".to_string()];
        assert_eq!(snapshot_restorable_command(true, Some(stale.clone())), None);
        assert_eq!(
            snapshot_restorable_command(false, Some(stale.clone())),
            Some(stale)
        );
    }

    #[test]
    fn removed_managed_remote_never_replays_legacy_snapshot_argv() {
        let stale = vec!["ssh".to_string(), "removed.example".to_string()];
        assert_eq!(
            replay_argv_for_unmanaged_leaf(Some("removed"), Some(&stale)),
            None
        );
        assert_eq!(
            replay_argv_for_unmanaged_leaf(None, Some(&stale)),
            Some(stale.as_slice())
        );
    }

    #[test]
    fn remote_reconnect_requires_the_same_only_unzoomed_pane() {
        assert!(reconnect_target_is_valid(1, false, Some(7), 7));
        assert!(!reconnect_target_is_valid(2, false, Some(7), 7));
        assert!(!reconnect_target_is_valid(1, true, Some(7), 7));
        assert!(!reconnect_target_is_valid(1, false, Some(8), 7));
        assert!(!reconnect_target_is_valid(1, false, None, 7));
    }

    #[test]
    fn active_index_tracks_the_same_pane_when_an_earlier_pane_is_removed() {
        assert_eq!(active_index_after_remove(2, 0, 2), 1);
        assert_eq!(active_index_after_remove(1, 0, 2), 0);
    }

    #[test]
    fn removing_the_active_pane_prefers_the_next_then_previous_pane() {
        assert_eq!(active_index_after_remove(1, 1, 2), 1);
        assert_eq!(active_index_after_remove(2, 2, 2), 1);
    }

    #[test]
    fn removing_a_later_pane_keeps_the_active_index() {
        assert_eq!(active_index_after_remove(0, 2, 2), 0);
    }

    #[test]
    fn running_process_summary_is_empty_or_bounded_without_losing_count() {
        assert_eq!(format_running_process_summary(Vec::new()), None);
        let summary = format_running_process_summary(
            (1..=10).map(|index| format!("tab {index} — vim")).collect(),
        )
        .unwrap();
        assert!(summary.contains("tab 1 — vim"));
        assert!(!summary.contains("tab 9 — vim"));
        assert!(summary.ends_with("…and 2 more"));
    }
}
