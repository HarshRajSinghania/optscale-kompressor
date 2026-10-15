//! Deterministic, bounded budget selection: how many items to keep.
//!
//! Every sampling compressor asks the same question — of `n` items, how many are
//! enough to represent the payload? A fixed count is wrong in both directions: a
//! run of near-identical rows needs three, and a heterogeneous dump needs
//! dozens. The answer here is driven by how fast *new information* accumulates
//! as items are added, which is cheap to compute and stable across runs.
//!
//! Stability is the constraint that shapes the implementation. An agent re-sends
//! its history every turn, so a budget that moved between identical runs would
//! rewrite markers and burst the provider's prompt cache — which costs more than
//! any sampling win. Hence a keyed digest rather than the standard library's
//! hasher, which is randomised per process.

use std::collections::HashSet;

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use once_cell::sync::Lazy;
use regex::Regex;
use std::io::Write;

static WORDS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\w+").unwrap());

fn tokens(text: &str) -> Vec<String> {
    WORDS
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str().to_string())
        .collect()
}

/// A stable 64-bit SimHash. Never the process-randomised default hasher.
pub fn simhash(text: &str) -> u64 {
    let mut bits = [0i32; 64];
    let toks = tokens(text);
    let iter: Vec<&str> = if toks.is_empty() {
        vec![""]
    } else {
        toks.iter().map(|s| s.as_str()).collect()
    };
    for token in iter {
        let mut hasher = Blake2bVar::new(8).expect("8 is a valid digest size");
        hasher.update(token.as_bytes());
        let mut digest = [0u8; 8];
        hasher.finalize_variable(&mut digest).expect("8 bytes out");
        let value = u64::from_be_bytes(digest);
        for (bit, slot) in bits.iter_mut().enumerate() {
            *slot += if value & (1u64 << bit) != 0 { 1 } else { -1 };
        }
    }
    bits.iter()
        .enumerate()
        .filter(|(_, &score)| score >= 0)
        .fold(0u64, |acc, (bit, _)| acc | (1u64 << bit))
}

