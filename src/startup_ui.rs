//! Reusable GTK construction helpers for the Relm4 application bootstrap.
//!
//! `Component::init` remains the sole owner of every widget and controller. These
//! helpers only construct recurring GTK groups and register CSS providers.

use super::*;

/// Translate a tab row's output into an app message. Shared by the strip and
/// by the sidebar's mirror list so both behave identically.
pub(crate) fn tab_row_output_to_msg(output: tab_strip::TabRowOutput) -> AppMsg {
    match output {
        tab_strip::TabRowOutput::Select(id) => AppMsg::SelectTab(id),
        tab_strip::TabRowOutput::Close(id) => AppMsg::CloseTab(id),
        tab_strip::TabRowOutput::Rename(id, title) => AppMsg::RenameTab(id, title),
        tab_strip::TabRowOutput::NewTab => AppMsg::NewTab,
        tab_strip::TabRowOutput::Action(id, action) => AppMsg::TabRowAction(id, action),
        tab_strip::TabRowOutput::ConnectRemote(index) => {
            AppMsg::Action(Action::ConnectRemote(index))
        }
        tab_strip::TabRowOutput::Resize(width) => AppMsg::SetTabWidth(width),
        tab_strip::TabRowOutput::Reorder { source_id, target } => {
            AppMsg::ReorderTab(source_id, target)
        }
        tab_strip::TabRowOutput::DragStarted {
            source_tab_id,
            drag_id,
        } => AppMsg::TabDragStarted {
            source_tab_id,
            drag_id,
        },
        tab_strip::TabRowOutput::DragEnded {
            source_tab_id,
            drag_id,
        } => AppMsg::TabDragEnded {
            source_tab_id,
            drag_id,
        },
        tab_strip::TabRowOutput::PreviewDropTarget {
            source_tab_id,
            target_tab_id,
            drag_id,
            hover_generation,
        } => AppMsg::PreviewTabDrop {
            source_tab_id,
            target_tab_id,
            drag_id,
            hover_generation,
        },
        tab_strip::TabRowOutput::PromotePane {
            pane_id,
            anchor_tab_id,
            after,
        } => AppMsg::PromotePaneToTab {
            pane_id,
            anchor_tab_id: Some(anchor_tab_id),
            after,
        },
    }
}

