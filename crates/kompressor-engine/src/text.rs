//! Text primitives with exactly one definition each.
//!
//! Two conventions run through the whole engine. Every size threshold is
//! expressed in **code points**, not bytes, so that a payload's eligibility does
//! not depend on how much non-ASCII it holds; and line splitting recognises the
//! **full Unicode separator set**, not just `\n`, so that a payload using `\x0b`
//! or `\u{2028}` cannot smuggle content past a line-oriented compressor.
//!
//! Getting either wrong silently changes which payloads compress and where
//! markers land, so both live here rather than being open-coded per compressor.

/// Number of Unicode code points.
///
/// Every size threshold in the engine — the 512-character compression gate,
/// `max_field_chars`, the never-larger comparison, the 2% reformat floor —
/// counts code points. Using `str::len` instead would make a payload's
/// eligibility depend on how many non-ASCII characters it holds.
#[inline]
pub fn chars_len(s: &str) -> usize {
    s.chars().count()
}

/// Whether `c` terminates a line.
///
/// The list is the full Unicode one, not Rust's `\n`-only: `\v`, `\f`, the file/group/record
/// separators, NEL and the Unicode line/paragraph separators all split there
/// and none of them split under `str::lines()`.
#[inline]
fn is_line_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Byte ranges of each line, without its terminator — Python `str.splitlines()`.
///
/// Returned as ranges rather than slices so callers that need offsets into the
/// original buffer (the mixed segmenter rewrites by offset) and callers that
/// just want the text can share one scan.
pub fn line_ranges(s: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        // fast path: ASCII, which is every separator except NEL/LS/PS
        let b = bytes[i];
        if b < 0x80 {
            if matches!(b, b'\n' | b'\r' | 0x0b | 0x0c | 0x1c | 0x1d | 0x1e) {
                out.push(start..i);
                // CRLF counts as one terminator
                if b == b'\r' && i + 1 < s.len() && bytes[i + 1] == b'\n' {
                    i += 1;
                }
                i += 1;
                start = i;
                continue;
            }
            i += 1;
            continue;
        }
        let c = s[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if is_line_boundary(c) {
            out.push(start..i);
            i += clen;
            start = i;
            continue;
        }
        i += clen;
    }
    if start < s.len() {
        out.push(start..s.len());
    }
    out
}

/// Python `str.splitlines()`.
pub fn split_lines(s: &str) -> Vec<&str> {
    line_ranges(s).into_iter().map(|r| &s[r]).collect()
}

/// Python `str.splitlines(keepends=True)`: each slice carries its terminator,
/// so concatenating the result reproduces the input byte for byte.
pub fn split_lines_keepends(s: &str) -> Vec<&str> {
    let ranges = line_ranges(s);
    let mut out = Vec::with_capacity(ranges.len());
    for (i, r) in ranges.iter().enumerate() {
        let end = match ranges.get(i + 1) {
            Some(next) => next.start,
            None => s.len(),
        };
        out.push(&s[r.start..end]);
    }
    out
}

/// Python `line.strip()` — Unicode whitespace from both ends.
#[inline]
pub fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// Non-blank lines, the shape most detector ratios are computed over.
pub fn nonblank_lines(s: &str) -> Vec<&str> {
    split_lines(s)
        .into_iter()
        .filter(|l| !is_blank(l))
        .collect()
}

/// A prefix of at most `max_chars` code points, on a char boundary.
///
/// The detector samples the first million characters of a payload; slicing by
/// bytes would panic mid-code-point on non-ASCII input.
pub fn char_prefix(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Byte offset of the `n`-th code point, clamped to the end of the string.
pub fn byte_offset_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// Join lines with `\n`, the way every compressor rebuilds its output.
pub fn join_lines<S: AsRef<str>>(lines: &[S]) -> String {
    let total: usize = lines.iter().map(|l| l.as_ref().len() + 1).sum();
    let mut out = String::with_capacity(total);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.as_ref());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python_separators() {
        // Python: 'a\nb\rc\r\nd\x0be\x0cf\x1cg\x1dh\x1ei\x85j k l'.splitlines()
        let s = "a\nb\rc\r\nd\u{0b}e\u{0c}f\u{1c}g\u{1d}h\u{1e}i\u{85}j\u{2028}k\u{2029}l";
        assert_eq!(
            split_lines(s),
            vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
        );
    }

    #[test]
    fn splitlines_drops_the_trailing_terminator_but_keepends_does_not() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines_keepends("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines_keepends("a\r\nb").concat(), "a\r\nb");
    }

    #[test]
    fn keepends_reconstructs_the_input() {
        for s in ["", "abc", "a\nb\r\nc\rd", "\n\n\n", "x\u{2028}y"] {
            assert_eq!(split_lines_keepends(s).concat(), s);
        }
    }

    #[test]
    fn chars_len_counts_code_points_not_bytes() {
        assert_eq!(chars_len("héllo"), 5);
        assert_eq!("héllo".len(), 6);
    }

    #[test]
    fn char_prefix_never_splits_a_code_point() {
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("héllo", 99), "héllo");
    }
}
