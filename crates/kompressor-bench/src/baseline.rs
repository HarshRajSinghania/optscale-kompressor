//! Recording a run, and comparing a run against a recorded one.
//!
//! A baseline is what makes the harness a gate rather than a readout. It is
//! also the thing most likely to go stale, so it carries [`BASELINE_FORMAT`]
//! and a mismatch is refused outright — reading an old file and defaulting the
//! keys it does not have compares this run against zeros, which reports every
//! metric added since as a free improvement.
//!
//! `fixtures/baseline.json` ships a recorded run in this format, so a replay can
//! be measured against a fixed floor rather than against itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::corpus::BenchError;
use crate::metrics::{round4, Timing};
use crate::replay::ReplayReport;

/// The version of the recorded shape. Bump it when a field changes meaning.
pub const BASELINE_FORMAT: u32 = 4;

/// One recorded run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub format: u32,
    /// Share of eligible bytes removed, 0-100.
    pub coverage_pct: f64,
    /// Share of eligible bytes some compressor claimed, 0-100.
    pub claimed_pct: f64,
    pub payloads: usize,
    pub chars_in: u64,
    pub chars_out: u64,
    /// Absent when the corpus recorded no request counts.
    #[serde(default)]
    pub billed: Option<BilledBaseline>,
    #[serde(default)]
    pub buckets: BTreeMap<String, BucketBaseline>,
    /// `content_id` to `[bucket, chars_in, chars_out]`.
    ///
    /// This is what makes a stable-membership bucket comparison possible:
    /// without knowing which payload was in which bucket, two runs can only be
    /// compared on bucket averages, and those move when payloads move.
    #[serde(default)]
    pub per_payload: BTreeMap<String, PerPayload>,
    #[serde(default)]
    pub query_dependent: QueryDependent,
    #[serde(default)]
    pub timing: Timing,
    /// Violation name to the number of payloads that hit it. A recorded
    /// baseline should have this empty; it is recorded so a file with entries
    /// is visibly not a clean run.
    #[serde(default)]
    pub violations: BTreeMap<String, u64>,
}

/// The billed view.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BilledBaseline {
    pub coverage_pct: f64,
    pub chars_in: f64,
    pub chars_out: f64,
    pub cache_read_rate: f64,
}

/// One bucket's totals.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BucketBaseline {
    pub payloads: usize,
    pub chars_in: u64,
    pub chars_out: u64,
    pub saved_pct: f64,
    #[serde(default)]
    pub billed_out: f64,
}

/// `[bucket, chars_in, chars_out]`, as an array so the file stays compact at
/// one line per payload.
pub type PerPayload = (String, usize, usize);

/// Payloads whose output moved with the user turn that preceded them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryDependent {
    pub payloads: usize,
    pub chars_in: u64,
    pub ids: Vec<String>,
}

impl ReplayReport {
    /// This run, in recordable form.
    pub fn to_baseline(&self) -> Baseline {
        let coverage = self.coverage();
        let query_dependent = self.query_dependent();
        Baseline {
            format: BASELINE_FORMAT,
            coverage_pct: round4(coverage.pct),
            claimed_pct: round4(self.claimed_pct()),
            payloads: self.results.len(),
            chars_in: coverage.chars_in,
            chars_out: coverage.chars_out,
            billed: self.billed().map(|b| BilledBaseline {
                coverage_pct: round4(b.pct),
                chars_in: round4(b.chars_in),
                chars_out: round4(b.chars_out),
                cache_read_rate: b.cache_read_rate,
            }),
            buckets: self
                .buckets()
                .into_iter()
                .map(|(name, stats)| {
                    (
                        name,
                        BucketBaseline {
                            payloads: stats.payloads,
                            chars_in: stats.chars_in,
                            chars_out: stats.chars_out,
                            saved_pct: round4(stats.saved_pct),
                            billed_out: round4(stats.billed_out),
                        },
                    )
                })
                .collect(),
            per_payload: self
                .results
                .iter()
                .map(|r| {
                    (
                        r.content_id.clone(),
                        (r.bucket.clone(), r.chars_in, r.chars_out),
                    )
                })
                .collect(),
            query_dependent: QueryDependent {
                payloads: query_dependent.len(),
                chars_in: query_dependent.iter().map(|r| r.chars_in as u64).sum(),
                ids: query_dependent
                    .iter()
                    .map(|r| r.content_id.clone())
                    .collect(),
            },
            timing: Timing {
                ms_per_kb: round4(self.timing().ms_per_kb),
                p50_ms: round4(self.timing().p50_ms),
                p95_ms: round4(self.timing().p95_ms),
                max_ms: round4(self.timing().max_ms),
                mean_ms: round4(self.timing().mean_ms),
            },
            violations: self.violations(),
        }
    }
}

