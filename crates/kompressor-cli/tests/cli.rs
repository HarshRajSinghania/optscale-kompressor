//! The command line, driven as a user drives it.
//!
//! These run the real binary rather than calling the command functions, because
//! most of what can break here is the part a unit test does not reach: argument
//! parsing, what lands on stdout against stderr, and the exit code. The exit
//! code in particular is load-bearing — `bench replay --compare` is meant to
//! gate CI on it.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// The binary under test, built by cargo for this test target.
///
/// Configuration discovery is pinned at a directory that holds no
/// configuration, because otherwise every one of these runs would read
/// whatever the developer happens to have on the search path: a `[refs] path`
/// in a personal config would send `retrieve` looking somewhere the test never
/// wrote. `$XDG_CONFIG_HOME` ends the search before `$HOME` is consulted.
fn kompressor() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kompressor"));
    command
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(scratch_dir());
    command
}

/// A working directory belonging to this invocation alone.
///
/// `compress` in recoverable mode retains fragments in the configured refs
/// store, and the default is `./data/refs.db` — *relative to the working
/// directory*. Every test that did not set one ran in the package directory
/// and shared one SQLite file, in parallel. Under contention the retention
/// check fails, the CLI correctly declines to emit a handle it cannot honour
/// and returns the payload uncompressed, and an assertion that compression
/// shrank anything fails.
///
/// It failed twice before the cause was understood, both times passing on
/// every re-run in isolation, which is exactly how a shared-state flake
/// presents. A test that sets its own `current_dir` still overrides this.
fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cwd-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("a working directory for this invocation");
    path
}

/// A directory that exists and contains no `kompressor/config.toml`.
fn no_config_dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-config");
    std::fs::create_dir_all(&path).expect("a directory to point config discovery at");
    path
}

