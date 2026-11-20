use std::fmt;

const MATCH_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Occurrence {
    /// One-based source line and UTF-8 byte offsets within that line.
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Default)]
pub(crate) struct SourceMatches {
    hits: Vec<Occurrence>,
    current: Option<usize>,
    truncated: bool,
}

impl SourceMatches {
    pub fn update(&mut self, source: &str, query: &str, match_case: bool) {
        let previous = self.active();
        self.hits.clear();
        self.clear_active();
        self.truncated = false;
        if query.is_empty() {
            return;
        }
        let Ok(pattern) = regex::RegexBuilder::new(&regex::escape(query))
            .case_insensitive(!match_case)
            .build()
        else {
            return;
        };
        'source: for (row, line) in source.lines().enumerate() {
            for hit in pattern.find_iter(line) {
                if self.hits.len() == MATCH_LIMIT {
                    self.truncated = true;
                    break 'source;
                }
                self.hits.push(Occurrence {
                    line: row + 1,
                    start: hit.start(),
                    end: hit.end(),
                });
            }
        }
        self.current = previous.and_then(|hit| self.hits.binary_search(&hit).ok());
    }

    pub fn hits(&self, line: Option<usize>) -> &[Occurrence] {
        let Some(line) = line else {
            return &self.hits;
        };
        let start = self.hits.partition_point(|hit| hit.line < line);
        let end = self.hits.partition_point(|hit| hit.line <= line);
        &self.hits[start..end]
    }

    pub fn active(&self) -> Option<Occurrence> {
        self.current.map(|index| self.hits[index])
    }

    pub fn clear_active(&mut self) {
        self.current = None;
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn step(&mut self, line: Option<usize>, previous: bool) -> Option<Occurrence> {
        let count = self.hits.len();
        if count == 0 {
            return None;
        }
        let index = match (self.current, previous) {
            (Some(index), true) => index.checked_sub(1).unwrap_or(count - 1),
            (Some(index), false) => (index + 1) % count,
            (None, true) => line
                .map_or(count, |line| {
                    self.hits.partition_point(|hit| hit.line <= line)
                })
                .checked_sub(1)
                .unwrap_or(count - 1),
            (None, false) => {
                line.map_or(0, |line| self.hits.partition_point(|hit| hit.line < line)) % count
            }
        };
        self.current = Some(index);
        self.active()
    }

    pub fn status(&self) -> String {
        let count = self.hits.len();
        let suffix = if self.truncated { "+" } else { "" };
        match (self.current, count) {
            (_, 0) => "No matches".into(),
            (Some(index), _) => format!("Match {} of {count}{suffix}", index + 1),
            (None, 1) => "1 match".into(),
            (None, _) => format!("{count}{suffix} matches"),
        }
    }
}

pub(crate) fn match_background(active: bool) -> [u8; 3] {
    if active {
        [248, 169, 47]
    } else {
        [246, 216, 126]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum ImageZoom {
    #[default]
    Fit,
    Percent(u16),
}

impl ImageZoom {
    pub const OPTIONS: [Self; 9] = [
        Self::Fit,
        Self::Percent(25),
        Self::Percent(50),
        Self::Percent(75),
        Self::Percent(100),
        Self::Percent(125),
        Self::Percent(150),
        Self::Percent(200),
        Self::Percent(400),
    ];

    pub fn scale(self) -> Option<f32> {
        match self {
            Self::Fit => None,
            Self::Percent(percent) => Some(f32::from(percent) / 100.0),
        }
    }
}

impl fmt::Display for ImageZoom {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fit => out.write_str("Fit image"),
            Self::Percent(100) => out.write_str("100% (actual size)"),
            Self::Percent(percent) => write!(out, "{percent}%"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_search_indexes_literal_unicode_occurrences_by_byte_range() {
        let mut matches = SourceMatches::default();
        let source = "Σ [word] σ [WORD]\r\n雪 [Word]\r\n";
        matches.update(source, "[word]", true);
        assert_eq!(matches.status(), "1 match");
        matches.update(source, "[word]", false);
        let first = Occurrence {
            line: 1,
            start: 3,
            end: 9,
        };
        let second = Occurrence {
            line: 1,
            start: 13,
            end: 19,
        };
        let third = Occurrence {
            line: 2,
            start: 4,
            end: 10,
        };
        assert_eq!(matches.hits(Some(1)), &[first, second]);
        assert_eq!(matches.step(None, false), Some(first));
        assert_eq!(matches.step(Some(1), false), Some(second));
        assert_eq!(matches.step(None, false), Some(third));
        assert_eq!(matches.step(None, false), Some(first));
        assert_eq!(matches.step(None, true), Some(third));
        matches.update(source, "[word]", false);
        assert_eq!(matches.active(), Some(third));
        assert_eq!(matches.status(), "Match 3 of 3");
        matches.clear_active();
        assert_eq!(matches.step(Some(1), true), Some(second));
        matches.update(source, "σ", false);
        assert_eq!(matches.hits(None).len(), 2);
        assert_eq!(matches.active(), None);
    }

    #[test]
    fn broad_search_reports_the_retained_match_limit() {
        let mut matches = SourceMatches::default();
        matches.update(&"a".repeat(MATCH_LIMIT + 1), "a", true);
        assert_eq!(matches.hits(None).len(), MATCH_LIMIT);
        assert_eq!(matches.status(), format!("{MATCH_LIMIT}+ matches"));
        assert_eq!(matches.step(None, true).unwrap().end, MATCH_LIMIT);
        matches.update("a", "", true);
        assert!(matches.is_empty());
        assert_eq!(matches.step(None, false), None);
    }
}
