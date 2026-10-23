//! Native project-search options and Problems feedback. Reading controls never launches a search;
//! Enter or the Search button sends one immutable set of options to Vim.

use super::*;
use jterm_core::vim9::{
    Vim9Diagnostic, Vim9Search, Vim9SearchCase, Vim9SearchOptions, Vim9Snapshot,
};

#[derive(Clone)]
pub(super) struct SearchOptions {
    pub(super) menu: gtk::MenuButton,
    case: gtk::DropDown,
    regex: gtk::CheckButton,
    whole_word: gtk::CheckButton,
    glob: gtk::Entry,
}

impl SearchOptions {
    pub(super) fn new() -> Self {
        let menu = gtk::MenuButton::new();
        menu.set_icon_name("preferences-other-symbolic");
        menu.set_tooltip_text(Some("Project search options"));
        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        let heading = label("PROJECT SEARCH");
        heading.add_css_class("heading");
        content.append(&heading);
        let case = gtk::DropDown::from_strings(&["Smart case", "Match case", "Ignore case"]);
        case.set_tooltip_text(Some(
            "Smart case ignores case unless the query contains an uppercase letter",
        ));
        content.append(&case);
        let regex = gtk::CheckButton::with_label("Regular expression");
        regex.set_tooltip_text(Some("Interpret the query as a ripgrep regular expression"));
        content.append(&regex);
        let whole_word = gtk::CheckButton::with_label("Match whole words");
        content.append(&whole_word);
        let glob_label = label("Files to include or exclude");
        content.append(&glob_label);
        let glob = gtk::Entry::new();
        glob.set_placeholder_text(Some("File pattern, e.g. *.rs or !*.lock"));
        glob.set_tooltip_text(Some(
            "One ripgrep glob. Spaces and commas belong to the pattern; leave empty for all searchable files.",
        ));
        content.append(&glob);
        let note = label("Options apply to the next explicit search.");
        note.add_css_class("caption");
        note.set_wrap(true);
        content.append(&note);
        popover.set_child(Some(&content));
        menu.set_popover(Some(&popover));
        Self {
            menu,
            case,
            regex,
            whole_word,
            glob,
        }
    }

    pub(super) fn restore(&self, options: &Vim9SearchOptions) {
        self.case.set_selected(match options.case {
            Vim9SearchCase::Sensitive => 1,
            Vim9SearchCase::Ignore => 2,
            Vim9SearchCase::Smart => 0,
        });
        self.regex.set_active(options.regex);
        self.whole_word.set_active(options.whole_word);
        self.glob.set_text(&options.glob);
    }

    pub(super) fn options(&self) -> Vim9SearchOptions {
        Vim9SearchOptions {
            case: match self.case.selected() {
                1 => Vim9SearchCase::Sensitive,
                2 => Vim9SearchCase::Ignore,
                _ => Vim9SearchCase::Smart,
            },
            regex: self.regex.is_active(),
            whole_word: self.whole_word.is_active(),
            glob: self.glob.text().into(),
        }
    }
}

fn options_summary(options: &Vim9SearchOptions) -> String {
    let mut parts = vec![
        if options.regex { "regex" } else { "literal" }.to_owned(),
        match options.case {
            Vim9SearchCase::Smart => "smart case",
            Vim9SearchCase::Sensitive => "match case",
            Vim9SearchCase::Ignore => "ignore case",
        }
        .to_owned(),
    ];
    if options.whole_word {
        parts.push("whole words".into());
    }
    if !options.glob.is_empty() {
        parts.push(format!("files: {}", visible(&options.glob)));
    }
    parts.join(" · ")
}

/// Describe the options attached to the job, rather than controls the user
/// may already have changed in preparation for their next search.
pub(super) fn feedback(search: &Vim9Search) -> String {
    if search.query.is_empty() {
        return String::new();
    }
    let details = format!(
        "{} · {}",
        visible(&search.query),
        options_summary(&search.options)
    );
    if search.running {
        format!("Searching: {details}…")
    } else if search.cancelled {
        format!("Search cancelled: {details}")
    } else if !search.errors.is_empty() || search.exit_code < 0 || search.exit_code > 1 {
        format!(
            "Search failed ({}): {} · {}",
            search.exit_code,
            visible(&search.errors.join(" · ")),
            details
        )
    } else {
        format!(
            "Search: {} {}{} for {}",
            search.count,
            if search.count == 1 {
                "match"
            } else {
                "matches"
            },
            if search.truncated > 0 {
                format!(" · {} omitted", search.truncated)
            } else {
                String::new()
            },
            details
        )
    }
}

fn diagnostic_kind(kind: &str) -> (&'static str, &'static str) {
    match kind.to_ascii_lowercase().as_str() {
        "e" | "error" => ("E · Error", "error"),
        "w" | "warning" => ("W · Warning", "warning"),
        "i" | "info" | "information" => ("I · Info", "accent"),
        "h" | "hint" => ("H · Hint", "dim-label"),
        _ => ("• · Problem", "dim-label"),
    }
}

