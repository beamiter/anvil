//! ansi — small ANSI helpers used to build the plain-text shadow
//! (`BlockData.output`) consumed by search / export / "copy block".
//! All SGR→TextTag rendering lives in VTE now; see `block_view/blocks.rs`.

pub(crate) fn skip_osc_sequence(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return i + 1;
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

pub(crate) fn skip_escape_sequence(bytes: &[u8], i: usize) -> usize {
    if i + 1 >= bytes.len() {
        return i + 1;
    }

    match bytes[i + 1] {
        b'[' => {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() {
                j += 1;
            }
            j
        }
        b']' => skip_osc_sequence(bytes, i),
        next if (0x20..=0x2f).contains(&next) => {
            let mut j = i + 2;
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && (0x30..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            j
        }
        _ => i + 2,
    }
}

/// Independent bounds for the visibility replay state. A row/column bound
/// alone is insufficient: sparse cursor writes could otherwise retain the
/// product of both limits. The aggregate high-water budget also accounts for
/// `Vec` capacity retained after an erase/refill cycle.
const MAX_VISIBILITY_GRID_ROWS: usize = 100_000;
const MAX_VISIBILITY_GRID_COLS: usize = 10_000;
const MAX_VISIBILITY_GRID_CELLS: usize = 1024 * 1024;
const MAX_VISIBILITY_CSI_PARAM_BYTES: usize = 256;

struct VisibilityGrid {
    rows: Vec<Vec<char>>,
    row_cell_high_water: Vec<usize>,
    cells: usize,
    retained_cells: usize,
}

impl VisibilityGrid {
    fn new() -> Self {
        Self {
            rows: vec![Vec::new()],
            row_cell_high_water: vec![0],
            cells: 0,
            retained_cells: 0,
        }
    }

    fn ensure_row(&mut self, row: usize) -> bool {
        if row >= MAX_VISIBILITY_GRID_ROWS {
            return false;
        }
        if self.rows.len() <= row {
            self.rows.resize_with(row + 1, Vec::new);
            self.row_cell_high_water.resize(row + 1, 0);
        }
        true
    }

    fn ensure_len(&mut self, row: usize, len: usize) -> bool {
        if row >= MAX_VISIBILITY_GRID_ROWS || len > MAX_VISIBILITY_GRID_COLS {
            return false;
        }
        let old_len = self.rows.get(row).map_or(0, Vec::len);
        let old_high_water = self.row_cell_high_water.get(row).copied().unwrap_or(0);
        let additional = len.saturating_sub(old_len);
        let retained_additional = len.saturating_sub(old_high_water);
        if additional > MAX_VISIBILITY_GRID_CELLS.saturating_sub(self.cells)
            || retained_additional > MAX_VISIBILITY_GRID_CELLS.saturating_sub(self.retained_cells)
        {
            return false;
        }
        if !self.ensure_row(row) {
            return false;
        }
        if additional > 0 {
            if self.rows[row].capacity() < len
                && self.rows[row].try_reserve_exact(additional).is_err()
            {
                return false;
            }
            self.rows[row].resize(len, ' ');
            self.cells += additional;
        }
        if retained_additional > 0 {
            self.row_cell_high_water[row] = len;
            self.retained_cells += retained_additional;
        }
        true
    }

    fn set_cell(&mut self, row: usize, col: usize, value: char) -> bool {
        let Some(len) = col.checked_add(1) else {
            return false;
        };
        if !self.ensure_len(row, len) {
            return false;
        }
        self.rows[row][col] = value;
        true
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.rows.push(Vec::new());
        self.row_cell_high_water.clear();
        self.row_cell_high_water.push(0);
        self.cells = 0;
        self.retained_cells = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let Some(cells) = self.rows.get_mut(row) else {
            return;
        };
        self.cells = self.cells.saturating_sub(cells.len());
        cells.clear();
    }

    fn clear_rows_before(&mut self, end: usize) {
        for cells in self.rows.iter_mut().take(end) {
            self.cells = self.cells.saturating_sub(cells.len());
            cells.clear();
        }
    }

    fn truncate_row(&mut self, row: usize, len: usize) {
        let Some(cells) = self.rows.get_mut(row) else {
            return;
        };
        if cells.len() > len {
            self.cells -= cells.len() - len;
            cells.truncate(len);
        }
    }

    fn truncate_rows(&mut self, len: usize) {
        if self.rows.len() <= len {
            return;
        }
        let removed = self.rows[len..].iter().map(Vec::len).sum::<usize>();
        let removed_retained = self.row_cell_high_water[len..].iter().sum::<usize>();
        self.cells = self.cells.saturating_sub(removed);
        self.retained_cells = self.retained_cells.saturating_sub(removed_retained);
        self.rows.truncate(len);
        self.row_cell_high_water.truncate(len);
    }

    fn has_visible_text(&self) -> bool {
        self.rows
            .iter()
            .flatten()
            .any(|ch| !ch.is_whitespace() && !ch.is_control())
    }
}

/// Decode one UTF-8 scalar with `String::from_utf8_lossy`-equivalent invalid
/// sequence boundaries, without allocating a replacement string.
fn next_lossy_char(bytes: &[u8], index: usize) -> (char, usize) {
    let first = bytes[index];
    if first.is_ascii() {
        return (char::from(first), 1);
    }
    let width = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return ('\u{FFFD}', 1),
    };
    let remaining = &bytes[index..];
    let candidate_len = width.min(remaining.len());
    match std::str::from_utf8(&remaining[..candidate_len]) {
        Ok(text) if candidate_len == width => (
            text.chars().next().expect("one complete UTF-8 scalar"),
            width,
        ),
        Err(error) => (
            '\u{FFFD}',
            error.error_len().unwrap_or(remaining.len()).max(1),
        ),
        Ok(_) => ('\u{FFFD}', remaining.len().max(1)),
    }
}

fn replay_visibility(bytes: &[u8]) -> VisibilityGrid {
    let mut grid = VisibilityGrid::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    let params_start = i;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        break;
                    }
                    let final_byte = bytes[i];
                    let params = &bytes[params_start..i];
                    i += 1;
                    if params.len() > MAX_VISIBILITY_CSI_PARAM_BYTES
                        || matches!(params.first(), Some(0x3c..=0x3f))
                    {
                        continue;
                    }

                    match final_byte {
                        b'H' | b'f' => {
                            row = parse_param_nth(params, 0, 1)
                                .saturating_sub(1)
                                .min(MAX_VISIBILITY_GRID_ROWS);
                            col = parse_param_nth(params, 1, 1)
                                .saturating_sub(1)
                                .min(MAX_VISIBILITY_GRID_COLS);
                        }
                        b'A' => row = row.saturating_sub(parse_param_first(params, 1)),
                        b'B' | b'e' => {
                            row = row
                                .saturating_add(parse_param_first(params, 1))
                                .min(MAX_VISIBILITY_GRID_ROWS);
                        }
                        b'E' => {
                            row = row
                                .saturating_add(parse_param_first(params, 1))
                                .min(MAX_VISIBILITY_GRID_ROWS);
                            col = 0;
                        }
                        b'F' => {
                            row = row.saturating_sub(parse_param_first(params, 1));
                            col = 0;
                        }
                        b'd' => {
                            row = parse_param_first(params, 1)
                                .saturating_sub(1)
                                .min(MAX_VISIBILITY_GRID_ROWS);
                        }
                        b'C' | b'a' => {
                            col = col
                                .saturating_add(parse_param_first(params, 1))
                                .min(MAX_VISIBILITY_GRID_COLS);
                        }
                        b'D' => col = col.saturating_sub(parse_param_first(params, 1)),
                        b'G' | b'`' => {
                            col = parse_param_first(params, 1)
                                .saturating_sub(1)
                                .min(MAX_VISIBILITY_GRID_COLS);
                        }
                        b'J' => match params {
                            b"" | b"0" => {
                                grid.truncate_row(row, col);
                                grid.truncate_rows(row.saturating_add(1));
                            }
                            b"1" => {
                                grid.clear_rows_before(row);
                                if let Some(cells) = grid.rows.get_mut(row) {
                                    let upto = col.min(cells.len());
                                    for cell in cells.iter_mut().take(upto) {
                                        *cell = ' ';
                                    }
                                }
                            }
                            b"2" | b"3" => {
                                grid.clear();
                                row = 0;
                                col = 0;
                            }
                            _ => {}
                        },
                        b'K' => match params {
                            b"" | b"0" => grid.truncate_row(row, col),
                            b"1" => {
                                if let Some(cells) = grid.rows.get_mut(row) {
                                    let upto = col.min(cells.len());
                                    for cell in cells.iter_mut().take(upto) {
                                        *cell = ' ';
                                    }
                                }
                            }
                            b"2" => grid.clear_row(row),
                            _ => {}
                        },
                        _ => {}
                    }
                }
                b']' => i = skip_osc_sequence(bytes, i),
                _ => i = skip_escape_sequence(bytes, i),
            }
        } else if bytes[i] == b'\n' {
            row = row.saturating_add(1).min(MAX_VISIBILITY_GRID_ROWS);
            col = 0;
            if !grid.ensure_row(row) {
                break;
            }
            i += 1;
        } else if bytes[i] == b'\r' {
            col = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            col = col.saturating_sub(1);
            i += 1;
        } else {
            let (ch, consumed) = next_lossy_char(bytes, i);
            i += consumed;
            if col >= MAX_VISIBILITY_GRID_COLS || !grid.set_cell(row, col, ch) {
                break;
            }
            col = col.saturating_add(1);
        }
    }
    grid
}