/// A payload well over the size gate that the log compressor folds hard.
/// Lines sharing a template but differing in their values, so the fold cannot
/// be reversed from its `[×N]` count and a handle is retained.
fn varying_log() -> String {
    (0..200)
        .map(|i| format!("2026-01-01T00:00:00Z INFO worker heartbeat ok, queue depth {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn foldable_log() -> String {
    (0..200)
        .map(|_| "2026-01-01T00:00:00Z INFO worker heartbeat ok, queue depth 0")
        .collect::<Vec<_>>()
        .join("\n")
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("a temp file");
    file.write_all(body.as_bytes()).expect("written");
    path
}

/// A path as a TOML basic string. Temporary directories are plain enough that
/// only the quoting itself matters, but writing it by hand is how a config
/// fixture ends up unparseable on the one machine with a space in its tmpdir.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[test]
fn compressing_from_standard_input_writes_only_the_payload_to_standard_output() {
    let log = foldable_log();
    let assert = kompressor()
        .args(["compress", "--mode", "recoverable"])
        .write_stdin(log.clone())
        .assert()
        .success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(output.len() < log.len(), "the payload was not compressed");
    assert!(
        output.contains("[×"),
        "a fold has to announce itself: {output}"
    );
    assert!(
        assert.get_output().stderr.is_empty(),
        "nothing but the payload unless asked"
    );
}

#[test]
fn compressing_a_file_by_name_gives_the_same_answer_as_the_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let log = foldable_log();
    let path = write(dir.path(), "tool-output.log", &log);

    let from_file = kompressor()
        .args(["compress", path.to_str().unwrap()])
        .assert()
        .success();
    let from_stdin = kompressor()
        .arg("compress")
        .write_stdin(log)
        .assert()
        .success();

    assert_eq!(
        from_file.get_output().stdout,
        from_stdin.get_output().stdout,
        "the same bytes must compress the same way whichever way they arrive"
    );
}

#[test]
fn a_protected_role_comes_back_byte_identical() {
    let log = foldable_log();
    let assert = kompressor()
        .args(["compress", "--role", "assistant"])
        .write_stdin(log.clone())
        .assert()
        .success();
    assert_eq!(
        String::from_utf8(assert.get_output().stdout.clone()).unwrap(),
        log,
        "the model's own turns are its working memory"
    );
}

#[test]
fn compress_json_reports_the_type_the_sizes_and_the_handles() {
    let assert = kompressor()
        .args(["compress", "--json"])
        .write_stdin(varying_log())
        .assert()
        .success();
    let value: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object");

    assert_eq!(value["kind"], "log");
    assert_eq!(value["mode"], "recoverable");
    assert!(value["chars_out"].as_u64().unwrap() < value["chars_in"].as_u64().unwrap());
    assert!(value["saved_pct"].as_f64().unwrap() > 50.0);
    assert!(
        !value["refs"].as_array().unwrap().is_empty(),
        "recoverable mode retains what it folds"
    );

    // The same fold over identical lines reports no handle at all: `[×200]`
    // reconstructs them, so there is nothing a fetch could add.
    let assert = kompressor()
        .args(["compress", "--json"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object");
    assert!(value["chars_out"].as_u64().unwrap() < value["chars_in"].as_u64().unwrap());
    assert!(
        value["refs"].as_array().unwrap().is_empty(),
        "an identical repeat needs no handle"
    );
    assert!(
        value["content"].as_str().unwrap().contains("[×180]"),
        "180 folded, 20 kept as the untouched tail"
    );
}

#[test]
fn detect_json_names_the_type_and_the_compressor_it_selects() {
    let assert = kompressor()
        .args(["detect", "--json"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();

    assert_eq!(value["content_type"], "build");
    assert_eq!(value["strategy"], "log");
    assert!(value["confidence"].as_f64().unwrap() > 0.0);
}

#[test]
fn detect_says_when_a_payload_was_never_classified_because_it_is_too_small() {
    kompressor()
        .arg("detect")
        .write_stdin("tiny")
        .assert()
        .success()
        .stdout(predicate::str::contains("below min_compress_chars"));
}

#[test]
fn explain_names_the_winning_route_and_every_decision_behind_it() {
    let assert = kompressor()
        .args(["compress", "--explain"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let explanation = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(
        explanation.contains("route        whole_message"),
        "the winning reading has to be named: {explanation}"
    );
    for expected in [
        "size gate    passed",
        "detected     build",
        "strategy     log",
        "redraw",
        "accounted    accepted: the output carries an omission marker",
        "markers",
        "refs",
    ] {
        assert!(
            explanation.contains(expected),
            "{expected} missing from:\n{explanation}"
        );
    }
    assert!(
        !assert.get_output().stdout.is_empty(),
        "the payload still goes to stdout"
    );
}

#[test]
fn explain_says_a_handleless_marker_is_a_size_cap_rather_than_a_mode() {
    // The confusion this command exists to end: in aggressive mode nothing is
    // retained, and every marker is plain. The report must say so rather than
    // leaving an operator to conclude the mode is misconfigured.
    let assert = kompressor()
        .args(["compress", "--explain", "--mode", "aggressive"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let explanation = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(
        explanation.contains("retention is off in aggressive mode"),
        "{explanation}"
    );
    assert!(explanation.contains("refs         none retained"));
}

#[test]
fn explain_in_json_carries_the_same_report_as_one_field() {
    let assert = kompressor()
        .args(["compress", "--json", "--explain"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();

    assert_eq!(value["explain"]["route"]["winner"], "whole_message");
    assert_eq!(value["explain"]["strategy"], "log");
    assert!(value["explain"]["accounted"]["marker_present"]
        .as_bool()
        .unwrap());
    assert!(!value["explain"]["markers"].as_array().unwrap().is_empty());
}

#[test]
fn check_config_accepts_the_built_in_defaults_and_prints_them_back() {
    kompressor()
        .arg("check-config")
        .assert()
        .success()
        .stdout(predicate::str::contains("[server]"))
        .stdout(predicate::str::contains("min_compress_chars = 512"));
}

#[test]
fn check_config_reads_a_file_and_redacts_its_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "kompressor.toml",
        "[server]\nlisten = \"127.0.0.1:9123\"\n\n[auth]\nsecret = \"hunter2\"\n",
    );
    kompressor()
        .args(["check-config", "--config", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("127.0.0.1:9123"))
        .stdout(predicate::str::contains("<redacted>"))
        .stdout(predicate::str::contains("hunter2").not());
}

#[test]
fn check_config_rejects_an_invalid_file_with_the_usage_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "broken.toml", "[server\nlisten = ");
    kompressor()
        .args(["check-config", "--config", path.to_str().unwrap()])
        .assert()
        .code(2);
}

#[test]
fn retrieve_returns_a_fragment_and_a_line_window_of_it() {
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use kompressor_store::types::{PutMeta, RefStore, Tenant};
    use kompressor_store::SqliteRefStore;

    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("refs.db");
    let fragment = "line one\nline two\nline three\nline four";
    let handle = ref_for(fragment);
    {
        let store = SqliteRefStore::open(&refs_path).unwrap();
        store
            .put_many(
                &Tenant::new("acme"),
                &[(handle, fragment.to_string())],
                &PutMeta::new(Kind::Log),
            )
            .unwrap();
    }

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "acme",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("line one"))
        .stdout(predicate::str::contains("line four"));

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "acme",
            "--lines",
            "2:3",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("line two"))
        .stdout(predicate::str::contains("line three"))
        .stdout(predicate::str::contains("line one").not())
        .stdout(predicate::str::contains("line four").not());
}

#[test]
fn retrieving_a_handle_from_another_tenant_fails_with_a_hint_that_does_not_invite_invention() {
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use kompressor_store::types::{PutMeta, RefStore, Tenant};
    use kompressor_store::SqliteRefStore;

    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("refs.db");
    let handle = ref_for("tenant acme's tool output");
    {
        let store = SqliteRefStore::open(&refs_path).unwrap();
        store
            .put_many(
                &Tenant::new("acme"),
                &[(handle, "tenant acme's tool output".to_string())],
                &PutMeta::new(Kind::Log),
            )
            .unwrap();
    }

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "beta",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("rather than reconstructing"));
}

#[test]
fn a_handle_that_is_not_shaped_like_a_handle_is_a_usage_error() {
    kompressor()
        .args(["retrieve", "refs/heads/main"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is not a handle"));
}

/// A statistics database holding one compression and one retrieval of it.
fn stats_database(dir: &Path) -> PathBuf {
    use kompressor_engine::types::{Kind, Mode};
    use kompressor_stats::{spawn, Event, OptimizationEvent, RetrievalEvent, StatsConfig, Surface};

    let stats_path = dir.join("stats.db");
    let config = StatsConfig::at(&stats_path);
    let (sink, handle) = spawn(&config).unwrap();

    let mut event = OptimizationEvent::new(
        1_700_000_000_000,
        "acme",
        "req-1",
        Surface::Api,
        Mode::Recoverable,
    );
    event.tokens_in = 9_000;
    event.tokens_out = 3_000;
    event.record_type(Kind::Json, 2);
    sink.send(Event::Optimization(event));

    // The debit side: without it every report reads as pure profit.
    sink.send(Event::Retrieval(RetrievalEvent {
        time_ms: 1_700_000_001_000,
        tenant: "acme".to_string(),
        request_id: Some("req-1".to_string()),
        surface: Surface::Mcp,
        reference: "k_AbCdEfGhIjKl".to_string(),
        found: true,
        expired: false,
        ranged: true,
        fragment_bytes: 4_000,
        fragment_chars: 4_000,
        fragment_tokens: 1_000,
        returned_chars: 2_000,
        returned_tokens: 500,
        withheld: false,
    }));

    sink.flush().unwrap();
    handle.shutdown().unwrap();
    stats_path
}

#[test]
fn stats_summary_reports_gross_and_net_from_a_real_database() {
    let dir = tempfile::tempdir().unwrap();
    let stats_path = stats_database(dir.path());

    kompressor()
        .args([
            "stats",
            "summary",
            "--stats",
            stats_path.to_str().unwrap(),
            "--tenant",
            "acme",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("saved gross         6000"))
        // gross minus what the retrieval put back: a report that showed only
        // gross would be advertising the system rather than describing it
        .stdout(predicate::str::contains("saved net           5500"));

    let assert = kompressor()
        .args([
            "stats",
            "by-type",
            "--stats",
            stats_path.to_str().unwrap(),
            "--tenant",
            "acme",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let rows: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(rows[0]["content_type"], "json");
}

#[test]
fn every_other_stats_report_answers_from_the_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let stats_path = stats_database(dir.path());
    let path = stats_path.to_str().unwrap().to_string();

    let run = |args: &[&str]| {
        let mut command = kompressor();
        command.arg("stats");
        command.args(args);
        command.args(["--stats", &path, "--tenant", "acme"]);
        command.assert().success()
    };

    run(&["histogram", "--bucket", "day"])
        .stdout(predicate::str::contains("2023-11-14"))
        .stdout(predicate::str::contains("5500"));
    run(&["retrievals"])
        .stdout(predicate::str::contains("hits           1"))
        // a ranged read is the cheap kind, so its share is worth watching
        .stdout(predicate::str::contains("ranged         1"))
        .stdout(predicate::str::contains("k_AbCdEfGhIjKl"));
    run(&["request", "req-1"])
        .stdout(predicate::str::contains("saved gross    6000"))
        .stdout(predicate::str::contains("saved net      5500"));
    // No sweep has sampled the store, and that is not an error.
    run(&["store"]).stdout(predicate::str::contains("no store sample"));

    kompressor()
        .args(["stats", "request", "req-missing", "--stats", &path])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no events recorded"));
}

#[test]
fn stats_against_a_database_that_does_not_exist_says_so_rather_than_reporting_zeros() {
    kompressor()
        .args(["stats", "summary", "--stats", "/nonexistent/stats.db"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no statistics database"));
}

#[test]
fn a_window_bound_that_is_not_a_time_is_a_usage_error() {
    kompressor()
        .args(["stats", "summary", "--from", "last tuesday"])
        .assert()
        .code(2);
}

/// A one-payload corpus and a baseline claiming a coverage this run cannot meet.
fn regression_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let corpus = write(
        dir,
        "corpus.jsonl",
        &format!(
            "{{\"corpus_version\": 1}}\n{}\n",
            serde_json::json!({
                "content_id": "a",
                "content": foldable_log(),
                "role": "tool",
                "source_tool": "exec_command",
                "requests": 4,
            })
        ),
    );
    let baseline = write(
        dir,
        "baseline.json",
        "{\"format\": 4, \"coverage_pct\": 99.0, \"claimed_pct\": 100.0, \"payloads\": 1,
          \"chars_in\": 1000, \"chars_out\": 10}",
    );
    (corpus, baseline)
}

#[test]
fn bench_replay_prints_coverage_claimed_and_billed() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());

    kompressor()
        .args(["bench", "replay", "--corpus", corpus.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("coverage"))
        .stdout(predicate::str::contains("claimed"))
        .stdout(predicate::str::contains("billed"))
        .stdout(predicate::str::contains("no invariant violated"));
}

#[test]
fn a_bench_comparison_that_regressed_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, baseline) = regression_fixture(dir.path());

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            baseline.to_str().unwrap(),
        ])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("REGRESSED"))
        .stdout(predicate::str::contains("FAIL"));
}

#[test]
fn a_run_compared_against_its_own_recording_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let recorded = dir.path().join("recorded.json");

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--save",
            recorded.to_str().unwrap(),
        ])
        .assert()
        .success();
    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            recorded.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PASS"));
}

#[test]
fn a_baseline_from_an_older_format_fails_loudly_instead_of_comparing_against_zeros() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let stale = write(
        dir.path(),
        "stale.json",
        "{\"format\": 2, \"coverage_pct\": 1}",
    );

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            stale.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("baseline format 2, expected 4"));
}

