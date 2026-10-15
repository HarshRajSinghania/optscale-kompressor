//! The regression gate: the committed corpus, replayed in every mode.
//!
//! These are slower than a unit test and they are the only tests in the
//! workspace that run real agent output — 233 payloads that between them cover
//! every routing bucket, including the ones nothing claims. A compressor change
//! that breaks an invariant on real data almost never breaks it on a
//! hand-written fixture, because the hand-written fixture was written by
//! someone who already had the bug in mind.

use std::path::PathBuf;

use kompressor_bench::{
    compare_baseline, load_baseline, load_corpus, replay, save_baseline, Tolerance,
    COMMITTED_CORPUS_PAYLOADS,
};
use kompressor_engine::options::EngineOptions;
use kompressor_engine::types::Mode;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn corpus() -> Vec<kompressor_bench::Payload> {
    load_corpus(&fixture("corpus.jsonl.gz")).expect("the committed corpus loads")
}

#[test]
fn the_committed_corpus_loads_and_is_not_trivial() {
    let payloads = corpus();
    // 233 payloads sampled across the routing buckets, 7 added later as
    // regressions (two `grep -C` results with a mixed `:`/`-` gutter, two whose
    // content sits inside one opaque line, a `cat -n` over disassembly, two apt
    // transcripts whose repeats differ in a package name), and 17 added for
    // size, tagged `size-tail`: the corpus had nothing above 28 KB while
    // production runs to 40 KB, where the agent harness truncates tool output.
    // It stops there — payloads that looked larger in the Harbor dumps were
    // base64 `input_image` blocks, which the wire walker never offers to a
    // compressor.
    assert_eq!(payloads.len(), COMMITTED_CORPUS_PAYLOADS);
    let chars: usize = payloads.iter().map(|p| p.content.chars().count()).sum();
    assert!(
        chars > 1_000_000,
        "{chars} characters is too small a corpus"
    );
    assert!(
        payloads.iter().all(|p| p.role == "tool"),
        "every other role is protected from compression, so replaying it measures nothing"
    );
    assert!(
        payloads.iter().filter(|p| p.requests > 1).count() > 100,
        "the billed metric needs request counts to weigh anything"
    );
}

#[test]
fn no_safety_invariant_is_violated_on_the_real_corpus_in_any_mode() {
    let payloads = corpus();
    for mode in [Mode::Recoverable, Mode::Aggressive, Mode::Lossless] {
        let options = EngineOptions {
            mode,
            ..EngineOptions::default()
        };
        let report = replay(&payloads, &options, true);

        assert_eq!(report.results.len(), COMMITTED_CORPUS_PAYLOADS);
        assert!(
            report.is_clean(),
            "{mode}: {:?}\n{}",
            report.violations(),
            report.render_table()
        );
        assert!(
            report.query_dependent().is_empty(),
            "{mode}: output must not depend on the user turn that preceded the payload"
        );
    }
}

#[test]
fn every_payload_the_recoverable_run_declines_comes_back_byte_identical() {
    let payloads = corpus();
    let report = replay(&payloads, &EngineOptions::default(), false);
    for (payload, result) in payloads.iter().zip(&report.results) {
        if result.kind.is_none() {
            assert_eq!(
                result.chars_out,
                payload.content.chars().count(),
                "{}: a decline is not allowed to change a byte",
                payload.content_id
            );
        }
        assert!(
            result.chars_out <= result.chars_in,
            "{}: compression made it bigger",
            payload.content_id
        );
    }
}

#[test]
fn the_real_corpus_removes_a_meaningful_share_of_its_eligible_bytes() {
    let report = replay(&corpus(), &EngineOptions::default(), false);
    let coverage = report.coverage();

    // Floors, not targets: they exist to catch a change that silently turns a
    // compressor off, which otherwise shows up as a quietly smaller number
    // nobody reads. The recorded baseline is what measures an improvement.
    assert!(coverage.pct > 30.0, "coverage collapsed: {coverage:?}");
    assert!(
        report.claimed_pct() >= coverage.pct,
        "claimed is an upper bound on coverage"
    );
    let billed = report.billed().expect("the corpus records request counts");
    assert!(billed.pct > 0.0);
    assert!(
        report.buckets().len() > 8,
        "the corpus is stratified across buckets; {} is too few",
        report.buckets().len()
    );
}

#[test]
fn a_run_measured_against_the_recorded_baseline_reports_both_sides() {
    // The point of shipping a recorded floor: a replay is measured against a
    // fixed reference point rather than against itself.
    let baseline = load_baseline(&fixture("baseline.json")).expect("format 4 loads");
    let report = replay(&corpus(), &EngineOptions::default(), false);
    let comparison = compare_baseline(&report, &baseline, Tolerance::default());

    assert_eq!(comparison.headline.len(), 3, "coverage, claimed and billed");
    assert!(
        comparison
            .headline
            .iter()
            .any(|m| m.name == "billed_pct" && m.current > 0.0),
        "the baseline records billed coverage, so it has to be compared"
    );
    assert!(
        comparison.violations.is_empty(),
        "{:?}",
        comparison.violations
    );
    // Every payload in the recorded baseline is in this corpus, so nothing is
    // silently dropped from the bucket comparison.
    assert!(
        !comparison
            .notes
            .iter()
            .any(|n| n.contains("not in this corpus")),
        "{:?}",
        comparison.notes
    );
}

#[test]
fn a_run_compared_against_its_own_recording_passes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("baseline.json");
    let report = replay(&corpus(), &EngineOptions::default(), true);

    save_baseline(&report, &path).unwrap();
    let baseline = load_baseline(&path).unwrap();
    let comparison = compare_baseline(&report, &baseline, Tolerance::default());

    assert!(comparison.passed, "{}", comparison.render());
    assert!(
        comparison.membership.is_empty(),
        "nothing moved bucket between a run and itself"
    );
    assert!(comparison.buckets.iter().all(|b| b.delta == 0.0));
}
