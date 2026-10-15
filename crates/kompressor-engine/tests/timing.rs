//! A coarse throughput check over the real corpus.
//!
//! Not a benchmark harness — it exists so a change that makes the engine an
//! order of magnitude slower fails a test rather than being noticed in
//! production. Grammars are warmed before the clock starts: they load lazily,
//! and the first C-family payload otherwise charges its one-time cost to
//! whichever payload happens to sort first.

use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use kompressor_engine::EngineOptions;
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::Context;

#[derive(serde::Deserialize)]
struct Golden {
    records: Vec<Record>,
}
#[derive(serde::Deserialize)]
struct Record {
    role: String,
    content: String,
}

fn corpus() -> Vec<Record> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden.json.gz");
    let file = std::fs::File::open(path).expect("corpus readable");
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .unwrap();
    serde_json::from_str::<Golden>(&text).unwrap().records
}

/// Skipped in a debug build, where this would measure the absence of
/// optimization rather than the engine: the same corpus runs about fifteen
/// times slower unoptimized, so a threshold loose enough to pass there would
/// catch nothing worth catching. Run it with `--release`.
#[cfg_attr(
    debug_assertions,
    ignore = "timing is only meaningful in a release build"
)]
#[test]
fn the_engine_keeps_up_with_agent_traffic() {
    let records = corpus();
    let opts = EngineOptions::default();

    // warm every lazily-loaded grammar and regex before timing
    for record in records.iter().take(40) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role(&record.role);
        let _ = compress_content(
            &record.content,
            &record.role,
            &opts,
            None,
            &mut refs,
            &ctx,
            &mut applied,
        );
    }

    let total_chars: usize = records.iter().map(|r| r.content.chars().count()).sum();
    let started = Instant::now();
    for record in &records {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role(&record.role);
        let _ = compress_content(
            &record.content,
            &record.role,
            &opts,
            None,
            &mut refs,
            &ctx,
            &mut applied,
        );
    }
    let elapsed = started.elapsed();
    let ms_per_kb = elapsed.as_secs_f64() * 1000.0 / (total_chars as f64 / 1024.0);
    println!(
        "compressed {} payloads, {:.1} KB, in {:.1} ms -> {:.4} ms/KB",
        records.len(),
        total_chars as f64 / 1024.0,
        elapsed.as_secs_f64() * 1000.0,
        ms_per_kb
    );
    // A deliberately generous ceiling: it still catches an order-of-magnitude
    // regression while staying stable on a loaded or shared machine, which is
    // the only way a timing assertion earns its place in a test suite.
    assert!(ms_per_kb < 0.65, "engine slowed to {ms_per_kb:.4} ms/KB");
}