/// Write `report` to `path` as a baseline.
pub fn save_baseline(report: &ReplayReport, path: &Path) -> Result<(), BenchError> {
    let baseline = report.to_baseline();
    let text = serde_json::to_string_pretty(&baseline).map_err(|e| BenchError::Json {
        path: path.display().to_string(),
        source: e,
    })?;
    std::fs::write(path, text + "\n").map_err(|e| BenchError::Write(path.display().to_string(), e))
}

/// Read a baseline, refusing one written in another format.
pub fn load_baseline(path: &Path) -> Result<Baseline, BenchError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| BenchError::Read(display.clone(), e))?;

    // The format is read on its own first, so a file from another version
    // reports its version rather than a deserialization error about whichever
    // field happened to change.
    #[derive(Deserialize)]
    struct FormatOnly {
        #[serde(default)]
        format: u32,
    }
    let probe: FormatOnly = serde_json::from_str(&text).map_err(|e| BenchError::Json {
        path: display.clone(),
        source: e,
    })?;
    if probe.format != BASELINE_FORMAT {
        return Err(BenchError::BaselineFormat {
            path: display,
            found: probe.format,
            expected: BASELINE_FORMAT,
        });
    }

    serde_json::from_str(&text).map_err(|e| BenchError::Json {
        path: display,
        source: e,
    })
}

/// How much a metric may fall before it counts as a regression.
#[derive(Clone, Copy, Debug)]
pub struct Tolerance {
    /// Percentage points a headline metric may lose.
    ///
    /// Not zero: coverage is a ratio of integers over hundreds of payloads and
    /// an unrelated refactor can move the last decimal. Small enough that a
    /// real regression cannot hide under it — the smallest change worth
    /// shipping moves coverage by whole points.
    pub headline_pct: f64,
    /// Percentage points a bucket may lose, measured over stable membership.
    ///
    /// Looser than the headline: a bucket can hold two payloads, where one
    /// compressor tweak moves its average several points without meaning
    /// anything for the corpus.
    pub bucket_pct: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Tolerance {
            headline_pct: 0.05,
            bucket_pct: 0.5,
        }
    }
}

/// One metric, before and after.
#[derive(Clone, Debug, Serialize)]
pub struct MetricDelta {
    pub name: String,
    pub baseline: f64,
    pub current: f64,
    /// `current - baseline`; negative is worse for every metric here.
    pub delta: f64,
    pub regressed: bool,
}

/// One bucket, compared over the payloads that were in it both times.
#[derive(Clone, Debug, Serialize)]
pub struct BucketDelta {
    pub name: String,
    /// Payloads in this bucket before *and* after. The comparison says nothing
    /// about the others.
    pub stable_payloads: usize,
    pub baseline_pct: f64,
    pub current_pct: f64,
    pub delta: f64,
    pub regressed: bool,
}

/// A payload that changed bucket between the two runs.
#[derive(Clone, Debug, Serialize)]
pub struct MembershipChange {
    pub content_id: String,
    pub from: String,
    pub to: String,
    pub chars_in: usize,
    /// Percentage points this payload itself gained. A move is an improvement
    /// when this is positive, whatever it did to either bucket's average.
    pub own_delta: f64,
}

/// The verdict.
#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    /// False when anything regressed beyond tolerance, or any invariant broke.
    pub passed: bool,
    pub headline: Vec<MetricDelta>,
    /// Buckets with stable membership, worst first.
    pub buckets: Vec<BucketDelta>,
    /// Payloads that moved between buckets, largest first.
    pub membership: Vec<MembershipChange>,
    /// Violations in the current run. Any entry fails the comparison.
    pub violations: BTreeMap<String, u64>,
    /// What could not be compared, and why.
    pub notes: Vec<String>,
    pub regressions: Vec<String>,
}