#[test]
fn saving_and_comparing_in_one_run_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, baseline) = regression_fixture(dir.path());

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--save",
            baseline.to_str().unwrap(),
            "--compare",
            baseline.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

#[test]
fn version_reports_the_build_and_whether_the_grammars_are_compiled_in() {
    // Against the compiled-in version rather than a literal. Pinning the
    // number made this the one test that fails on a release, which teaches
    // whoever bumps it that a red suite is routine — and it never checked
    // anything the build did not already guarantee.
    kompressor()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "kompressor {}",
            env!("CARGO_PKG_VERSION")
        )))
        .stdout(predicate::str::contains("code-aware"));
}

#[test]
fn recoverable_cli_handles_survive_the_compress_process() {
    let dir = tempfile::tempdir().unwrap();
    let log = varying_log();
    let output = kompressor()
        .current_dir(dir.path())
        .args(["compress", "--json"])
        .write_stdin(log.clone())
        .assert()
        .success();
    let compressed: serde_json::Value =
        serde_json::from_slice(&output.get_output().stdout).unwrap();
    let refs = compressed["refs"].as_array().unwrap();
    assert!(!refs.is_empty());
    for reference in refs {
        let handle = reference["handle"].as_str().unwrap();
        let retrieved = kompressor()
            .current_dir(dir.path())
            .args(["retrieve", handle])
            .assert()
            .success();
        let fragment = std::str::from_utf8(&retrieved.get_output().stdout)
            .unwrap()
            .trim_end();
        assert!(!fragment.is_empty());
        assert!(log.contains(fragment));
        assert_eq!(
            fragment.len(),
            reference["bytes"].as_u64().unwrap() as usize
        );
    }
}

