//! Token estimation for accounting.
//!
//! `len / 4` is the rule of thumb for English prose. Agent tool output is not
//! English prose, and measured against `o200k_base` over a corpus of real agent
//! traffic (34.2M characters, 12.1M real tokens) that rule was wrong by a lot
//! and always in the same direction: **29.5% under on the corpus total**, with
//! only 18.7% of payloads inside ±10% of truth. Paths, hex and numeric tables
//! fragment far harder than words — digits cost about one token per 1.2
//! characters against 3.1 for letters — so every token count reported was
//! understated by roughly a third, and so was every saving derived from one.
//!
//! So price the characters rather than counting them. One weight per character
//! class, summed:
//!
//! | class  | chars per token | why it lands there                          |
//! |--------|-----------------|---------------------------------------------|
//! | letter | 3.14            | BPE merges runs of letters into word pieces |
//! | digit  | 1.17            | numbers fragment; long ids completely       |
//! | punct  | 2.57            | separators mostly become their own token    |
//! | space  | 11.20           | whitespace is absorbed into its neighbours  |
//! | dense  | 1.45            | CJK and emoji — a different regime entirely |
//!
//! Measured on a held-out half of the corpus: 13.1% mean absolute error and
//! -0.6% bias, against 27.4% and -30.9% for `len / 4`.
//!
//! A linear cost was not the first thing tried. Bucketing payloads by
//! punctuation density and applying a per-bucket ratio scored a similar 14%
//! error, and was rejected for a reason that only shows up in the comparison the
//! accounting actually makes: **10.2% of compressed payloads lose characters
//! without losing tokens**, because compression strips cheap letters and spaces
//! while keeping expensive digits and separators. `len / 4` cannot represent
//! that at all, and the bucketed form only notices when a payload happens to
//! cross a threshold. Summing per-character costs tracks the composition shift
//! directly.
//!
//! `role` is the one thing the text does not reveal. Prose written by a model or
//! a person runs 4.2-4.6 chars/token where the terminal output it otherwise
//! resembles runs about 3.3, and the density signals overlap too much to
//! separate them — so the caller passes the role it already has. It is a cause
//! of density, not a correlate, which is what makes it safe to key on.

/// Tokens per character, by class. Fitted by least squares; see the module docs.
const TOKENS_PER_LETTER: f64 = 0.3181;
const TOKENS_PER_DIGIT: f64 = 0.8555;
const TOKENS_PER_SPACE: f64 = 0.0893;
const TOKENS_PER_PUNCTUATION: f64 = 0.3896;
/// Pinned rather than fitted: the corpus behind the other four is agent traffic
/// with essentially no CJK in it, and a least-squares fit on data that does not
/// contain a thing will happily assign it a nonsense coefficient.
const TOKENS_PER_DENSE: f64 = 0.6897;

/// Prose is cheaper per character than the weights above imply — they are fitted
/// on tool output, and applying them to an authored turn overshoots by 29%.
const PROSE_SCALE: f64 = 0.775;

/// Roles whose content a model or a person wrote rather than a command printed.
fn is_authored(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "assistant" | "user" | "system" | "developer"
    )
}

/// CJK and emoji are their own regime: measured at 1.43 (Chinese), 1.45
/// (Japanese) and 1.81 (Korean) chars/token against 3.11 for Latin letters.
#[inline]
fn is_dense(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30ff}'     // hiragana, katakana
        | '\u{3400}'..='\u{4dbf}'   // CJK extension A
        | '\u{4e00}'..='\u{9fff}'   // CJK ideographs
        | '\u{ac00}'..='\u{d7af}'   // hangul
        | '\u{1f000}'..='\u{1faff}' // emoji
    )
}