/// Compare `report` against `baseline`.
///
/// Headline metrics are compared directly. Buckets are compared over **stable
/// membership only**: the payloads that were in the bucket before and are in it
/// after. Comparing bucket averages wholesale reports an improvement as a
/// regression — adding a grammar moves payloads out of `untouched:source_code`
/// into `source_code`, where they compress worse than the payloads already
/// there, so `source_code` average falls, `untouched:source_code` average
/// rises, the corpus improves and not one payload got worse. Moves are listed
/// in [`Comparison::membership`] with what each payload itself gained, which is
/// the honest way to read them.
pub fn compare_baseline(
    report: &ReplayReport,
    baseline: &Baseline,
    tolerance: Tolerance,
) -> Comparison {
    let mut headline = Vec::new();
    let mut notes = Vec::new();
    let mut regressions = Vec::new();

    let coverage = report.coverage();
    let mut metric = |name: &str, before: f64, after: f64, regressions: &mut Vec<String>| {
        let delta = after - before;
        let regressed = delta < -tolerance.headline_pct;
        if regressed {
            regressions.push(format!("{name} {before:.4} -> {after:.4} ({delta:+.4})"));
        }
        headline.push(MetricDelta {
            name: name.to_string(),
            baseline: round4(before),
            current: round4(after),
            delta: round4(delta),
            regressed,
        });
    };
    metric(
        "coverage_pct",
        baseline.coverage_pct,
        coverage.pct,
        &mut regressions,
    );
    metric(
        "claimed_pct",
        baseline.claimed_pct,
        report.claimed_pct(),
        &mut regressions,
    );
    match (baseline.billed, report.billed()) {
        (Some(before), Some(after)) => {
            if (before.cache_read_rate - after.cache_read_rate).abs() > f64::EPSILON {
                notes.push(format!(
                    "billed rates differ: baseline {} vs current {}; the two are not comparable",
                    before.cache_read_rate, after.cache_read_rate
                ));
            }
            metric(
                "billed_pct",
                before.coverage_pct,
                after.pct,
                &mut regressions,
            );
        }
        (Some(_), None) => notes.push(
            "billed: this corpus records no request counts, so the baseline's billed coverage \
             cannot be checked"
                .to_string(),
        ),
        (None, Some(_)) => {
            notes.push("billed: the baseline recorded none, so there is nothing to compare".into())
        }
        (None, None) => {}
    }

    let (buckets, membership, bucket_notes) = compare_buckets(report, baseline, tolerance);
    notes.extend(bucket_notes);
    for bucket in buckets.iter().filter(|b| b.regressed) {
        regressions.push(format!(
            "bucket {} {:.4} -> {:.4} ({:+.4}) over {} stable payloads",
            bucket.name,
            bucket.baseline_pct,
            bucket.current_pct,
            bucket.delta,
            bucket.stable_payloads
        ));
    }

    let violations = report.violations();
    for (name, count) in &violations {
        regressions.push(format!("invariant {name} broken on {count} payload(s)"));
    }
    if !report.determinism_verified {
        notes.push(
            "determinism and query stability were not verified in this run (--check-determinism)"
                .to_string(),
        );
    }

    Comparison {
        passed: regressions.is_empty(),
        headline,
        buckets,
        membership,
        violations,
        notes,
        regressions,
    }
}