#[test]
fn a_configured_refs_path_round_trips_from_compress_to_retrieve() {
    // The packaged installs write a config naming a refs path outside the
    // working directory. `retrieve` used to ignore the configuration entirely
    // and open the compiled-in `./data/refs.db`, so every handle `compress`
    // minted on those installs failed to resolve.
    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("store").join("refs.db");
    let config = write(
        dir.path(),
        "config.toml",
        &format!(
            "[refs]\npath = {}\n",
            toml_string(refs_path.to_str().unwrap())
        ),
    );
    let config = config.to_str().unwrap();
    let log = varying_log();

    let output = kompressor()
        .current_dir(dir.path())
        .args(["compress", "--json", "--config", config])
        .write_stdin(log.clone())
        .assert()
        .success();
    let compressed: serde_json::Value =
        serde_json::from_slice(&output.get_output().stdout).unwrap();
    let refs = compressed["refs"].as_array().unwrap();
    assert!(!refs.is_empty(), "nothing was retained to retrieve");
    assert!(
        refs_path.exists(),
        "the fragments went somewhere other than the configured store"
    );
    assert!(
        !dir.path().join("data").exists(),
        "the compiled-in default store was written despite the configuration"
    );

    for reference in refs {
        let handle = reference["handle"].as_str().unwrap();
        // Named explicitly, the way a user debugging this would.
        let retrieved = kompressor()
            .current_dir(dir.path())
            .args(["retrieve", handle, "--config", config])
            .assert()
            .success();
        let fragment = std::str::from_utf8(&retrieved.get_output().stdout)
            .unwrap()
            .trim_end();
        assert!(log.contains(fragment));

        // And discovered, which is what the packaged installs actually do.
        kompressor()
            .current_dir(dir.path())
            .env("KOMPRESSOR_CONFIG", config)
            .args(["retrieve", handle])
            .assert()
            .success()
            .stdout(predicate::str::contains(fragment.to_string()));

        // `--refs` still wins over the configuration rather than being the
        // only way to reach a store.
        kompressor()
            .current_dir(dir.path())
            .args([
                "retrieve",
                handle,
                "--config",
                config,
                "--refs",
                dir.path().join("elsewhere.db").to_str().unwrap(),
            ])
            .assert()
            .code(1);
    }
}

#[test]
fn cli_falls_back_when_fragments_cannot_survive_exit() {
    for refs_config in [
        "backend = \"memory\"",
        "path = \"blocked/refs.db\"",
        "max_bytes = 1",
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "blocked", "a file, not a directory");
        let config = write(
            dir.path(),
            "config.toml",
            &format!("[refs]\n{refs_config}\n"),
        );
        let log = varying_log();
        let output = kompressor()
            .current_dir(dir.path())
            .args(["compress", "--json", "--config", config.to_str().unwrap()])
            .write_stdin(log.clone())
            .assert()
            .success();
        let compressed: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(compressed["content"], log);
        assert!(compressed["refs"].as_array().unwrap().is_empty());
        assert!(compressed["applied"].as_array().unwrap().is_empty());
    }
}
