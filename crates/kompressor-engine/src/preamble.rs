//! The metadata frame an agent harness wraps around every tool result.
//!
//! Harnesses stamp `Chunk ID:` / `Wall time:` / `Process exited with code N` /
//! `Original token count:` / `Output:` around a command's output, or banner a
//! terminal pane with `New Terminal Output:`. There are two separate reasons to
//! hold that frame out of compression:
//!
//! * **it carries the outcome.** `Process exited with code 1` is often the
//!   single most load-bearing line in a payload, and `Process running with
//!   session ID N` is the handle an agent needs to poll something it left
//!   running. A compressor that selects by score dropped both on a real run.
//! * **it defeats parse-based compression.** It is not part of whatever the
//!   payload *is*, so a grammar reports a syntax error on the whole file; real C
//!   payloads with elidable bodies were returned whole for that reason alone.
//!
//! So split it off, compress the body, and re-attach the frame verbatim.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::text::split_lines_keepends;

/// How many leading lines may frame a tool result before its real output.
///
/// Ten rather than eight: codex's truncated frame is already seven lines —
/// `Chunk ID`, `Wall time`, `Process exited`, `Original token count`,
/// `Output:`, `Warning: truncated output`, `Total output lines` — and a bound
/// that a real frame sits exactly on is a bound that silently fails the next
/// time one grows.
pub const MAX_PREAMBLE_LINES: usize = 10;

/// The keys a harness stamps around a command's output.
///
/// The frame is a moving target, so this list is the union of the shapes that
/// have actually been seen, not a guess at a format. codex 0.155.0-alpha.16.4
/// writes `Script completed` / `Wall time 0.1 seconds` / `Output:` where
/// 0.155.0-alpha.9 wrote `Chunk ID:` / `Wall time:` / `Process exited`, and the
/// two new shapes matched nothing: `Script completed` was absent entirely and
/// `Wall time` had lost its colon. Neither line started a run, so the splitter
/// returned an empty preamble and the frame went to the parser — a syntax error
/// on line 1, the whole file handed back. Measured on the sweep's one Python
/// payload: **0.00% with the frame, 43.06% without**, against 90.96% for the
/// same file untruncated. `Wall time\b` now covers both spellings.
static KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?:Chunk ID:|Wall time\b|Process (?:exited|running)\b|Original token count:",
        r"|Script (?:completed|failed)\b",
        r"|Warning: truncated output|Total output lines:)",
    ))
    .unwrap()
});

/// Terminators: matched, but never allowed to *start* a run, so a payload whose
/// own first line is `Output:` is treated as content rather than as a frame.
static END: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:Output:|New Terminal Output:|Current Terminal Screen:)\s*$").unwrap()
});

/// Frame lines codex writes *after* the terminator, when it truncated.
///
/// The terminator is not the end of the frame on a truncated result: codex
/// stamps `Output:` and then adds its truncation notice underneath, so a
/// splitter that stops at the terminator leaves two lines of prose at the head
/// of the body. That is enough to defeat every parse-based compressor, and it
/// happens on exactly the payloads worth compressing — the large ones.
///
/// Measured: a 40 KB Python result came back at **0.00%** with those two lines
/// in front of it and **86.93%** without, and a CSV result reclassified from
/// `text` to `tabular`.
///
/// Kept deliberately narrow. Anything after the terminator is content until
/// proven otherwise, so this lists the two notices rather than reusing [`KEY`]:
/// a payload whose own first line happens to start `Wall time:` is content, and
/// after the terminator it stays content.
static NOTICE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:Warning: truncated output|Total output lines:)").unwrap());