/// Bucket deltas over stable membership, plus the payloads that moved.
fn compare_buckets(
    report: &ReplayReport,
    baseline: &Baseline,
    tolerance: Tolerance,
) -> (Vec<BucketDelta>, Vec<MembershipChange>, Vec<String>) {
    let mut notes = Vec::new();
    if baseline.per_payload.is_empty() {
        notes.push(
            "the baseline records no per-payload buckets, so buckets cannot be compared on stable \
             membership and are not compared at all"
                .to_string(),
        );
        return (Vec::new(), Vec::new(), notes);
    }

    let mut stable: BTreeMap<String, (u64, u64, u64, u64, usize)> = BTreeMap::new();
    let mut membership = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    for result in report.results.iter().filter(|r| r.eligible) {
        seen.insert(result.content_id.as_str());
        let Some((was_bucket, was_in, was_out)) = baseline.per_payload.get(&result.content_id)
        else {
            continue;
        };
        if was_bucket == &result.bucket {
            let entry = stable.entry(result.bucket.clone()).or_default();
            entry.0 += *was_in as u64;
            entry.1 += *was_out as u64;
            entry.2 += result.chars_in as u64;
            entry.3 += result.chars_out as u64;
            entry.4 += 1;
        } else {
            let before = saved_pct(*was_in as u64, *was_out as u64);
            let after = saved_pct(result.chars_in as u64, result.chars_out as u64);
            membership.push(MembershipChange {
                content_id: result.content_id.clone(),
                from: was_bucket.clone(),
                to: result.bucket.clone(),
                chars_in: result.chars_in,
                own_delta: round4(after - before),
            });
        }
    }

    let missing = baseline
        .per_payload
        .keys()
        .filter(|id| !seen.contains(id.as_str()))
        .count();
    if missing > 0 {
        notes.push(format!(
            "{missing} payload(s) in the baseline are not in this corpus; they are excluded from \
             every bucket comparison"
        ));
    }

    let mut buckets: Vec<BucketDelta> = stable
        .into_iter()
        .map(|(name, (was_in, was_out, now_in, now_out, payloads))| {
            let baseline_pct = saved_pct(was_in, was_out);
            let current_pct = saved_pct(now_in, now_out);
            let delta = current_pct - baseline_pct;
            BucketDelta {
                name,
                stable_payloads: payloads,
                baseline_pct: round4(baseline_pct),
                current_pct: round4(current_pct),
                delta: round4(delta),
                regressed: delta < -tolerance.bucket_pct,
            }
        })
        .collect();
    buckets.sort_by(|a, b| a.delta.partial_cmp(&b.delta).expect("no NaN in a delta"));
    membership.sort_by_key(|entry| std::cmp::Reverse(entry.chars_in));

    (buckets, membership, notes)
}

