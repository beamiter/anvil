from pathlib import Path
import re

main_path = Path("src/main.rs")
text = main_path.read_text()

if "mod ai_palette_ops;\n" not in text:
    text = text.replace("mod ai;\n", "mod ai;\nmod ai_palette_ops;\n", 1)
if "mod startup_ui;\n" not in text:
    text = text.replace("mod settings_ops;\n", "mod settings_ops;\nmod startup_ui;\n", 1)

strip_start = text.find("/// Strip one layer of markdown code fence")
create_pane_start = text.find("#[allow(clippy::too_many_arguments)]\nfn create_pane", strip_start)
ai_start = text.find("    /// `?` palette accept handler")
impl_end_marker = "\n}\n\n#[allow(deprecated)]\nfn install_static_css"
impl_end = text.find(impl_end_marker, ai_start)
static_start = text.find("#[allow(deprecated)]\nfn install_static_css", impl_end)
component_start = text.find("\n\n#[relm4::component]", static_start)

if min(strip_start, create_pane_start, ai_start, impl_end, static_start, component_start) < 0:
    raise SystemExit("round 10 extraction markers not found")

strip_block = text[strip_start:create_pane_start]
ai_block = text[ai_start:impl_end]
static_block = text[static_start:component_start]

text = text.replace(strip_block, "", 1)
text = text.replace(ai_block, "", 1)
text = text.replace(static_block, "", 1)

css_start = text.find("        install_static_css();")
stack_start = text.find("        let stack = gtk::Stack::new();", css_start)
if min(css_start, stack_start) < 0:
    raise SystemExit("CSS bootstrap block not found")
text = (
    text[:css_start]
    + "        startup_ui::install_static_css();\n"
    + "        let dyn_css = startup_ui::install_dynamic_css_provider();\n\n"
    + text[stack_start:]
)

file_tree_start = text.find("        // File tree browser (lower half of the sidebar).")
sidebar_width_start = text.find("        let sidebar_width =", file_tree_start)
if min(file_tree_start, sidebar_width_start) < 0:
    raise SystemExit("file tree bootstrap block not found")
text = (
    text[:file_tree_start]
    + "        let startup_ui::FileTreeUi {\n"
    + "            store: file_tree_store,\n"
    + "            scroll: file_tree_scroll,\n"
    + "            header: file_header,\n"
    + "        } = startup_ui::build_file_tree(&sender);\n\n"
    + text[sidebar_width_start:]
)

scroll_start = text.find(
    "        // Scroll holders the tab strip can be reparented between (sidebar vs"
)
top_bar_start = text.find("        let top_bar =", scroll_start)
if min(scroll_start, top_bar_start) < 0:
    raise SystemExit("tab scroll bootstrap block not found")
text = (
    text[:scroll_start]
    + "        let (tab_strip_scroll, top_tab_scroll) = startup_ui::build_tab_scrolls();\n"
    + text[top_bar_start:]
)

main_path.write_text(text)

def expose_methods(block: str) -> str:
    return re.sub(r"^    fn ", "    pub(crate) fn ", block, flags=re.MULTILINE)

ai_module = '''//! Natural-language command palette and session-level AI panel operations.
//!
//! These are inherent methods on the existing Relm4 `AppModel`; AI requests still
//! return through the same `AppMsg` input channel and active terminal controller.

use super::*;

''' + strip_block + '''impl AppModel {
''' + expose_methods(ai_block) + "}\n"
Path("src/ai_palette_ops.rs").write_text(ai_module)

static_block = static_block.replace(
    "fn install_static_css()", "pub(crate) fn install_static_css()", 1
)
startup_module = '''//! Reusable GTK construction helpers for the Relm4 application bootstrap.
//!
//! `Component::init` remains the sole owner of every widget and controller. These
//! helpers only construct recurring GTK groups and register CSS providers.

use super::*;

''' + static_block + r'''

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
}

#[allow(deprecated)]
pub(crate) fn build_file_tree(sender: &ComponentSender<AppModel>) -> FileTreeUi {
    let store = file_tree::new_store();
    let view = file_tree::new_view(&store);
    view.add_css_class("file-tree");

    {
        let store = store.clone();
        view.connect_row_expanded(move |_view, iter, _path| {
            file_tree::on_expand(&store, iter);
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
    let header = sidebar::FileHeaderModel::builder().launch(()).forward(
        sender.input_sender(),
        |output| match output {
            sidebar::FileHeaderOutput::Up => AppMsg::FileTreeGoUp,
            sidebar::FileHeaderOutput::CurrentDirectory => AppMsg::FileTreeGotoCwd,
        },
    );

    FileTreeUi {
        store,
        scroll,
        header,
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
'''
Path("src/startup_ui.rs").write_text(startup_module)
