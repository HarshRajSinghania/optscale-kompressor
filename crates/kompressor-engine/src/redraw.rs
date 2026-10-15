//! Resolve the carriage returns a terminal progress meter leaves in captured
//! output.
//!
//! A carriage return moves the cursor to column 0 **without clearing the line**,
//! so what follows overwrites the frame beneath it character by character and
//! anything the new frame is too short to reach stays on screen. A `curl`
//! download, an `apt-get install` or a `docker pull` therefore leaves one line
//! holding every frame it ever drew, of which a human saw exactly one thing: the
//! overlay of all of them.
//!
//! So compute that overlay. It is the terminal's own rule, it needs no heuristic
//! about what a progress meter looks like, and it cannot remove anything that
//! was still visible when the command finished.
//!
//! The rule this replaced — keep only the text after the last `\r` — is the one
//! that looks obviously right and is not. Captured output is stdout and stderr
//! merged, so a meter drawing on stderr can be interrupted mid-line by a payload
//! arriving on stdout, and the meter's next frame then lands after it. On a real
//! 40K payload the frames were a 78-character meter, a 78-character meter
//! carrying an entire 27,178-character JSON response body, and a final meter.
//! Taking the last frame deletes the response the agent asked for; overlaying
//! keeps it, because a 78-character frame overwrites 78 characters and no more.

/// A meter frame is a status line: it fits a terminal. Above this, the "frame"
/// is carrying payload — stdout interrupting stderr mid-line — and the line is
/// left entirely alone.
///
/// This cap is the difference between faithful and useful. The overlay rule is
/// exactly what the terminal did, and on the real `curl` payload what the
/// terminal did was overwrite the first 78 characters of a 27,178-character
/// frame: the meter prefix, plus the opening of the JSON body behind it. A human
/// would indeed never have seen those bytes, but handing an agent a response
/// body with its first six characters missing is worse than handing it the raw
/// frames — unparseable rather than merely noisy.
///
/// Known limitation, the same trade seen from the other side: text whose line
/// separator is a lone CR (classic Mac line endings, or a tool emitting records
/// that way) is frames all the way down, each short enough to pass the cap. It
/// resolves to its overlay, which keeps roughly the last record and drops the
/// rest. That is faithful and still wrong, and it is left in because no textual
/// test separates the two — progress frames and CR-separated records both share
/// a prefix, both carry a changing number, both sit under the cap. What bounds
/// it: the fold runs only when fragments can be retained, so the original lines
/// are always retrievable through the marker's handle.
const MAX_FRAME_CHARS: usize = 256;

/// The visible remains of one line after its carriage returns are applied.
pub fn resolve_overwrites(line: &str) -> String {
    let mut screen = String::new();
    for frame in line.split('\r') {
        // each frame lands at column 0 and overwrites only as far as it reaches
        let frame_chars = frame.chars().count();
        let tail: String = screen.chars().skip(frame_chars).collect();
        screen.clear();
        screen.push_str(frame);
        screen.push_str(&tail);
    }
    screen
}

/// Apply the terminal's overwrite rule to every line of `content`.
///
/// Returns `(resolved, overwritten_lines)`, where `overwritten_lines` holds the
/// original text of every line that changed, in order, so the caller can stash
/// them behind one marker and keep the removal recoverable. `None` means no line
/// carried an overwrite and `content` is already what the terminal showed.
pub fn collapse_redraws(content: &str) -> Option<(String, Vec<&str>)> {
    // no carriage return anywhere is the overwhelmingly common case
    memchr::memchr(b'\r', content.as_bytes())?;

    let mut out = String::with_capacity(content.len());
    let mut overwritten: Vec<&str> = Vec::new();
    // Splitting on '\n' rather than by the full line-terminator set is
    // deliberate: a lone '\r' here is an overwrite to resolve, not a line break.
    for (index, line) in content.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        // A trailing CR is a CRLF line ending rather than an overwrite, and has
        // to be held out and put back: resolving it would append the frame to a
        // screen it never touched.
        let body = line.trim_end_matches('\r');
        let ending = &line[body.len()..];
        if !body.contains('\r') {
            out.push_str(line);
            continue;
        }
        if body
            .split('\r')
            .any(|f| f.chars().count() > MAX_FRAME_CHARS)
        {
            out.push_str(line);
            continue;
        }
        let resolved = resolve_overwrites(body);
        if resolved != body {
            overwritten.push(line);
        }
        out.push_str(&resolved);
        out.push_str(ending);
    }

    if overwritten.is_empty() {
        return None;
    }
    Some((out, overwritten))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_overwrites_only_as_far_as_it_reaches() {
        // the tail of the longer frame beneath stays on screen
        assert_eq!(resolve_overwrites("aaaaaaaa\rbbb"), "bbbaaaaa");
        assert_eq!(resolve_overwrites("abc\rxyz"), "xyz");
    }

    #[test]
    fn keeping_only_the_last_frame_would_delete_the_payload() {
        // stdout interrupting a stderr meter: the response body is frame two,
        // and frame three is only 5 chars, so most of the body survives.
        let line = "  10% |=====\rTHE ENTIRE RESPONSE BODY THE AGENT ASKED FOR\r  20%";
        let resolved = resolve_overwrites(line);
        assert!(resolved.contains("RESPONSE BODY"), "got {resolved:?}");
        assert!(resolved.starts_with("  20%"));
    }

    #[test]
    fn a_line_carrying_an_oversized_frame_is_left_alone() {
        let big = "x".repeat(MAX_FRAME_CHARS + 1);
        let content = format!("  10%\r{big}\n");
        let (resolved, _) = collapse_redraws(&content).unwrap_or((content.clone(), vec![]));
        assert_eq!(
            resolved, content,
            "an oversized frame is payload, not a meter"
        );
    }

    #[test]
    fn a_trailing_cr_is_a_line_ending_not_an_overwrite() {
        let content = "alpha\r\nbeta\r\n";
        assert!(
            collapse_redraws(content).is_none(),
            "CRLF text carries no overwrite"
        );
    }

    #[test]
    fn content_without_carriage_returns_is_not_touched() {
        assert!(collapse_redraws("plain\nlines\n").is_none());
    }

    #[test]
    fn the_overwritten_lines_are_reported_verbatim_for_retention() {
        let content = "  0%\r 50%\r100%\ndone\n";
        let (resolved, overwritten) = collapse_redraws(content).unwrap();
        assert_eq!(resolved, "100%\ndone\n");
        assert_eq!(overwritten, vec!["  0%\r 50%\r100%"]);
    }

    #[test]
    fn resolving_is_idempotent() {
        let content = "  0%\r 50%\r100%\ndone\n";
        let (once, _) = collapse_redraws(content).unwrap();
        assert!(collapse_redraws(&once).is_none(), "nothing left to resolve");
    }

    #[test]
    fn resolving_respects_character_boundaries() {
        // three two-byte code points overwritten by two: one char of the old
        // frame remains, and the result must still be valid text
        let resolved = resolve_overwrites("ééé\rab");
        assert_eq!(resolved, "abé");
    }
}