#[inline]
pub fn hamming(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

/// Two items are the same item for budgeting purposes within this distance.
const NEAR_DUPLICATE: u32 = 3;
/// How far back the near-duplicate check looks. Bounds the comparison to a
/// window rather than the whole payload, which is what keeps this linear.
const FINGERPRINT_WINDOW: usize = 256;

/// Choose a representative budget for `items` using bigram coverage.
///
/// The function is deliberately bounded: every item is compared only against a
/// small recent window, and zlib is consulted only as a final increase signal.
pub fn adaptive_k(items: &[&str], minimum: usize, maximum: usize, bias: f32) -> usize {
    let n = items.len();
    if n <= 8 {
        return n.min(maximum);
    }
    let maximum = maximum.min(n);

    // Tier 1 — how many genuinely distinct items are there at all?
    let mut fingerprints: Vec<u64> = Vec::new();
    for item in items {
        let fingerprint = simhash(item);
        let start = fingerprints.len().saturating_sub(FINGERPRINT_WINDOW);
        let near = fingerprints[start..]
            .iter()
            .any(|&prior| hamming(fingerprint, prior) <= NEAR_DUPLICATE);
        if !near {
            fingerprints.push(fingerprint);
        }
    }
    if fingerprints.len() <= 3 {
        return fingerprints.len().max(minimum).min(maximum);
    }

    // Tier 2 — the knee of the cumulative unique-bigram curve: the point past
    // which each additional item is mostly repeating what earlier ones said.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut coverage: Vec<usize> = Vec::with_capacity(n);
    for item in items {
        let toks = tokens(item);
        if toks.is_empty() {
            coverage.push(seen.len());
            continue;
        }
        if toks.len() == 1 {
            seen.insert((hash_token(&toks[0]), hash_token("")));
        } else {
            for pair in toks.windows(2) {
                seen.insert((hash_token(&pair[0]), hash_token(&pair[1])));
            }
        }
        coverage.push(seen.len());
    }
    let total = (*coverage.last().unwrap_or(&0)).max(1) as f64;

    let mut best = minimum;
    let mut distance = f64::NEG_INFINITY;
    for index in minimum..=maximum {
        if index == 0 || index > coverage.len() {
            continue;
        }
        let x = index as f64 / maximum as f64;
        let y = coverage[index - 1] as f64 / total;
        let candidate = y - x;
        if candidate > distance {
            best = index;
            distance = candidate;
        }
    }

    let scaled = (best as f64 * bias.max(1.0) as f64).round() as usize;
    let mut best = scaled.max(minimum).min(maximum);

    // Tier 3 — a sanity check on the chosen prefix. If the whole payload
    // compresses to more than twice what the prefix does, the prefix is
    // materially less varied than the payload and one more item is cheap.
    if best < maximum {
        let prefix = items[..best].join("\n");
        let whole = items.join("\n");
        if deflated_len(&whole) > deflated_len(&prefix) * 2 {
            best += 1;
        }
    }
    best.min(maximum)
}

fn hash_token(token: &str) -> u64 {
    let mut hasher = Blake2bVar::new(8).expect("8 is a valid digest size");
    hasher.update(token.as_bytes());
    let mut digest = [0u8; 8];
    hasher.finalize_variable(&mut digest).expect("8 bytes out");
    u64::from_be_bytes(digest)
}

fn deflated_len(text: &str) -> usize {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    if encoder.write_all(text.as_bytes()).is_err() {
        return text.len();
    }
    encoder.finish().map(|v| v.len()).unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_list_is_kept_whole() {
        let items = ["a", "b", "c"];
        assert_eq!(adaptive_k(&items, 3, 10, 1.0), 3);
    }

    #[test]
    fn a_uniform_payload_collapses_to_a_few_representatives() {
        let owned: Vec<String> = (0..200).map(|_| "status ok".to_string()).collect();
        let items: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let k = adaptive_k(&items, 3, 30, 1.0);
        assert!(k <= 5, "uniform items should need very few, got {k}");
    }

    #[test]
    fn a_varied_payload_keeps_more_than_a_uniform_one() {
        let uniform: Vec<String> = (0..200).map(|_| "status ok".to_string()).collect();
        let varied: Vec<String> = (0..200)
            .map(|i| format!("event {i} kind-{i} subject-{i} outcome-{i}"))
            .collect();
        let u: Vec<&str> = uniform.iter().map(|s| s.as_str()).collect();
        let v: Vec<&str> = varied.iter().map(|s| s.as_str()).collect();
        assert!(adaptive_k(&v, 3, 30, 1.0) > adaptive_k(&u, 3, 30, 1.0));
    }

    #[test]
    fn the_budget_respects_its_bounds() {
        let owned: Vec<String> = (0..500).map(|i| format!("line {i} of many")).collect();
        let items: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let k = adaptive_k(&items, 10, 100, 1.0);
        assert!((10..=100).contains(&k), "k {k} outside bounds");
    }

    #[test]
    fn the_budget_is_stable_across_runs() {
        // The whole point: a budget that moved between identical runs would
        // rewrite markers every turn and burst the provider prompt cache.
        let owned: Vec<String> = (0..300)
            .map(|i| format!("item {i} value {}", i * 7))
            .collect();
        let items: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let first = adaptive_k(&items, 3, 30, 1.0);
        for _ in 0..5 {
            assert_eq!(adaptive_k(&items, 3, 30, 1.0), first);
        }
    }

    #[test]
    fn simhash_is_stable_and_discriminating() {
        assert_eq!(simhash("the same text"), simhash("the same text"));
        assert!(hamming(simhash("alpha beta gamma"), simhash("zulu yankee xray")) > 3);
    }

    #[test]
    fn near_duplicates_collapse_under_the_hamming_window() {
        let a = simhash("connection refused to host db-1 after 30 seconds");
        let b = simhash("connection refused to host db-1 after 30 seconds");
        assert_eq!(hamming(a, b), 0);
    }

    #[test]
    fn a_bias_above_one_keeps_more() {
        let owned: Vec<String> = (0..200)
            .map(|i| format!("row {i} data {}", i % 17))
            .collect();
        let items: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let plain = adaptive_k(&items, 3, 60, 1.0);
        let biased = adaptive_k(&items, 3, 60, 2.0);
        assert!(biased >= plain);
    }
}
