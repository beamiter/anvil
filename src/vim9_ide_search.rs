//! Native project-search options. Reading controls never launches a search;
//! Enter or the Search button sends one immutable set of options to Vim.

use super::*;
use jterm_core::vim9::{Vim9Search, Vim9SearchCase, Vim9SearchOptions};

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

#[cfg(test)]
mod tests {
    use super::*;

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