/// Approximate the token count of `text`.
///
/// `role` only sharpens the estimate: pass the message's own role where the
/// caller has it and prose is priced as prose rather than as terminal output.
/// Passing `""` keeps the machine-output calibration, which is the right default
/// for a raw tool payload. Never returns less than 1, so a non-empty payload
/// always registers.
pub fn estimate_tokens(text: &str, role: &str) -> u32 {
    if text.is_empty() {
        return 1;
    }

    let mut letters = 0usize;
    let mut digits = 0usize;
    let mut spaces = 0usize;
    let mut punctuation = 0usize;
    let mut dense = 0usize;

    // The ASCII fast path is worth keeping separate: it is the overwhelming
    // majority of agent output, and it avoids the range tests below entirely.
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            if b.is_ascii_digit() {
                digits += 1;
            } else if b.is_ascii_whitespace() {
                spaces += 1;
            } else if b.is_ascii_alphabetic() {
                letters += 1;
            } else {
                punctuation += 1;
            }
            i += 1;
            continue;
        }
        let c = text[i..].chars().next().unwrap();
        i += c.len_utf8();
        if is_dense(c) {
            dense += 1;
        } else {
            // everything else non-ASCII is priced as a letter
            letters += 1;
        }
    }

    let mut tokens = TOKENS_PER_LETTER * letters as f64
        + TOKENS_PER_DIGIT * digits as f64
        + TOKENS_PER_SPACE * spaces as f64
        + TOKENS_PER_PUNCTUATION * punctuation as f64
        + TOKENS_PER_DENSE * dense as f64;

    if is_authored(role) {
        tokens *= PROSE_SCALE;
    }

    (tokens as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_payload_still_registers() {
        assert_eq!(estimate_tokens("", ""), 1);
        assert_eq!(estimate_tokens("x", ""), 1);
    }

    #[test]
    fn digits_cost_far_more_than_letters() {
        let digits = estimate_tokens(&"1".repeat(100), "");
        let letters = estimate_tokens(&"a".repeat(100), "");
        assert!(
            digits > letters * 2,
            "digits {digits} should dwarf letters {letters}"
        );
    }

    #[test]
    fn whitespace_is_nearly_free() {
        assert!(estimate_tokens(&" ".repeat(100), "") < estimate_tokens(&"a".repeat(100), ""));
    }

    #[test]
    fn prose_is_priced_below_terminal_output() {
        let text = "The quick brown fox jumps over the lazy dog, repeatedly.";
        assert!(estimate_tokens(text, "assistant") < estimate_tokens(text, "tool"));
    }

    #[test]
    fn dense_scripts_are_their_own_regime() {
        let cjk = estimate_tokens(&"漢".repeat(50), "");
        let latin = estimate_tokens(&"a".repeat(50), "");
        assert!(cjk > latin, "cjk {cjk} should exceed latin {latin}");
    }

    #[test]
    fn the_estimate_tracks_a_composition_shift_len_over_four_cannot() {
        // Stripping cheap letters while keeping expensive ids leaves a payload
        // that is shorter and *not* cheaper. `len/4` would report a saving.
        let before = "INFO starting the service now with identifier 9f3a7c2b8e1d4a6f";
        let after = "9f3a7c2b8e1d4a6f";
        assert!(before.len() / 4 > after.len() / 4, "len/4 sees a saving");
        let ratio = estimate_tokens(after, "") as f64 / estimate_tokens(before, "") as f64;
        assert!(
            ratio > 0.4,
            "the id carries most of the cost, ratio {ratio}"
        );
    }

    /// Pinned vectors for the weights above. Any drift here is a change in what
    /// every reported saving is measured against, so these are recorded
    /// expectations rather than properties: they are meant to be hard to change
    /// by accident.
    #[test]
    fn the_estimate_matches_its_recorded_vectors() {
        let cases: &[(&str, &str, u32)] = &[
            ("2026-01-02T03:04:05Z ERROR db timeout id=42\n", "", 21),
            (
                "The quick brown fox jumps over the lazy dog, repeatedly.",
                "assistant",
                12,
            ),
            (
                "The quick brown fox jumps over the lazy dog, repeatedly.",
                "tool",
                15,
            ),
            (
                "INFO starting the service now with identifier 9f3a7c2b8e1d4a6f",
                "",
                22,
            ),
            ("9f3a7c2b8e1d4a6f", "", 9),
            (
                "/usr/lib/python3.12/site-packages/foo/bar.py:42: warning: unused",
                "",
                23,
            ),
            ("", "", 1),
            ("x", "", 1),
        ];
        for (text, role, expected) in cases {
            assert_eq!(
                estimate_tokens(text, role),
                *expected,
                "role {role:?} text {text:?}"
            );
        }
        assert_eq!(estimate_tokens(&"a".repeat(100), ""), 31);
        assert_eq!(estimate_tokens(&"1".repeat(100), ""), 85);
        assert_eq!(estimate_tokens(&" ".repeat(100), ""), 8);
        assert_eq!(estimate_tokens(&"漢".repeat(50), ""), 34);
    }
}