#[allow(deprecated)]
pub(crate) fn install_static_css() {
    // Colors are theme work (`apply_dynamic_css`); only the bar's shape and
    // canonical height come from the family contract.
    let bar_height = jterm_core::bottom_bar::BAR_HEIGHT as i32;
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&format!(
        "{}{}
         .bottom-bar {{ min-height: {bar_height}px; padding: 0 8px; font-size: 0.85em; border-top: 1px solid rgba(127,127,127,0.4); }}",
        crate::pane_header::PANE_HEADER_CSS,
        ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; border-bottom: 1px solid alpha(currentColor, 0.1); margin-bottom: 2px; color: #ffffff; }
         .tab-strip-btn:checked { font-weight: bold; border-radius: 4px; background-color: alpha(currentColor, 0.14); outline: 2px solid alpha(currentColor, 0.8); outline-offset: -2px; }
         .tab-strip-close { min-width: 16px; min-height: 16px; padding: 0; margin: 0; color: #ffffff; }
         .tab-resize-handle { min-width: 8px; margin-left: 2px; border-left: 1px solid alpha(currentColor, 0.24); }
         .tab-resize-handle:hover { border-left-color: currentColor; }
         .tab-strip { min-width: 140px; padding: 2px 4px; }
         .file-tree { padding: 2px; }
         .sidebar-toggle { color: #ffffff; }
         .top-bar { padding: 2px 4px; }
         .window-controls { margin-left: 2px; }
         .window-control { min-width: 24px; min-height: 24px; padding: 0; border-radius: 999px; }
         .terminal-box scrollbar slider { min-width: 6px; border-radius: 3px; }
         .terminal-box scrollbar { padding: 0; }
         .tab-activity { font-style: italic; }
         .tab-bell { color: #f1fa8c; }
         .tab-marked { background-color: rgba(80,160,255,0.22); font-weight: bold; }
         .tab-pinned { background-color: rgba(255,200,80,0.18); }
         .conn-dot { margin: 0 4px; font-size: 9px; }
         .conn-connecting { color: #f1fa8c; }
         .conn-connected { color: #50fa7b; }
         .conn-disconnected { color: #ff5555; }
         .top-tabs { } .top-tabs .tab-row { margin-right: 2px; }
         .top-tabs .tab-strip-btn { border-bottom: none; margin-bottom: 0; }
         .top-tab-scroll, .top-tab-scroll > viewport { min-width: 0; }
         .sidebar-toggle-row { margin-bottom: 2px; }
         .sidebar-toggle { padding: 2px 6px; }",
    ));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub(crate) fn install_dynamic_css_provider() -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    }
    provider
}

#[allow(deprecated)]
pub(crate) struct FileTreeUi {
    pub(crate) store: gtk::TreeStore,
    pub(crate) scroll: gtk::ScrolledWindow,
    pub(crate) header: Controller<sidebar::FileHeaderModel>,
    pub(crate) scan_generation: Rc<std::cell::Cell<u64>>,
}

#[allow(deprecated)]
pub(crate) fn build_file_tree(
    sender: &ComponentSender<AppModel>,
    config: &Rc<RefCell<Config>>,
    location: &Rc<RefCell<remote_fs::FsLocation>>,
    clipboard: &Rc<RefCell<Option<remote_fs::FsClipboard>>>,
) -> FileTreeUi {
    let store = file_tree::new_store();
    let view = file_tree::new_view(&store);
    let scan_generation = Rc::new(std::cell::Cell::new(0));
    view.add_css_class("file-tree");

    {
        let store = store.clone();
        let scan_generation = scan_generation.clone();
        let config = config.clone();
        let location = location.clone();
        view.connect_row_expanded(move |_view, iter, _path| {
            let hosts = config.borrow().remote_hosts.clone();
            file_tree::on_expand(&store, iter, &scan_generation, &location, hosts);
        });
    }
    {
        let store = store.clone();
        let sender = sender.clone();
        view.connect_row_activated(move |view, path, _column| {
            let Some(iter) = store.iter(path) else { return };
            let is_dir: bool = store
                .get_value(&iter, file_tree::COL_IS_DIR as i32)
                .get()
                .unwrap_or(false);
            if is_dir {
                if view.row_expanded(path) {
                    view.collapse_row(path);
                } else {
                    view.expand_row(path, false);
                }
                return;
            }

            let path_identity: String = store
                .get_value(&iter, file_tree::COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if path_identity.is_empty() {
                return;
            }
            let Some(file_path) = file_tree::decode_path_identity(&path_identity) else {
                log::warn!("file-tree activation ignored an invalid path identity");
                return;
            };
            if file_tree::is_notebook_path(&file_path) {
                sender.input(AppMsg::OpenNotebook(file_path));
            } else {
                sender.input(AppMsg::FileTreeActivateFile(file_path));
            }
        });
    }
    {
        // Right-click context menu: file operations for the row under the
        // pointer, or for the tree root when the pointer is over empty space.
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let view_for_gesture = view.clone();
        let store_for_gesture = store.clone();
        let location = location.clone();
        let clipboard = clipboard.clone();
        let sender = sender.clone();
        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_file_tree_context_menu(
                &view_for_gesture,
                &store_for_gesture,
                x,
                y,
                &location,
                &clipboard,
                &sender,
            );
        });
        view.add_controller(gesture);
    }

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&view));
    let labels = remote_fs::location_labels(&config.borrow().remote_hosts);
    let header = sidebar::FileHeaderModel::builder().launch(labels).forward(
        sender.input_sender(),
        |output| match output {
            sidebar::FileHeaderOutput::Up => AppMsg::FileTreeGoUp,
            sidebar::FileHeaderOutput::CurrentDirectory => AppMsg::FileTreeGotoCwd,
            sidebar::FileHeaderOutput::SelectLocation(index) => {
                AppMsg::FileTreeSelectLocation(index)
            }
        },
    );

    FileTreeUi {
        store,
        scroll,
        header,
        scan_generation,
    }
}

/// One menu row, styled like the tab strip's context-menu buttons.
fn file_menu_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_has_frame(false);
    button.add_css_class("flat");
    if let Some(child) = button.child() {
        child.set_halign(gtk::Align::Start);
    }
    button
}

fn add_file_menu_item(
    menu: &gtk::Box,
    label: &str,
    popover: &gtk::Popover,
    sender: &ComponentSender<AppModel>,
    msg: AppMsg,
) {
    let button = file_menu_button(label);
    let popover = popover.clone();
    let sender = sender.clone();
    button.connect_clicked(move |_| {
        popover.popdown();
        sender.input(msg.clone());
    });
    menu.append(&button);
}