/// Whether replaying `bytes` leaves a visible, non-whitespace cell. The common
/// no-cursor-control path decodes directly from the byte slice, including
/// invalid UTF-8 replacement semantics; cursor motion and erases use the
/// bounded grid above. Neither path allocates a lossy serialized copy.
pub(crate) fn ansi_has_visible_text(bytes: &[u8]) -> bool {
    if memchr::memchr3(0x1b, b'\r', b'\x08', bytes).is_none() {
        let mut i = 0;
        while i < bytes.len() {
            let (ch, consumed) = next_lossy_char(bytes, i);
            if !ch.is_whitespace() && !ch.is_control() {
                return true;
            }
            i += consumed;
        }
        return false;
    }
    replay_visibility(bytes).has_visible_text()
}

pub(crate) fn strip_ansi_with_clear_detect(input: &str) -> (String, bool) {
    // Plain command output is overwhelmingly the common case. LF does not
    // alter replay semantics: the slow path splits rows and serializes the
    // same separators again. Only ESC, CR, and BS can change or erase already
    // emitted cells, so avoid the per-character grid and row allocations when
    // none of those bytes are present.
    if memchr::memchr3(0x1b, b'\r', b'\x08', input.as_bytes()).is_none() {
        return (input.to_owned(), false);
    }
    strip_ansi_with_clear_detect_replay(input)
}

