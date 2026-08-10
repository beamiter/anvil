//! find — extracted from block_view (mechanical split, no logic changes)
//!
//! Find-within-blocks: VTE's native PCRE2 highlighter paints every hit inside
//! each finished block's command/output VTE and the current command's live VTE;
//! we only track which surface each hit belongs to so Next/Prev can step the
//! per-VTE search cursor across block boundaries. Also hosts the metadata-only
//! filter pass used by the command palette's failed/slow toggles and by the
//! debug dashboard counts.

use gtk::glib;
use gtk::prelude::*;
use relm4::gtk;
use vte4::TerminalExt;

use super::{contains_case_insensitive, replace_finished_block_selection, BlockFilters, TermView};

/// One hit from a find-within-blocks pass. With VTE-backed blocks the match
/// position lives inside the VTE itself (highlighted automatically by
/// `search_set_regex`); we only remember which (block, surface) it belongs
/// to so navigation can move the per-VTE search cursor to the right widget.
#[derive(Clone)]
pub(crate) struct FindMatch {
    pub(crate) block_id: u64,
    /// false = command VTE, true = output VTE.
    pub(crate) is_output: bool,
    /// The hit lives in the live VTE for the command that is still running,
    /// rather than in a finished block. `block_id` is unused in this case.
    pub(crate) is_live: bool,
}

#[derive(Default)]
pub(crate) struct FindState {
    pub(crate) matches: Vec<FindMatch>,
    /// Index into `matches` of the currently focused hit.
    pub(crate) current: usize,
}

/// One result row from a cross-block ripgrep-style scan. Carries enough
/// context for a flat result list — block id (for jump), surface flag (so
/// the per-block VTE search cursor goes to the right widget), the 1-based
/// line number inside that surface, the line snippet itself (trimmed/
/// truncated for display), and a one-line cmd preview for context.
#[derive(Clone, Debug)]
pub struct CrossBlockHit {
    pub block_id: u64,
    pub is_output: bool,
    pub line_no: usize,
    pub line_text: String,
    pub cmd_preview: String,
}

/// Trim a line to a reasonable display width — the palette row is one
/// horizontal line so an unbounded long line (think bundled JSON) would
/// just blow out the dialog width. We truncate with a leading ellipsis if
/// the match isn't near the start, but for the MVP we just hard-cap.
fn snippet(line: &str) -> String {
    const CAP: usize = 240;
    let mut chars = line.chars();
    let mut snippet: String = chars.by_ref().take(CAP).collect();
    if chars.next().is_some() {
        snippet.push('…');
    }
    snippet
}

/// Only states whose bytes belong to the current command may join the live
/// search surface. Prompt/editor text must not be counted as command output,
/// and alt-screen programs own their own interactive viewport.
fn live_output_is_searchable(state: super::BlockState) -> bool {
    matches!(
        state,
        super::BlockState::CollectingOutput | super::BlockState::PostCommand
    )
}

/// Duration-related filters are meaningful only for blocks whose shell
/// integration reported a duration. Older restored history can legitimately
/// lack that field; treating `None` as a match made a "slow blocks" jump land
/// on an unknown-duration command instead of an actually slow one.
/// Whether a block's status passes the exit-code and failed-only filters.
///
/// `None` is a status the shell never reported, so it satisfies neither: it is
/// not equal to any code the user filtered for, and it is not a failure this
/// terminal watched happen.
fn exit_status_matches(
    resolved_command: Option<&str>,
    reported_exit_code: Option<i32>,
    filters: &BlockFilters,
) -> bool {
    // BlockData owns anvil's already-resolved command (OSC metadata first,
    // bounded screen scrape second). Classify that value while the exit status
    // is still an Option, before legacy i32-only surfaces synthesize a zero.
    let outcome =
        jterm_core::block_contract::classify_completed(resolved_command, reported_exit_code);
    if let Some(wanted) = filters.exit_code {
        if outcome.reported_exit_code() != Some(wanted) {
            return false;
        }
    }
    if filters.failed_only && !outcome.is_failed() {
        return false;
    }
    true
}