#[allow(deprecated)]
fn show_file_tree_context_menu(
    view: &gtk::TreeView,
    store: &gtk::TreeStore,
    x: f64,
    y: f64,
    location: &Rc<RefCell<remote_fs::FsLocation>>,
    clipboard: &Rc<RefCell<Option<remote_fs::FsClipboard>>>,
    sender: &ComponentSender<AppModel>,
) {
    // Resolve the row under the pointer to (path, is_dir); rows without a
    // valid identity (placeholders) behave like empty space.
    let row = view
        .path_at_pos(x as i32, y as i32)
        .and_then(|(path, _column, _x, _y)| path)
        .and_then(|path| store.iter(&path))
        .and_then(|iter| {
            let identity: String = store
                .get_value(&iter, file_tree::COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if identity.is_empty() {
                return None;
            }
            let path = file_tree::decode_path_identity(&identity)?;
            let is_dir: bool = store
                .get_value(&iter, file_tree::COL_IS_DIR as i32)
                .get()
                .unwrap_or(false);
            Some((path, is_dir))
        });
    // New entries and pastes land in the clicked directory, next to the
    // clicked file, or at the current root (handled message-side via None).
    let target_dir = match &row {
        Some((path, true)) => Some(path.clone()),
        Some((path, false)) => path.parent().map(std::path::Path::to_path_buf),
        None => None,
    };

    let loc = location.borrow().clone();
    let clip = clipboard.borrow().clone();
    // Cross-location paste streams between the two filesystems; the label
    // says which direction the bytes will flow.
    let (paste_sensitive, paste_label, paste_tooltip) = match &clip {
        None => (
            false,
            "Paste".to_string(),
            Some("Copy or cut an item first"),
        ),
        Some(clip) if clip.loc == loc => (true, "Paste".to_string(), None),
        Some(clip) => match (&clip.loc, &loc) {
            (remote_fs::FsLocation::Remote(_), remote_fs::FsLocation::Local) => {
                (true, "Paste (download)".to_string(), None)
            }
            (remote_fs::FsLocation::Local, remote_fs::FsLocation::Remote(_)) => {
                (true, "Paste (upload)".to_string(), None)
            }
            _ => (
                true,
                "Paste (via local relay)".to_string(),
                Some("Downloaded to a local staging file, then uploaded"),
            ),
        },
    };

    let popover = gtk::Popover::new();
    popover.set_parent(view);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu.add_css_class("menu");

    add_file_menu_item(
        &menu,
        "New File",
        &popover,
        sender,
        AppMsg::FileTreeNewFile {
            dir: target_dir.clone(),
        },
    );
    add_file_menu_item(
        &menu,
        "New Folder",
        &popover,
        sender,
        AppMsg::FileTreeNewFolder {
            dir: target_dir.clone(),
        },
    );
    if let Some((path, is_dir)) = &row {
        add_file_menu_item(
            &menu,
            "Rename",
            &popover,
            sender,
            AppMsg::FileTreeRename { path: path.clone() },
        );
        add_file_menu_item(
            &menu,
            "Delete",
            &popover,
            sender,
            AppMsg::FileTreeDelete { path: path.clone() },
        );
        add_file_menu_item(
            &menu,
            "Copy",
            &popover,
            sender,
            AppMsg::FileTreeCopy {
                path: path.clone(),
                is_dir: *is_dir,
            },
        );
        add_file_menu_item(
            &menu,
            "Cut",
            &popover,
            sender,
            AppMsg::FileTreeCut {
                path: path.clone(),
                is_dir: *is_dir,
            },
        );
        {
            // Remote rows copy the plain remote path (no prefix): that is
            // what users paste into the remote shell.
            let button = file_menu_button("Copy Path");
            let popover = popover.clone();
            let sender = sender.clone();
            let payload = file_tree::copy_path_payload(path);
            button.connect_clicked(move |_| {
                popover.popdown();
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&payload);
                    sender.input(AppMsg::Toast("Path copied to clipboard.".to_string()));
                }
            });
            menu.append(&button);
        }
    }
    {
        let button = file_menu_button(&paste_label);
        button.set_sensitive(paste_sensitive);
        if let Some(tooltip) = paste_tooltip {
            button.set_tooltip_text(Some(tooltip));
        }
        let popover = popover.clone();
        let sender = sender.clone();
        button.connect_clicked(move |_| {
            popover.popdown();
            sender.input(AppMsg::FileTreePaste {
                dir: target_dir.clone(),
            });
        });
        menu.append(&button);
    }
    add_file_menu_item(&menu, "Refresh", &popover, sender, AppMsg::FileTreeRefresh);

    popover.set_child(Some(&menu));
    popover.connect_closed(|popover| popover.unparent());
    popover.popup();
}

pub(crate) fn build_tab_scrolls() -> (gtk::ScrolledWindow, gtk::ScrolledWindow) {
    let sidebar = gtk::ScrolledWindow::new();
    sidebar.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sidebar.set_vexpand(true);

    let top = gtk::ScrolledWindow::new();
    top.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
    top.set_hexpand(true);
    top.set_vexpand(false);
    top.set_overflow(gtk::Overflow::Hidden);
    top.set_width_request(0);
    top.set_min_content_width(0);
    top.set_max_content_width(1);
    top.set_propagate_natural_width(false);
    top.add_css_class("top-tab-scroll");
    top.set_visible(false);
    top.set_margin_start(128);
    top.set_margin_end(104);

    (sidebar, top)
}
