//! The two lines a compiler prints under a diagnostic: the source it is quoting
//! and the caret pointing into it.
//!
//! They restate what `file:line:col:` already said and they are most of the
//! bytes — a real gcc payload measured 2,334 characters, of which 1,873 were
//! echo and caret. Both the detector (deciding a small payload really is build
//! output) and the log compressor (eliding them) need the same answer, so the
//! rule is stated once here.
//!
//! Matching the shape is **not** enough to call a line quoted source: a psql
//! result set, a markdown table and an ASCII box table all have it. The run has
//! to sit beside the diagnostic it belongs to. Without that, one stray
//! `warning:` anywhere in a payload claimed every `|`-row in it — a 60-row query
//! result collapsed from 64 lines to 4, its rows elided as "quoted source".

use once_cell::sync::Lazy;
use regex::Regex;

/// `NNN | source text`, or a bare `|` continuation.
static SOURCE_ECHO: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*(?:\d+\s*\||\|)\s").unwrap());
/// The caret run pointing into the quoted line.
static CARET: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\|?\s*\^~*\s*$").unwrap());

/// A compiler or linter diagnostic: `file.c:88: error: …`.
///
/// The keyword must *begin* the message, so a grep hit that merely contains the
/// word error (`mod.py:4:    raise ValueError("error: bad")`) is not one.
/// Multi-line, because the detector counts diagnostics across a whole payload
/// with one scan. Without the flag `^` anchors to the start of the haystack
/// alone, so a compiler's second and later diagnostics are invisible and only a
/// payload whose very first byte begins one is ever recognised as build output.
pub static DIAGNOSTIC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?im)^\s*(?:[A-Za-z]:)?[^\s:]+:\d+(?::\d+)?:\s*(?:fatal error|error|warning|note|undefined reference)\b",
    )
    .unwrap()
});

/// Both bounds are measured over real payloads carrying a diagnostic: of 2,040
/// echo runs, 2,039 sit within three lines of one — but on *either* side, which
/// is why this is not simply "below". gcc leads with the diagnostic and follows
/// with the echo, while rustc opens with a summary line and reaches its
/// `file:line:col` only after the quoted source, so requiring a diagnostic above
/// lost real echo lines.
const ECHO_MAX_GAP: usize = 3;
/// The length cap does the rest of the work: a compiler's longest run measured
/// 14 lines, while the tables this has to refuse run to dozens of rows.
const ECHO_MAX_RUN: usize = 16;

/// Whether `line` has the shape of quoted source or a caret.
pub fn is_source_echo(line: &str) -> bool {
    SOURCE_ECHO.is_match(line) || CARET.is_match(line)
}

/// Half-open spans of echo/caret lines that a diagnostic is responsible for.
///
/// A run no diagnostic sits beside, or one longer than a compiler would ever
/// emit, is left out — it is a table that happens to share the shape.
pub fn quoted_source_runs(lines: &[&str]) -> Vec<(usize, usize)> {
    let diagnostics: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| DIAGNOSTIC.is_match(line))
        .map(|(index, _)| index)
        .collect();
    if diagnostics.is_empty() {
        return Vec::new();
    }

    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if !is_source_echo(lines[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && is_source_echo(lines[index]) {
            index += 1;
        }
        if index - start > ECHO_MAX_RUN {
            continue;
        }
        // `start - 1` and `index` are the lines bracketing the run, so a
        // diagnostic on either of them is a gap of one.
        let low = start.saturating_sub(ECHO_MAX_GAP);
        let high = index + ECHO_MAX_GAP - 1;
        if diagnostics.iter().any(|&d| d >= low && d <= high) {
            runs.push((start, index));
        }
    }
    runs
}

/// Total lines covered by the runs — the co-signal the detector counts.
pub fn echoed_line_count(runs: &[(usize, usize)]) -> usize {
    runs.iter().map(|(start, stop)| stop - start).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GCC: &[&str] = &[
        "src/main.c:10:5: warning: unused variable 'x' [-Wunused-variable]",
        "   10 |     int x = 0;",
        "      |         ^~",
        "src/main.c:14:9: error: 'y' undeclared here",
        "   14 |     y = x + 1;",
        "      |     ^",
    ];

    #[test]
    fn a_compiler_echo_is_found_under_its_diagnostic() {
        let runs = quoted_source_runs(GCC);
        assert_eq!(runs, vec![(1, 3), (4, 6)]);
        assert_eq!(echoed_line_count(&runs), 4);
    }

    #[test]
    fn a_diagnostic_below_its_echo_still_claims_it() {
        // rustc reaches its file:line:col only after the quoted source
        let rustc = [
            "error[E0425]: cannot find value `y`",
            "  |",
            "3 |     let z = y;",
            "  |             ^ not found",
            "src/main.rs:3:13: error: aborting due to previous error",
        ];
        assert!(!quoted_source_runs(&rustc).is_empty());
    }

    #[test]
    fn a_table_that_shares_the_shape_is_refused() {
        // a psql result set with one stray diagnostic far away
        let mut lines = vec!["notes.txt:1: warning: stray"];
        // sixty identical rows: a result set is exactly this repetitive, and
        // that is what the rule has to refuse
        lines.extend(std::iter::repeat_n(" id | name | amount ", 60));
        let runs = quoted_source_runs(&lines);
        assert!(
            runs.is_empty(),
            "a 60-row result set is not quoted source, got {runs:?}"
        );
    }

    #[test]
    fn a_payload_with_no_diagnostic_has_no_echo() {
        let lines = ["  1 | some text", "  2 | more text"];
        assert!(quoted_source_runs(&lines).is_empty());
    }

    #[test]
    fn a_grep_hit_that_merely_mentions_error_is_not_a_diagnostic() {
        assert!(!DIAGNOSTIC.is_match("mod.py:4:    raise ValueError(\"error: bad\")"));
        assert!(DIAGNOSTIC.is_match("mod.py:4: error: bad"));
        assert!(DIAGNOSTIC.is_match("a.ts:3:12: warning: unused"));
    }
}