fn duration_matches(duration: Option<u64>, filters: &BlockFilters) -> bool {
    let needs_duration =
        filters.min_duration_ms.is_some() || filters.max_duration_ms.is_some() || filters.slow_only;
    if !needs_duration {
        return true;
    }
    let Some(duration) = duration else {
        return false;
    };
    if let Some(min_dur) = filters.min_duration_ms {
        if duration < min_dur {
            return false;
        }
    }
    if let Some(max_dur) = filters.max_duration_ms {
        if duration > max_dur {
            return false;
        }
    }
    !filters.slow_only || duration >= filters.slow_threshold_ms
}

#[cfg(test)]
// Keeping focused helper tests beside the helpers makes this mechanically
// extracted module easier to navigate; production methods continue below.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{duration_matches, exit_status_matches, live_output_is_searchable, snippet};
    use crate::block_view::{BlockFilters, BlockState};

    #[test]
    fn an_unreported_status_matches_neither_exit_filter() {
        let unreported = BlockFilters {
            ..Default::default()
        };
        assert!(exit_status_matches(Some("make"), None, &unreported));

        // "Failed" is a claim about what the shell said, so a block whose status
        // was never reported is not in the failure list.
        let failed_only = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        assert!(!exit_status_matches(Some("make"), None, &failed_only));
        assert!(!exit_status_matches(Some("make"), Some(0), &failed_only));
        assert!(exit_status_matches(Some("make"), Some(1), &failed_only));
        // A legacy/synthetic commandless row remains background output even if
        // it happens to carry a non-zero status.
        assert!(!exit_status_matches(None, Some(1), &failed_only));

        // Nor does it answer to a filter for one specific code, including zero.
        let zero_only = BlockFilters {
            exit_code: Some(0),
            ..Default::default()
        };
        assert!(!exit_status_matches(Some("make"), None, &zero_only));
        assert!(exit_status_matches(Some("make"), Some(0), &zero_only));

        let one_only = BlockFilters {
            exit_code: Some(1),
            ..Default::default()
        };
        assert!(exit_status_matches(Some("false"), Some(1), &one_only));
        assert!(!exit_status_matches(None, Some(1), &one_only));
    }

    #[test]
    fn snippet_passes_through_short_line() {
        assert_eq!(snippet("hello world"), "hello world");
    }

    #[test]
    fn snippet_truncates_long_line_with_ellipsis() {
        let long: String = "a".repeat(500);
        let out = snippet(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().filter(|&c| c == 'a').count(), 240);
    }

    #[test]
    fn snippet_truncates_unicode_on_char_boundaries() {
        for line in [
            format!("a{}", "界".repeat(240)),
            format!("a{}", "🙂".repeat(240)),
        ] {
            let out = snippet(&line);
            assert!(out.ends_with('…'));
            assert_eq!(out.chars().count(), 241);
            assert_eq!(
                out.chars().take(240).collect::<String>(),
                line.chars().take(240).collect::<String>()
            );
        }
    }

    #[test]
    fn slow_filter_excludes_unknown_duration() {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..BlockFilters::default()
        };
        assert!(!duration_matches(None, &filters));
        assert!(!duration_matches(Some(999), &filters));
        assert!(duration_matches(Some(1_000), &filters));
    }

    #[test]
    fn only_current_command_output_joins_the_live_find_surface() {
        assert!(live_output_is_searchable(BlockState::CollectingOutput));
        assert!(live_output_is_searchable(BlockState::PostCommand));
        for state in [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::AwaitingCommand,
            BlockState::AltScreen,
            BlockState::RawFallback,
        ] {
            assert!(!live_output_is_searchable(state), "{state:?}");
        }
    }
}

#[allow(dead_code)]
impl TermView {
    /// Search blocks for a query string (case-insensitive).
    /// Returns indices of matching blocks.
    pub fn search_blocks(&self, query: &str) -> Vec<usize> {
        self.search_blocks_with_filters(query, &BlockFilters::default())
    }