fn strip_ansi_with_clear_detect_replay(input: &str) -> (String, bool) {
    let bytes = input.as_bytes();
    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    let mut cursor = 0usize;
    let mut i = 0;
    let mut should_clear = false;
    let mut param_buf: Vec<u8> = Vec::with_capacity(16);

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    param_buf.clear();
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        param_buf.push(bytes[i]);
                        i += 1;
                    }
                    if i < bytes.len() {
                        let final_byte = bytes[i];
                        i += 1;

                        let cells = lines.last_mut().unwrap();
                        match final_byte {
                            b'J' => {
                                if param_buf == b"2" || param_buf == b"3" {
                                    should_clear = true;
                                }
                            }
                            b'K' => match param_buf.as_slice() {
                                b"" | b"0" => cells.truncate(cursor),
                                b"1" => {
                                    for c in cells.iter_mut().take(cursor) {
                                        *c = ' ';
                                    }
                                }
                                b"2" => {
                                    cells.clear();
                                    cursor = 0;
                                }
                                _ => {}
                            },
                            b'C' => {
                                let count = parse_param_first(&param_buf, 1);
                                cursor = (cursor + count).min(cells.len());
                            }
                            b'D' => {
                                let count = parse_param_first(&param_buf, 1);
                                cursor = cursor.saturating_sub(count);
                            }
                            b'G' => {
                                let col = parse_param_first(&param_buf, 1);
                                cursor = col.saturating_sub(1).min(cells.len());
                            }
                            _ => {}
                        }
                    }
                }
                b']' => {
                    i = skip_osc_sequence(bytes, i);
                }
                _ => {
                    i = skip_escape_sequence(bytes, i);
                }
            }
        } else if bytes[i] == b'\n' {
            lines.push(Vec::new());
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\r' {
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            let cells = lines.last_mut().unwrap();
            if cursor < cells.len() {
                cells[cursor] = ch;
            } else {
                cells.push(ch);
            }
            cursor += 1;
        }
    }
    let mut result = String::with_capacity(input.len());
    for (line_idx, cells) in lines.iter().enumerate() {
        if line_idx > 0 {
            result.push('\n');
        }
        for &ch in cells {
            result.push(ch);
        }
    }
    (result, should_clear)
}