/// Percentage of `chars_in` removed.
fn saved_pct(chars_in: u64, chars_out: u64) -> f64 {
    if chars_in == 0 {
        0.0
    } else {
        (chars_in - chars_out.min(chars_in)) as f64 * 100.0 / chars_in as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::COMMITTED_CORPUS_PAYLOADS;
    use crate::metrics::Violation;
    use crate::replay::PayloadResult;
    use kompressor_engine::options::EngineOptions;
    use kompressor_engine::types::{ContentType, Kind};

    /// A result with everything but the numbers held constant.
    fn result(id: &str, bucket: &str, chars_in: usize, chars_out: usize) -> PayloadResult {
        PayloadResult {
            content_id: id.to_string(),
            bucket: bucket.to_string(),
            detected_as: ContentType::PlainText,
            kind: (!bucket.starts_with("untouched:")).then_some(Kind::Text),
            eligible: true,
            chars_in,
            chars_out,
            elapsed_ms: 1.0,
            refs: 0,
            ref_bytes: 0,
            violations: Vec::new(),
            requests: 1,
        }
    }

    fn report(results: Vec<PayloadResult>) -> ReplayReport {
        ReplayReport {
            results,
            options: EngineOptions::default(),
            determinism_verified: true,
        }
    }

    #[test]
    fn a_baseline_from_another_format_is_rejected_rather_than_read_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        std::fs::write(&path, "{\"format\": 3, \"coverage_pct\": 40.0}").unwrap();
        match load_baseline(&path).unwrap_err() {
            BenchError::BaselineFormat {
                found, expected, ..
            } => {
                assert_eq!(found, 3);
                assert_eq!(expected, BASELINE_FORMAT);
            }
            other => panic!("expected a format refusal, got {other}"),
        }
    }

    #[test]
    fn a_baseline_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let report = report(vec![result("a", "log", 1000, 250)]);
        save_baseline(&report, &path).unwrap();

        let loaded = load_baseline(&path).unwrap();
        assert_eq!(loaded.format, BASELINE_FORMAT);
        assert_eq!(loaded.coverage_pct, 75.0);
        assert_eq!(loaded.per_payload["a"].0, "log");
    }

    #[test]
    fn the_recorded_reference_baseline_is_readable_and_compares() {
        // The recorded floor shipped in fixtures/: whatever it currently holds,
        // this exact file has to load and has to cover the committed corpus
        // payload for payload. Pinning the count to the corpus rather than to a
        // literal is the point — a baseline recorded against a different corpus
        // is not a floor, it is a coincidence.
        let path = std::path::Path::new("../../fixtures/baseline.json");
        let baseline = load_baseline(path).expect("the recorded reference baseline loads");
        assert_eq!(baseline.payloads, COMMITTED_CORPUS_PAYLOADS);
        assert!(baseline.coverage_pct > 0.0);
        assert!(baseline.billed.is_some());
        assert_eq!(baseline.per_payload.len(), COMMITTED_CORPUS_PAYLOADS);

        // and a run of the same shape compares against it without panicking
        let current = report(vec![result("01e36758efab39f4", "log", 1695, 1216)]);
        let comparison = compare_baseline(&current, &baseline, Tolerance::default());
        assert!(comparison
            .notes
            .iter()
            .any(|n| n.contains("not in this corpus")));
    }

    #[test]
    fn a_pure_mix_shift_is_not_reported_as_a_bucket_regression() {
        // Before: two source files compress 60%, and a third payload nothing
        // claimed. After: a grammar lands and claims the third at 20%. The
        // `source_code` average falls from 60% to ~46% and every payload is at
        // least as good as it was.
        let mut baseline = report(vec![
            result("a", "source_code", 1000, 400),
            result("b", "source_code", 1000, 400),
            result("c", "untouched:source_code", 1000, 1000),
        ])
        .to_baseline();
        baseline.format = BASELINE_FORMAT;

        let current = report(vec![
            result("a", "source_code", 1000, 400),
            result("b", "source_code", 1000, 400),
            result("c", "source_code", 1000, 800),
        ]);

        let comparison = compare_baseline(&current, &baseline, Tolerance::default());
        assert!(
            comparison.passed,
            "a mix shift that improves the corpus must pass: {:?}",
            comparison.regressions
        );
        let source = comparison
            .buckets
            .iter()
            .find(|b| b.name == "source_code")
            .expect("the bucket has stable members");
        assert_eq!(source.stable_payloads, 2, "c is excluded: it moved");
        assert_eq!(source.delta, 0.0);
        assert_eq!(comparison.membership.len(), 1);
        assert_eq!(comparison.membership[0].own_delta, 20.0);
        assert!(comparison.headline.iter().all(|m| !m.regressed));
    }

    #[test]
    fn a_real_bucket_regression_fails_the_comparison() {
        let baseline = report(vec![result("a", "log", 1000, 200)]).to_baseline();
        let current = report(vec![result("a", "log", 1000, 600)]);
        let comparison = compare_baseline(&current, &baseline, Tolerance::default());
        assert!(!comparison.passed);
        assert!(comparison
            .regressions
            .iter()
            .any(|r| r.contains("coverage_pct")));
        assert!(comparison
            .regressions
            .iter()
            .any(|r| r.contains("bucket log")));
    }

    #[test]
    fn a_broken_invariant_fails_a_comparison_whose_numbers_improved() {
        let baseline = report(vec![result("a", "log", 1000, 600)]).to_baseline();
        let mut better = result("a", "log", 1000, 200);
        better.violations.push(Violation::Nondeterministic);
        let comparison = compare_baseline(&report(vec![better]), &baseline, Tolerance::default());
        assert!(
            !comparison.passed,
            "safety is not tradeable against ratio: {:?}",
            comparison.headline
        );
        assert_eq!(comparison.violations["nondeterministic"], 1);
    }

    #[test]
    fn a_corpus_without_request_counts_does_not_fail_a_billed_baseline() {
        let baseline = report(vec![result("a", "log", 1000, 200)]).to_baseline();
        assert!(baseline.billed.is_some());

        let mut unmeasured = result("a", "log", 1000, 200);
        unmeasured.requests = 0;
        let comparison =
            compare_baseline(&report(vec![unmeasured]), &baseline, Tolerance::default());
        assert!(comparison.passed);
        assert!(comparison.notes.iter().any(|n| n.starts_with("billed:")));
        assert!(comparison.headline.iter().all(|m| m.name != "billed_pct"));
    }
}
