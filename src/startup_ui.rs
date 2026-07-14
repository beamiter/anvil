//! Reusable GTK construction helpers for the Relm4 application bootstrap.
//!
//! `Component::init` remains the sole owner of every widget and controller. These
//! helpers only construct recurring GTK groups and register CSS providers.

use super::*;

#[allow(deprecated)]
pub(crate) fn install_static_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; margin-bottom: 2px; color: #ffffff; }
         .tab-strip-btn:checked { font-weight: bold; border: 1px solid currentColor; border-radius: 4px; }
         .tab-strip-close { min-width: 16px; min-height: 16px; color: #ffffff; }
         .tab-resize-handle { min-width: 6px; margin: 0 1px; border-left: 1px solid rgba(255,255,255,0.38); }
         .tab-resize-handle:hover { border-left-color: rgba(255,255,255,0.9); }
         .tab-strip { min-width: 140px; padding: 2px 4px; }
         .file-tree { padding: 2px; }
         .sidebar-toggle { color: #ffffff; }
         .top-bar { padding: 2px 4px; }
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
         .top-tab-scroll, .top-tab-scroll > viewport { min-width: 0; }
         .sidebar-toggle-row { margin-bottom: 2px; }
         .sidebar-toggle { padding: 2px 6px; }",
    );
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
pub(crate) fn build_file_tree(sender: &ComponentSender<AppModel>) -> FileTreeUi {
    let store = file_tree::new_store();
    let view = file_tree::new_view(&store);
    let scan_generation = Rc::new(std::cell::Cell::new(0));
    view.add_css_class("file-tree");

    {
        let store = store.clone();
        let scan_generation = scan_generation.clone();
        view.connect_row_expanded(move |_view, iter, _path| {
            file_tree::on_expand(&store, iter, &scan_generation);
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

            let file_path: String = store
                .get_value(&iter, file_tree::COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if file_path.is_empty() {
                return;
            }
            if file_path.ends_with(".jtnb.md") {
                sender.input(AppMsg::OpenNotebook(std::path::PathBuf::from(file_path)));
            } else {
                sender.input(AppMsg::FileTreeActivateFile(file_path));
            }
        });
    }

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&view));
    let header =
        sidebar::FileHeaderModel::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                sidebar::FileHeaderOutput::Up => AppMsg::FileTreeGoUp,
                sidebar::FileHeaderOutput::CurrentDirectory => AppMsg::FileTreeGotoCwd,
            });

    FileTreeUi {
        store,
        scroll,
        header,
        scan_generation,
    }
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