pub(crate) fn contains_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }

    // Keep the allocation-free memchr path for the overwhelmingly common
    // ASCII case.  For terminal text containing non-ASCII UTF-8, use Rust's
    // Unicode lowercase mapping so literal filters match e.g. "ÉCHEC" with
    // "échec" instead of comparing only individual ASCII bytes.
    if !(haystack.is_ascii() && needle.is_ascii()) {
        if let (Ok(haystack), Ok(needle)) =
            (std::str::from_utf8(haystack), std::str::from_utf8(needle))
        {
            return haystack.to_lowercase().contains(&needle.to_lowercase());
        }
    }
    if haystack.len() < needle.len() {
        return false;
    }

    fn scan(haystack: &[u8], needle: &[u8], first: u8) -> bool {
        for pos in memchr::memchr_iter(first, haystack) {
            if pos + needle.len() > haystack.len() {
                break;
            }
            let candidate = &haystack[pos..pos + needle.len()];
            if candidate
                .iter()
                .zip(needle.iter())
                .all(|(&h, &n)| h.eq_ignore_ascii_case(&n))
            {
                return true;
            }
        }
        false
    }

    let first_lower = needle[0].to_ascii_lowercase();
    if scan(haystack, needle, first_lower) {
        return true;
    }

    let first_upper = needle[0].to_ascii_uppercase();
    if first_upper != first_lower && scan(haystack, needle, first_upper) {
        return true;
    }
    false
}

pub(crate) fn parse_param_first(buf: &[u8], default: usize) -> usize {
    parse_param_nth(buf, 0, default)
}

/// Parse one semicolon-separated numeric parameter, saturating rather than
/// overflowing on adversarially long digit runs.
pub(crate) fn parse_param_nth(buf: &[u8], n: usize, default: usize) -> usize {
    let Some(field) = buf.split(|&b| b == b';').nth(n) else {
        return default;
    };
    if field.is_empty() {
        return default;
    }
    let mut val = 0usize;
    for &b in field {
        if b.is_ascii_digit() {
            val = val.saturating_mul(10).saturating_add((b - b'0') as usize);
        } else {
            return default;
        }
    }
    if val == 0 {
        default
    } else {
        val
    }
}

pub(crate) fn strip_ansi(input: &str) -> String {
    strip_ansi_with_clear_detect(input).0
}