/// Split `content` into `(preamble, body)`.
///
/// The preamble is `""` when there is no frame. Line endings are preserved on
/// both halves, so `preamble + body` is always the original content.
pub fn split_harness_preamble(content: &str) -> (&str, &str) {
    let lines = split_lines_keepends(content);
    let mut count = 0usize;
    while count < MAX_PREAMBLE_LINES.min(lines.len()) {
        let line = lines[count];
        if KEY.is_match(line) {
            count += 1;
            continue;
        }
        if count > 0 && END.is_match(line.trim_end_matches(['\r', '\n'])) {
            count += 1; // the terminator belongs to the frame
                        // …and so does a truncation notice written under it.
            while count < MAX_PREAMBLE_LINES.min(lines.len()) && NOTICE.is_match(lines[count]) {
                count += 1;
            }
        }
        break;
    }
    if count == 0 || count >= lines.len() {
        return ("", content);
    }
    let split_at: usize = lines[..count].iter().map(|l| l.len()).sum();
    (&content[..split_at], &content[split_at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codex_frame_is_held_out_of_the_body() {
        let content = "Chunk ID: abc\nWall time: 1.2 seconds\nProcess exited with code 1\nOutput:\nreal line\n";
        let (preamble, body) = split_harness_preamble(content);
        assert_eq!(
            preamble,
            "Chunk ID: abc\nWall time: 1.2 seconds\nProcess exited with code 1\nOutput:\n"
        );
        assert_eq!(body, "real line\n");
    }

    #[test]
    fn the_frame_codex_writes_today_is_held_out_too() {
        // 0.155.0-alpha.16.4. `Script completed` did not exist in the key list
        // and `Wall time` had lost its colon, so nothing matched, the preamble
        // came back empty, and the parser met `Script completed` on line 1.
        let content = concat!(
            "Script completed\n",
            "Wall time 0.1 seconds\n",
            "Output:\n",
            "Warning: truncated output (original token count: 16573)\n",
            "Total output lines: 1577\n",
            "\n",
            "def transform_0(records):\n",
        );
        let (preamble, body) = split_harness_preamble(content);
        assert!(
            preamble.ends_with("Total output lines: 1577\n"),
            "the whole frame belongs to the preamble, got {preamble:?}"
        );
        assert!(
            body.trim_start().starts_with("def transform_0"),
            "the body must start at the source, got {body:?}"
        );
    }

    #[test]
    fn a_truncation_notice_under_the_terminator_is_part_of_the_frame() {
        // The shape codex writes whenever it cuts a tool result down. Leaving
        // these two lines at the head of the body is what returned a 40 KB
        // Python result at 0.00% instead of 86.93%.
        let content = concat!(
            "Chunk ID: ff1912\n",
            "Wall time: 0.0000 seconds\n",
            "Process exited with code 0\n",
            "Original token count: 16692\n",
            "Output:\n",
            "Warning: truncated output (original token count: 16692)\n",
            "Total output lines: 1577\n",
            "\n",
            "def transform_0(records):\n",
        );
        let (preamble, body) = split_harness_preamble(content);
        assert!(
            preamble.ends_with("Total output lines: 1577\n"),
            "{preamble:?}"
        );
        assert_eq!(body, "\ndef transform_0(records):\n");
    }

    #[test]
    fn a_notice_is_only_a_notice_after_the_terminator() {
        // Before one, it is already covered by `KEY`; on its own it is content.
        // A payload that opens with the words is output, not a frame.
        let content = "Total output lines: 4\nreal line\n";
        let (preamble, body) = split_harness_preamble(content);
        assert_eq!(
            preamble, "Total output lines: 4\n",
            "KEY still owns it up front"
        );
        assert_eq!(body, "real line\n");

        // …but a line that merely resembles one, after the terminator, is not
        // swallowed: only the two notices are.
        let content = "Chunk ID: a\nOutput:\nWall time is what this log line is about\n";
        let (_, body) = split_harness_preamble(content);
        assert_eq!(body, "Wall time is what this log line is about\n");
    }

    #[test]
    fn the_halves_always_reconstruct_the_input() {
        for content in [
            "Chunk ID: a\nOutput:\nbody\n",
            "no frame at all\njust content\n",
            "Wall time: 3\r\nOutput:\r\nbody\r\n",
            "",
        ] {
            let (p, b) = split_harness_preamble(content);
            assert_eq!(format!("{p}{b}"), content);
        }
    }

    #[test]
    fn a_payload_that_opens_with_output_is_content_not_a_frame() {
        let content = "Output:\nsomething\n";
        let (preamble, body) = split_harness_preamble(content);
        assert_eq!(preamble, "", "a terminator cannot start a frame");
        assert_eq!(body, content);
    }

    #[test]
    fn a_frame_with_no_body_is_left_alone() {
        // nothing to compress, so there is nothing to protect it from
        let content = "Chunk ID: abc\nWall time: 1.2 seconds\n";
        let (preamble, body) = split_harness_preamble(content);
        assert_eq!(preamble, "");
        assert_eq!(body, content);
    }

    #[test]
    fn the_frame_is_bounded() {
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&format!("Wall time: {i}\n"));
        }
        content.push_str("body\n");
        let (preamble, _) = split_harness_preamble(&content);
        assert_eq!(preamble.lines().count(), MAX_PREAMBLE_LINES);
    }

    #[test]
    fn a_session_handle_survives() {
        let content = "Process running with session ID 7\nOutput:\nstill going\n";
        let (preamble, body) = split_harness_preamble(content);
        assert!(preamble.contains("session ID 7"));
        assert_eq!(body, "still going\n");
    }
}