    /// Search blocks with optional filters
    pub fn search_blocks_with_filters(&self, query: &str, filters: &BlockFilters) -> Vec<usize> {
        let q_bytes = query.as_bytes();

        let re = if filters.use_regex && !query.is_empty() {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .ok()
        } else {
            None
        };

        let results: Vec<usize> = self
            .block_data
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                let text_match = if query.is_empty() {
                    true
                } else if let Some(ref re) = re {
                    re.is_match(&b.prompt) || re.is_match(&b.cmd) || re.is_match(&b.output)
                } else {
                    contains_case_insensitive(b.prompt.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.cmd.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.output.as_bytes(), q_bytes)
                };

                if !text_match {
                    return false;
                }

                if !exit_status_matches(Some(&b.cmd), b.exit_code, filters) {
                    return false;
                }

                if !duration_matches(b.duration_ms, filters) {
                    return false;
                }

                true
            })
            .map(|(i, _)| i)
            .collect();

        results
    }

    /// Highlight every occurrence of `query` across the finished blocks and
    /// focus the first hit. Returns (current_1based, total); (0, 0) for no match.
    /// Mirrors Warp's FindWithinBlock highlight pass.
    pub fn find_in_blocks(&self, query: &str, use_regex: bool) -> (usize, usize) {
        self.clear_find();
        if query.is_empty() {
            return (0, 0);
        }
        let pattern = if use_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let re = match regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .multi_line(true)
            .build()
        {
            Ok(re) => re,
            Err(_) => return (0, 0),
        };

        // Compile the same pattern for VTE (PCRE2) so its native highlighter
        // paints every hit and its search cursor can step within each block.
        let vte_re = match vte4::Regex::for_search(
            &pattern,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        ) {
            Ok(r) => r,
            Err(_) => return (0, 0),
        };

        let mut matches: Vec<FindMatch> = Vec::new();
        {
            let finished = self.finished_blocks.borrow();
            for block in finished.iter() {
                let cmd_count = re.find_iter(&block.cmd_text).count();
                let out_count = block.with_stripped_output(|s| re.find_iter(s).count());
                if cmd_count > 0 {
                    block.command_vte.search_set_regex(Some(&vte_re), 0);
                    block.command_vte.search_set_wrap_around(true);
                    for _ in 0..cmd_count {
                        matches.push(FindMatch {
                            block_id: block.id,
                            is_output: false,
                            is_live: false,
                        });
                    }
                }
                if out_count > 0 {
                    block.output_vte.search_set_regex(Some(&vte_re), 0);
                    block.output_vte.search_set_wrap_around(true);
                    for _ in 0..out_count {
                        matches.push(FindMatch {
                            block_id: block.id,
                            is_output: true,
                            is_live: false,
                        });
                    }
                }
            }
        }

        // A running command is the last surface in document order, after all
        // finished blocks. Count from ActiveBlock's bounded raw-output capture
        // so prompt/editor text is excluded; VTE owns painting and stepping the
        // matches that are currently visible in its live viewport.
        if live_output_is_searchable(self.bstate.get()) {
            let live_text = super::strip_ansi(&self.active.borrow().output_text());
            let live_count = re.find_iter(&live_text).count();
            if live_count > 0 {
                self.active_vte.search_set_regex(Some(&vte_re), 0);
                self.active_vte.search_set_wrap_around(true);
                for _ in 0..live_count {
                    matches.push(FindMatch {
                        block_id: 0,
                        is_output: true,
                        is_live: true,
                    });
                }
            }
        }

        if matches.is_empty() {
            return (0, 0);
        }
        let total = matches.len();
        {
            let mut st = self.find_state.borrow_mut();
            st.matches = matches;
            st.current = 0;
        }
        self.focus_current_match();
        self.scroll_to_current_match();
        (1, total)
    }

    /// Step to the next match (wrapping). Returns (current_1based, total).
    pub fn find_next(&self) -> (usize, usize) {
        self.step_find(1)
    }

    /// Step to the previous match (wrapping). Returns (current_1based, total).
    pub fn find_prev(&self) -> (usize, usize) {
        self.step_find(-1)
    }

    fn step_find(&self, delta: isize) -> (usize, usize) {
        let (cur, total) = {
            let st = self.find_state.borrow();
            (st.current, st.matches.len())
        };
        if total == 0 {
            return (0, 0);
        }
        let next = ((cur as isize + delta).rem_euclid(total as isize)) as usize;
        self.find_state.borrow_mut().current = next;
        self.focus_current_match_step(delta);
        self.scroll_to_current_match();
        (next + 1, total)
    }

    /// Move the VTE search cursor on the block backing the current match.
    /// Used after the find_state index is updated; `delta` direction tells
    /// VTE which way to step its internal cursor.
    fn focus_current_match_step(&self, delta: isize) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let vte = if fm.is_live {
            &self.active_vte
        } else if let Some(block) = finished.iter().find(|b| b.id == fm.block_id) {
            if fm.is_output {
                &block.output_vte
            } else {
                &block.command_vte
            }
        } else {
            return;
        };
        if delta >= 0 {
            vte.search_find_next();
        } else {
            vte.search_find_previous();
        }
    }

    /// Move VTE's search cursor to the very first match of the current pass.
    fn focus_current_match(&self) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let vte = if fm.is_live {
            &self.active_vte
        } else if let Some(block) = finished.iter().find(|b| b.id == fm.block_id) {
            if fm.is_output {
                &block.output_vte
            } else {
                &block.command_vte
            }
        } else {
            return;
        };
        vte.search_find_next();
    }

    fn scroll_to_current_match(&self) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let widget = if fm.is_live {
            self.active.borrow().widget().clone()
        } else if let Some(block) = finished.iter().find(|b| b.id == fm.block_id) {
            block.widget().clone()
        } else {
            return;
        };
        let scroll = self.block_scroll.clone();
        glib::idle_add_local_once(move || {
            if let Some(point) = widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0))
            {
                let adj = scroll.vadjustment();
                let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
                let target = adj.value() + point.y() as f64 - adj.page_size() / 3.0;
                adj.set_value(target.clamp(adj.lower(), max_value));
            }
        });
    }

    /// Cross-block ripgrep-style flat-result scan over cached stripped output
    /// and command text. Caller passes a literal substring (case-insensitive)
    /// when `is_regex == false`, else a regex. Returns at most `max_hits`
    ///
    /// hits in block-list order; each hit carries enough context (line
    /// number + the raw line + cmd preview) to drive a palette UI that lets
    /// the user pick one and jump to it.
    ///
    /// Errors only on invalid regex; an empty pattern returns `Ok(vec![])`
    /// so the caller can clear results without a special branch.
    pub fn cross_block_search(
        &self,
        pattern: &str,
        is_regex: bool,
        max_hits: usize,
    ) -> Result<Vec<CrossBlockHit>, String> {
        if pattern.is_empty() {
            return Ok(Vec::new());
        }
        let re = if is_regex {
            Some(
                regex::RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .multi_line(true)
                    .build()
                    .map_err(|e| format!("{e}"))?,
            )
        } else {
            None
        };
        let pattern_bytes = pattern.as_bytes();

        let finished = self.finished_blocks.borrow();
        let mut hits: Vec<CrossBlockHit> = Vec::new();

        for block in finished.iter() {
            if hits.len() >= max_hits {
                break;
            }
            let cmd_preview = block
                .cmd_text
                .lines()
                .next()
                .unwrap_or(&block.cmd_text)
                .to_string();

            // Cmd surface — usually 1 line, but multiline commands exist.
            for (ln_idx, line) in block.cmd_text.lines().enumerate() {
                if hits.len() >= max_hits {
                    break;
                }
                let is_match = match re.as_ref() {
                    Some(re) => re.is_match(line),
                    None => contains_case_insensitive(line.as_bytes(), pattern_bytes),
                };
                if is_match {
                    hits.push(CrossBlockHit {
                        block_id: block.id,
                        is_output: false,
                        line_no: ln_idx + 1,
                        line_text: snippet(line),
                        cmd_preview: cmd_preview.clone(),
                    });
                }
            }

            // Output surface — uses the cached ANSI-stripped view.
            block.with_stripped_output(|s| {
                for (ln_idx, line) in s.lines().enumerate() {
                    if hits.len() >= max_hits {
                        break;
                    }
                    let is_match = match re.as_ref() {
                        Some(re) => re.is_match(line),
                        None => contains_case_insensitive(line.as_bytes(), pattern_bytes),
                    };
                    if is_match {
                        hits.push(CrossBlockHit {
                            block_id: block.id,
                            is_output: true,
                            line_no: ln_idx + 1,
                            line_text: snippet(line),
                            cmd_preview: cmd_preview.clone(),
                        });
                    }
                }
            });
        }
        Ok(hits)
    }

    /// Scroll the named block into view (by stable id, not list index).
    /// Returns `false` if the id is unknown — likely evicted by the
    /// `max_blocks` cap or deleted via the per-block menu.
    pub fn scroll_to_block_id(&self, block_id: u64) -> bool {
        let finished = self.finished_blocks.borrow();
        let Some(block) = finished.iter().find(|b| b.id == block_id) else {
            return false;
        };
        replace_finished_block_selection(
            &finished,
            &self.selected_block_ids,
            &self.selected_block_id,
            &self.selection_anchor_id,
            Some(block_id),
        );
        block.widget().grab_focus();
        let adj = self.block_scroll.vadjustment();
        if let Some(value) = block
            .widget()
            .compute_point(&self.block_scroll, &gtk::graphene::Point::new(0.0, 0.0))
        {
            let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
            let target = adj.value() + value.y() as f64;
            adj.set_value(target.clamp(adj.lower(), max_value));
        }
        true
    }

    /// Light up the chosen block's command/output VTE with a PCRE2 search
    /// for `pattern` and advance its internal search cursor to the first
    /// hit. Other blocks keep whatever highlight state they had — this is
    /// the "jump to this hit" companion for `cross_block_search`. Returns
    /// `false` when the id is unknown or the pattern can't compile.
    pub fn focus_match_in_block(
        &self,
        block_id: u64,
        pattern: &str,
        is_regex: bool,
        is_output: bool,
    ) -> bool {
        if pattern.is_empty() {
            return false;
        }
        let compiled = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let Ok(vte_re) = vte4::Regex::for_search(
            &compiled,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        ) else {
            return false;
        };
        let finished = self.finished_blocks.borrow();
        let Some(block) = finished.iter().find(|b| b.id == block_id) else {
            return false;
        };
        let vte = if is_output {
            &block.output_vte
        } else {
            &block.command_vte
        };
        vte.search_set_regex(Some(&vte_re), 0);
        vte.search_set_wrap_around(true);
        vte.search_find_next();
        true
    }

    /// Remove all find highlights and reset the find cursor (call on close).
    pub fn clear_find(&self) {
        {
            let finished = self.finished_blocks.borrow();
            for block in finished.iter() {
                block.command_vte.search_set_regex(None::<&vte4::Regex>, 0);
                block.output_vte.search_set_regex(None::<&vte4::Regex>, 0);
            }
        }
        self.active_vte.search_set_regex(None::<&vte4::Regex>, 0);
        let mut st = self.find_state.borrow_mut();
        st.matches.clear();
        st.current = 0;
    }

    /// Get only command blocks classified as failed by the shared status model.
    pub fn get_failed_blocks(&self) -> Vec<usize> {
        let filters = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }

    /// Get only slow blocks (duration > threshold)
    pub fn get_slow_blocks(&self, threshold_ms: u64) -> Vec<usize> {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: threshold_ms,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }
}