fn diagnostic_path<'a>(root: &str, filename: &'a str) -> &'a str {
    use std::path::Component;
    let file = Path::new(filename);
    let root = Path::new(root);
    if file.is_absolute()
        && root.is_absolute()
        && !file.components().any(|part| part == Component::ParentDir)
        && !root.components().any(|part| part == Component::ParentDir)
    {
        if let Ok(relative) = file.strip_prefix(root) {
            if let Some(relative) = relative.to_str().filter(|name| !name.is_empty()) {
                return relative;
            }
        }
    }
    filename
}

pub(super) fn diagnostic_row(root: &str, diagnostic: &Vim9Diagnostic) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
    row.set_margin_start(8);
    row.set_margin_end(8);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let (kind, class) = diagnostic_kind(&diagnostic.kind);
    let badge = label(kind);
    badge.add_css_class(class);
    heading.append(&badge);
    let path = diagnostic_path(root, &diagnostic.filename);
    let location = label(&format!(
        "{}:{}:{}",
        if path.is_empty() {
            "[No file]".into()
        } else {
            visible(path)
        },
        diagnostic.line,
        diagnostic.column,
    ));
    location.set_hexpand(true);
    location.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    location.add_css_class("dim-label");
    heading.append(&location);
    row.append(&heading);
    let message = label(&visible(&diagnostic.text));
    message.set_wrap(true);
    message.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    message.set_lines(2);
    message.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&message);
    row.set_tooltip_text(Some(&jterm_core::review_input::safe_multiline_display(
        &format!(
            "{kind}\n{}:{}:{}\n{}",
            diagnostic.filename, diagnostic.line, diagnostic.column, diagnostic.text
        ),
        32 * 1024,
    )));
    row
}

/// A zero-result search is distinct from the merged quickfix/location/LSP
/// diagnostics, and from a failed or cancelled job retaining an older list.
pub(super) fn empty_results(snapshot: &Vim9Snapshot) -> Option<String> {
    let search = &snapshot.search;
    (snapshot.quickfix.source == "search"
        && snapshot.quickfix.title == format!("Search: {}", search.query)
        && snapshot.quickfix.count == 0
        && snapshot.diagnostics.is_empty()
        && !search.query.is_empty()
        && !search.running
        && !search.cancelled
        && search.exit_code == 1
        && search.errors.is_empty()
        && search.count == 0)
        .then(|| {
            format!(
                "No project matches for “{}”. Change the query or search options and search again.",
                visible(&search.query)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_paths_only_relativize_a_valid_project_descendant() {
        assert_eq!(
            diagnostic_path("/project", "/project/src/main.rs"),
            "src/main.rs"
        );
        for path in [
            "/project-other/src/main.rs",
            "/project/../outside.rs",
            "/outside.rs",
            "/project",
            "src/relative.rs",
        ] {
            assert_eq!(diagnostic_path("/project", path), path);
        }
        assert_eq!(
            diagnostic_path("project", "/project/src/main.rs"),
            "/project/src/main.rs"
        );
        assert_eq!(
            diagnostic_path("/project/../outside", "/project/src/main.rs"),
            "/project/src/main.rs"
        );
    }

    #[test]
    fn empty_search_feedback_requires_the_same_successful_owned_list() {
        let mut snapshot = Vim9Snapshot::default();
        snapshot.search.query = "needle".into();
        snapshot.search.exit_code = 1;
        snapshot.quickfix.source = "search".into();
        snapshot.quickfix.title = "Search: needle".into();
        assert!(empty_results(&snapshot).is_some());
        snapshot.quickfix.title = "Search: older query".into();
        assert!(empty_results(&snapshot).is_none());
        snapshot.quickfix.title = "Search: needle".into();
        snapshot.quickfix.source = "build".into();
        assert!(empty_results(&snapshot).is_none());
        snapshot.quickfix.source = "search".into();
        snapshot.diagnostics.push(Vim9Diagnostic::default());
        assert!(
            empty_results(&snapshot).is_none(),
            "location/LSP diagnostics are not an empty Problems list"
        );
        snapshot.diagnostics.clear();
        snapshot.search.cancelled = true;
        assert!(empty_results(&snapshot).is_none());
        snapshot.search.cancelled = false;
        snapshot.search.running = true;
        assert!(empty_results(&snapshot).is_none());
        snapshot.search.running = false;
        snapshot.search.exit_code = 2;
        assert!(empty_results(&snapshot).is_none());
    }

    #[test]
    fn search_feedback_distinguishes_cancel_failure_and_no_matches() {
        let mut search = Vim9Search {
            query: "needle".into(),
            options: Vim9SearchOptions {
                regex: true,
                whole_word: true,
                glob: "src/a b/*.rs".into(),
                ..Default::default()
            },
            exit_code: 1,
            ..Default::default()
        };
        assert!(feedback(&search).contains("0 matches"));
        assert!(
            feedback(&search).contains("regex · smart case · whole words · files: src/a b/*.rs")
        );
        search.cancelled = true;
        search.exit_code = -1;
        assert!(feedback(&search).starts_with("Search cancelled:"));
        search.cancelled = false;
        search.exit_code = 2;
        search.errors = vec!["invalid pattern".into()];
        assert!(feedback(&search).starts_with("Search failed (2): invalid pattern"));
    }
}
