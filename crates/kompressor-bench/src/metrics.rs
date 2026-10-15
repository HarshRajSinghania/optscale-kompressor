//! The aggregates a replay produces, and the invariants it refuses to trade.

use serde::{Deserialize, Serialize};

/// What a prompt-cache read costs relative to fresh input.
///
/// A tenth, which is the order of magnitude every provider charges. It is the
/// whole reason [`Billed`] exists: an agent re-sends its history every turn, so
/// a payload from turn 3 of a 40-turn task is billed 38 times as a cache read
/// and once at full rate. Weighting by the request count flat would make that
/// payload look 38 times as important as a payload seen once, when it is worth
/// about 4.7 times as much.
pub const CACHE_READ_RATE: f64 = 0.1;

/// Bytes in, bytes out, and the share removed.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub chars_in: u64,
    pub chars_out: u64,
    /// Percentage of `chars_in` removed, 0-100.
    pub pct: f64,
}

impl Coverage {
    /// Build from totals, treating an empty corpus as zero rather than NaN.
    pub fn new(chars_in: u64, chars_out: u64) -> Coverage {
        let pct = if chars_in == 0 {
            0.0
        } else {
            (chars_in - chars_out.min(chars_in)) as f64 * 100.0 / chars_in as f64
        };
        Coverage {
            chars_in,
            chars_out,
            pct,
        }
    }
}

/// Coverage weighted by how often each payload was actually sent.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Billed {
    /// Σ weight × chars_in.
    pub chars_in: f64,
    /// Σ weight × chars_out.
    pub chars_out: f64,
    pub pct: f64,
    /// The rate the weight was built with, recorded so a baseline comparison
    /// cannot silently mix two conventions.
    pub cache_read_rate: f64,
}

/// One routing bucket: a compressor that claimed payloads, or the detected type
/// of the payloads nothing claimed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BucketStats {
    pub payloads: usize,
    pub chars_in: u64,
    pub chars_out: u64,
    /// Percentage removed within this bucket.
    pub saved_pct: f64,
    /// Σ weight × chars_out for the bucket: what it costs over a conversation,
    /// which orders the buckets by what fixing them is worth rather than by how
    /// much of the corpus file they occupy.
    pub billed_out: f64,
}

/// Per-payload latency, and the throughput it implies.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Timing {
    /// Milliseconds per kilobyte of input across the whole run.
    pub ms_per_kb: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

/// A safety property that did not hold.
///
/// None of these is tradeable against compression ratio, so a replay that
/// reports one has failed however good its numbers are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// Compression made the payload bigger. The engine's first guarantee is
    /// that a compressor which cannot shrink a payload returns it untouched.
    LongerThanInput { chars_in: usize, chars_out: usize },

    /// The payload was declined and came back changed anyway. A decline has to
    /// be byte-identical, or "we left it alone" is not a statement a caller can
    /// rely on — and the change is unaccounted by definition, since nothing
    /// claimed the payload to account for it.
    MutatedWhileDeclining,

    /// A fragment was retained under a handle the output never names: a write
    /// nothing can read. It costs store bytes and a TTL, and the model has no
    /// way to ask for it.
    OrphanedRef { handle: String },

    /// Handles were minted but the output carries no marker at all.
    ///
    /// Implied by [`Violation::OrphanedRef`] while a bare handle counts as a
    /// marker, and kept separate because that is a property of the marker
    /// vocabulary rather than of this check: if the vocabulary ever changes,
    /// this one still catches retention that announces nothing.
    RefsWithoutMarker { refs: usize },

    /// A handle was minted for nothing. Retrieving it returns an empty string,
    /// which reads to a model as "the data was empty" rather than "the data was
    /// never there".
    EmptyRefFragment { handle: String },

    /// A handle-free `[×N]` fold claims a repeat the input does not contain.
    ///
    /// The bare form asserts something stronger than an omission marker does:
    /// not "content was removed here" but "this exact line occurred N times",
    /// which is the whole reason it needs no handle. A fold that drops lines
    /// the count cannot reconstruct has to keep one.
    ///
    /// The line numbers a `grep -n` gutter carries are not part of the claim:
    /// `lift_gutter` strips the column before anything folds and re-attaches it
    /// to the lines that survive, so the count is over the stripped bodies.
    /// This check compares against both views for that reason.
    FoldCountUnsupported {
        line: String,
        claimed: usize,
        found: usize,
    },

    /// Re-compressing the same bytes produced different bytes. Most billed
    /// input on agent traffic is a prompt-cache read, so instability here costs
    /// more every turn than any ratio win pays back once.
    Nondeterministic,

    /// The output changed when the preceding user turn changed.
    ///
    /// Ranking a payload against the question that prompted it is the obvious
    /// way to compress harder and it is forbidden for the same reason: the same
    /// tool output then has different bytes on every turn it survives into.
    QueryDependent,
}

impl Violation {
    /// Stable name, used as the key in reports and baselines.
    pub fn name(&self) -> &'static str {
        match self {
            Violation::LongerThanInput { .. } => "longer_than_input",
            Violation::MutatedWhileDeclining => "mutated_while_declining",
            Violation::FoldCountUnsupported { .. } => "fold_count_unsupported",
            Violation::OrphanedRef { .. } => "orphaned_ref",
            Violation::RefsWithoutMarker { .. } => "refs_without_marker",
            Violation::EmptyRefFragment { .. } => "empty_ref_fragment",
            Violation::Nondeterministic => "nondeterministic",
            Violation::QueryDependent => "query_dependent",
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::LongerThanInput {
                chars_in,
                chars_out,
            } => write!(f, "longer_than_input ({chars_in} -> {chars_out} chars)"),
            Violation::FoldCountUnsupported {
                line,
                claimed,
                found,
            } => write!(
                f,
                "fold_count_unsupported (claimed x{claimed}, found {found}): {line:?}"
            ),
            Violation::OrphanedRef { handle } => write!(f, "orphaned_ref ({handle})"),
            Violation::RefsWithoutMarker { refs } => {
                write!(f, "refs_without_marker ({refs} retained)")
            }
            Violation::EmptyRefFragment { handle } => write!(f, "empty_ref_fragment ({handle})"),
            other => f.write_str(other.name()),
        }
    }
}

/// The p-th percentile of `sorted`, by nearest rank.
///
/// Nearest rank rather than interpolation because these are latencies of real
/// payloads: an interpolated p95 is a number no payload took, and the question
/// being asked is "how slow is a slow payload", not "where does the curve sit".
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Round to four decimals, so a recorded baseline diffs readably and a
/// comparison is not decided by the last bit of a float.
pub fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_of_an_empty_corpus_is_zero_rather_than_not_a_number() {
        let coverage = Coverage::new(0, 0);
        assert_eq!(coverage.pct, 0.0);
    }

    #[test]
    fn coverage_is_the_share_of_input_removed() {
        assert_eq!(Coverage::new(1000, 250).pct, 75.0);
    }

    #[test]
    fn a_percentile_is_a_value_some_payload_actually_took() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert_eq!(percentile(&sorted, 50.0), 3.0);
        assert_eq!(percentile(&sorted, 95.0), 100.0);
        assert_eq!(percentile(&sorted, 0.0), 1.0);
    }
}
