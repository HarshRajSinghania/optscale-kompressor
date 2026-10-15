//! Request identifiers.
//!
//! The id is what joins a log line to a stats row to a retrieval three turns
//! later, so it has to be unique, cheap, and sortable by time — sortable
//! because the first thing anyone does with a pile of ids is put them in order.
//! That is a ULID, and it is short enough to write here rather than to take a
//! dependency for.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford base32: no I, L, O or U, so an id read aloud or copied by hand
/// cannot be confused with a different one.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Longest inbound id we will carry.
const MAX_INBOUND: usize = 128;

/// A fresh identifier: 48 bits of millisecond timestamp, then 80 bits that make
/// it unique.
///
/// Those 80 bits are a per-process seed followed by a counter rather than fresh
/// randomness per call. Within a process that makes ids issued in the same
/// millisecond strictly increasing — two requests that arrive together stay in
/// the order they arrived when someone sorts the log — and across processes the
/// seed keeps two servers started in the same millisecond apart.
pub fn new_request_id() -> String {
    static SEED: OnceLock<u64> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        & 0xFFFF_FFFF_FFFF;
    let seed = *SEED.get_or_init(process_seed) & 0xFFFF;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let value = (u128::from(ms) << 80) | (u128::from(seed) << 64) | u128::from(counter);
    encode(value)
}

/// Accept an inbound `X-Request-Id`, or refuse it.
///
/// The id is written into log lines and into a stats row, so a caller that can
/// put a newline or an ANSI escape in it can forge log entries in whatever
/// reads them. Anything outside a conservative set is refused rather than
/// escaped, and the caller gets a generated id instead — it loses its
/// correlation, which is the caller's problem, not the operator's.
pub fn sanitize(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_INBOUND {
        return None;
    }
    let acceptable = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '+' | '/' | '='));
    acceptable.then(|| trimmed.to_string())
}

fn process_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // A cheap avalanche over the two things that differ between processes
    // started at the same moment; this is a collision guard, not a secret.
    let mut x = nanos ^ (u64::from(std::process::id()) << 32);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x
}

fn encode(value: u128) -> String {
    let mut out = [0u8; 26];
    let mut remaining = value;
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(remaining & 31) as usize];
        remaining >>= 5;
    }
    // Every byte came from ALPHABET, which is ASCII.
    String::from_utf8(out.to_vec()).unwrap_or_else(|_| "0".repeat(26))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_twenty_six_characters_of_the_crockford_alphabet() {
        let id = new_request_id();
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn ids_issued_in_the_same_millisecond_still_sort_in_issue_order() {
        let ids: Vec<String> = (0..1000).map(|_| new_request_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "the counter breaks ties inside a millisecond");
        sorted.dedup();
        assert_eq!(sorted.len(), 1000);
    }

    #[test]
    fn an_inbound_id_that_could_forge_a_log_line_is_refused() {
        assert_eq!(sanitize("req-42"), Some("req-42".to_string()));
        assert_eq!(sanitize("  req-42  "), Some("req-42".to_string()));
        assert_eq!(sanitize("req\n2026-01-01 INFO fake line"), None);
        assert_eq!(sanitize("\u{1b}[31mred"), None);
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize(&"x".repeat(MAX_INBOUND + 1)), None);
    }
}