#[cfg(test)]
mod tests {
    use super::{
        ansi_has_visible_text, contains_case_insensitive as cci, strip_ansi_with_clear_detect,
        strip_ansi_with_clear_detect_replay,
    };

    #[test]
    fn plain_fast_path_matches_replay_semantics() {
        let cases = [
            "",
            "one line",
            "first\nsecond\n",
            "\nleading and trailing\n",
            "tabs\tand\0controls\u{7f}",
            "Unicode: 界 🙂 e\u{301}\nПривет\n",
        ];
        for input in cases {
            assert!(memchr::memchr3(0x1b, b'\r', b'\x08', input.as_bytes()).is_none());
            assert_eq!(
                strip_ansi_with_clear_detect(input),
                strip_ansi_with_clear_detect_replay(input),
                "plain fast path diverged for {input:?}"
            );
            assert_eq!(
                strip_ansi_with_clear_detect(input),
                (input.to_owned(), false)
            );
        }
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn plain_strip_ansi_micro_benchmark() {
        for bytes in [1 << 20, 8 << 20] {
            let mut input = "ordinary build output\n".repeat(bytes / 22 + 1);
            input.truncate(bytes);

            let replay_started = std::time::Instant::now();
            std::hint::black_box(strip_ansi_with_clear_detect_replay(&input));
            let replay_elapsed = replay_started.elapsed();

            let fast_started = std::time::Instant::now();
            std::hint::black_box(strip_ansi_with_clear_detect(&input));
            eprintln!(
                "plain strip {} MiB: replay={replay_elapsed:?}, fast={:?}",
                bytes >> 20,
                fast_started.elapsed()
            );
        }
    }

    #[test]
    fn matches_regardless_of_case() {
        assert!(cci(b"Hello World", b"hello"));
        assert!(cci(b"Hello World", b"HELLO"));
        assert!(cci(b"hello world", b"world"));
        assert!(cci(b"MiXeD", b"mixed"));
    }

    #[test]
    fn reports_absent_needle() {
        assert!(!cci(b"abcdef", b"xyz"));
    }

    #[test]
    fn empty_needle_always_matches() {
        assert!(cci(b"anything", b""));
    }

    #[test]
    fn finds_uppercase_first_byte_after_oob_lowercase_hit() {
        // Regression: the lowercase-first-byte scan finds 'f' at the final
        // position (out of bounds for a 2-byte needle). The old code returned
        // false there, skipping the uppercase-variant scan that matches "Fo"
        // at position 0.
        assert!(cci(b"Foxf", b"fo"));
    }

    #[test]
    fn matches_unicode_case_insensitively() {
        assert!(cci("ÉCHEC: déjà vu".as_bytes(), "échec".as_bytes()));
        assert!(cci("ПРИВЕТ мир".as_bytes(), "привет".as_bytes()));
        assert!(!cci("你好世界".as_bytes(), "再见".as_bytes()));
    }

    #[test]
    fn visibility_replays_cursor_overwrite_and_erases_without_plain_copy() {
        assert!(!ansi_has_visible_text(b"\r\n\x1b[0m"));
        assert!(ansi_has_visible_text(b"\x1b[36mworker finished\x1b[0m\r\n"));
        assert!(ansi_has_visible_text(b"visible\rhidden"));
        assert!(!ansi_has_visible_text(b"visible\r\x1b[2K"));
        assert!(!ansi_has_visible_text(
            b"\x1b]8;;https://example.invalid\x1b\\\x1b]8;;\x1b\\"
        ));
        assert!(!ansi_has_visible_text(b"first\nsecond\x1b[2J"));
    }

    #[test]
    fn visibility_scans_large_invalid_utf8_without_a_lossy_copy() {
        let invalid = vec![0xff; 4 * 1024 * 1024];
        assert!(ansi_has_visible_text(&invalid));
        assert!(!ansi_has_visible_text(b" \n\t"));
    }
}
