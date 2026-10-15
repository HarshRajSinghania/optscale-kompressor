//! Bounded, file-grouped compression of grep and ripgrep output.
//!
//! A search result is already a selection the agent asked for, so the only safe
//! lever is sampling *within* it: group the hits by the file they came from,
//! rank the files, and keep a bounded slice of each. Everything that leaves is
//! counted, including whole files — see [`crush_search`].

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::adaptive::adaptive_k;
use crate::markers::omission_marker;
use crate::options::EngineOptions;
use crate::refs::RefCollector;
use crate::text::{chars_len, join_lines, split_lines};
use crate::types::Context;

/// grep's hit form, `path:line:text`.
///
/// The path is **non-greedy**, which is safe precisely because a path can never
/// span a colon: the match stops at the real separator however many hyphens the
/// filename holds.
static MATCH_COLON: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^((?:[A-Za-z]:)?[^:\n]+?):(\d+):").unwrap());

/// ripgrep's *context* form, `path-line-text`.
///
/// Here the separator is a hyphen, which filenames use freely, so the path has
/// to be **greedy** and take the last `-<digits>-` rather than the first.
///
/// The two patterns are separate for exactly this reason. One pattern accepting
/// either separator with a non-greedy path split
/// `/app/logs/2025-07-03_api.log:12:ERROR …` into path `/app/logs/2025` and
/// line `07`, collapsing 41 dated files into a single pseudo-file on a real run.
static MATCH_DASH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^((?:[A-Za-z]:)?[^:\n]+)-(\d+)-").unwrap());

/// Words that make one hit worth keeping over its neighbours.
static SIGNAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:error|fail(?:ed|ure)?|exception|panic|warn(?:ing)?|todo|fixme)\b")
        .unwrap()
});

/// Fewer hits than this and the payload is its own summary already.
const MIN_MATCHES: usize = 8;

/// The path a grep line reports, preferring the unambiguous colon form.
fn match_path(line: &str) -> Option<&str> {
    let caps = MATCH_COLON
        .captures(line)
        .or_else(|| MATCH_DASH.captures(line))?;
    Some(caps.get(1).unwrap().as_str())
}

/// How much one hit is worth keeping, judged from the line alone.
///
/// This deliberately does **not** score the line against the question that
/// prompted the search. Query relevance used to add up to 0.3 here, and it was
/// removed because of what it cost against what it bought.
///
/// The cost is the provider prompt cache. An agent re-sends its history every
/// turn under a *different* latest question, so a payload scored against that
/// question changes bytes inside a prefix the provider is caching — and 93% of
/// billed input on this traffic is a cache read. Over the corpus, the
/// query-scored engine left 292 payloads (4.85M chars, 14% of the total)
/// producing different output for different questions; without it, none do.
///
/// What it bought, measured against what the agent actually went on to use: 3
/// needles out of 5,786, 91.01% retention against 90.96%. Coverage was 0.03pp
/// *better* without it. Signal words were carrying the weight anyway (0.5
/// against at most 0.3). Reviving query awareness needs frozen-prefix wiring
/// first, so scoring can touch the live tail and never rewrite cached bytes.
fn score(line: &str) -> f64 {
    if SIGNAL.is_match(line) {
        0.51
    } else {
        // a deterministic preference order inside a file with no signal at all
        0.01
    }
}

/// Half-to-even rounding, which the per-file allowance is derived from.
///
/// Rust's `f64::round` is half-away-from-zero, so a budget landing exactly on
/// `.5` would keep one more line than the allowance intends. A one-character
/// difference in a marker count is still a different prompt, and a different
/// prompt is a prompt-cache miss.
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let frac = value - floor;
    if frac > 0.5 || (frac == 0.5 && (floor as i64) % 2 != 0) {
        floor + 1.0
    } else {
        floor
    }
}

struct Entry<'a> {
    /// Position in the original payload, the tiebreak that keeps ranking stable.
    index: usize,
    line: &'a str,
    score: f64,
}

struct Group<'a> {
    path: &'a str,
    entries: Vec<Entry<'a>>,
}

/// Compress grep-shaped output by sampling hits per file.
///
/// Returns `None` when the payload is left alone: too few hits to be worth
/// sampling, or nothing actually dropped.
pub fn crush_search(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // Sampling hits is not reversible from the output, and lossless is the one
    // mode that exists to drop nothing. The reference engine had no mode check
    // here at all, so lossless silently lost grep matches.
    if opts.declines_as_lossless() {
        return None;
    }

    let lines = split_lines(content);
    let mut order: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<Group<'_>> = Vec::new();
    // Lines that parse as no file's hit — a `Searching …` banner, a blank
    // separator. They are hoisted ahead of the groups, which is the one
    // reordering this function performs.
    let mut outside: Vec<&str> = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        match match_path(line) {
            Some(path) => {
                let slot = *order.entry(path).or_insert_with(|| {
                    groups.push(Group {
                        path,
                        entries: Vec::new(),
                    });
                    groups.len() - 1
                });
                groups[slot].entries.push(Entry {
                    index,
                    line,
                    score: score(line),
                });
            }
            None => outside.push(line),
        }
    }

    let total: usize = groups.iter().map(|g| g.entries.len()).sum();
    if groups.is_empty() || total < MIN_MATCHES {
        return None;
    }

    // Most signal first, path as the tiebreak so the same payload always ranks
    // the same way regardless of how the hash map happened to order it.
    groups.sort_by(|a, b| {
        let left: f64 = a.entries.iter().map(|e| e.score).sum();
        let right: f64 = b.entries.iter().map(|e| e.score).sum();
        right
            .partial_cmp(&left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(b.path))
    });

    let kept_files = opts.search_max_files.min(groups.len());
    let (ranked, cut) = groups.split_at(kept_files);

    let candidates: Vec<&str> = ranked
        .iter()
        .flat_map(|g| g.entries.iter().map(|e| e.line))
        .collect();
    let budget = adaptive_k(&candidates, 5, 30.min(candidates.len()), 1.0);

    let mut out: Vec<String> = outside.iter().map(|l| (*l).to_string()).collect();
    let mut dropped = false;

    for group in ranked {
        let entries = &group.entries;
        let share = budget as f64 * entries.len() as f64 / candidates.len() as f64;
        let allowance = (round_half_even(share) as usize).max(2);

        // The first and last hit in a file are never negotiable: together they
        // report the span of the file the search actually reached.
        let mut keep: HashSet<usize> = HashSet::from([0, entries.len() - 1]);
        let mut by_signal: Vec<usize> = (0..entries.len()).collect();
        by_signal.sort_by(|&a, &b| {
            entries[b]
                .score
                .partial_cmp(&entries[a].score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| entries[a].index.cmp(&entries[b].index))
        });
        for index in by_signal {
            if keep.len() >= allowance {
                break;
            }
            keep.insert(index);
        }

        let omitted: Vec<&str> = entries
            .iter()
            .enumerate()
            .filter(|(index, _)| !keep.contains(index))
            .map(|(_, entry)| entry.line)
            .collect();
        out.extend(
            entries
                .iter()
                .enumerate()
                .filter(|(index, _)| keep.contains(index))
                .map(|(_, entry)| entry.line.to_string()),
        );
        if !omitted.is_empty() {
            dropped = true;
            out.push(omission_marker(
                "match",
                omitted.len(),
                &join_lines(&omitted),
                refs,
                &format!(" in {}", group.path),
            ));
        }
    }

    if !cut.is_empty() {
        // Files past the budget used to be dropped with no marker at all: the
        // per-file marker above only covers hits omitted *inside* a kept file. A
        // grep over 25 files came back as 8, and an agent reading that concludes
        // the other 17 hold no matches. Whatever leaves has to be counted.
        dropped = true;
        let hidden: Vec<&str> = cut
            .iter()
            .flat_map(|g| g.entries.iter().map(|e| e.line))
            .collect();
        out.push(omission_marker(
            "match",
            hidden.len(),
            &join_lines(&hidden),
            refs,
            &format!(" across {} further files", cut.len()),
        ));
    }

    if !dropped {
        // Nothing left out, so the only difference from the input would be this
        // function's own regrouping plus the trailing newline `splitlines` ate:
        // a mutated payload, no marker, and a bogus optimization event. grep's
        // ordering is information; keep it when there is nothing to gain.
        return None;
    }

    let result = join_lines(&out);
    (chars_len(&result) < chars_len(content)).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs() -> RefCollector {
        RefCollector::new(1 << 20)
    }

    fn opts() -> EngineOptions {
        EngineOptions::default()
    }

    /// `n` hits in `path`, long enough that dropping one is a real saving.
    fn hits(path: &str, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    "{path}:{}:    some ordinary matching line of source {i}",
                    i + 1
                )
            })
            .collect()
    }

    #[test]
    fn search_groups_by_the_real_path() {
        // The colon form must not stop at a hyphen inside the filename, and the
        // hyphen form must take the last `-<digits>-`, not the first.
        assert_eq!(
            match_path("/app/logs/2025-07-03_api.log:12:ERROR connection refused"),
            Some("/app/logs/2025-07-03_api.log")
        );
        assert_eq!(
            match_path("/app/logs/2025-07-03_api.log-11-    prior context line"),
            Some("/app/logs/2025-07-03_api.log")
        );
        assert_eq!(
            match_path(r"C:\src\main.rs:4:let x = 1;"),
            Some(r"C:\src\main.rs")
        );
        assert_eq!(match_path("Searching 41 files for ERROR"), None);
    }

    #[test]
    fn dated_log_files_stay_distinct_end_to_end() {
        // The failure this guards against collapsed 41 dated files into one
        // pseudo-file, which would leave nothing for the cut marker to count.
        let mut lines = Vec::new();
        for day in 3..9 {
            lines.extend(hits(&format!("/app/logs/2025-07-0{day}_api.log"), 6));
        }
        let content = lines.join("\n");
        let options = EngineOptions {
            search_max_files: 2,
            ..opts()
        };
        let out = crush_search(&content, &options, &Context::default(), &mut refs()).unwrap();
        assert!(
            out.contains("across 4 further files"),
            "six dated files must rank as six, got {out}"
        );
    }

    #[test]
    fn search_never_drops_a_file_silently() {
        let mut lines = Vec::new();
        for i in 0..25 {
            lines.extend(hits(&format!("src/module_{i}.rs"), 3));
        }
        let content = lines.join("\n");
        let out = crush_search(&content, &opts(), &Context::default(), &mut refs()).unwrap();
        // 25 files, 8 kept: the other 17 and their 51 hits must be counted.
        assert!(out.contains("across 17 further files"), "got {out}");
        assert!(out.contains("[... 51 matches omitted"), "got {out}");
    }

    #[test]
    fn regrouping_alone_is_not_compression() {
        // One file, eight hits: the budget covers every one of them, so the only
        // change would be hoisting the banner and losing the trailing newline.
        let mut lines = hits("src/app.rs", 8);
        lines.insert(4, "Searching src for TODO".to_string());
        let content = format!("{}\n", lines.join("\n"));
        assert!(crush_search(&content, &opts(), &Context::default(), &mut refs()).is_none());
    }

    #[test]
    fn a_handful_of_matches_is_left_alone() {
        let content = hits("src/app.rs", MIN_MATCHES - 1).join("\n");
        assert!(crush_search(&content, &opts(), &Context::default(), &mut refs()).is_none());
    }

    #[test]
    fn a_payload_with_no_matches_at_all_is_left_alone() {
        let content = "just\nsome\nprose\nwith\nno\ngrep\nhits\nin\nit\n";
        assert!(crush_search(content, &opts(), &Context::default(), &mut refs()).is_none());
    }

    #[test]
    fn every_kept_file_keeps_its_first_and_last_match() {
        let mut lines = Vec::new();
        for i in 0..12 {
            lines.extend(hits(&format!("src/module_{i}.rs"), 20));
        }
        let content = lines.join("\n");
        let out = crush_search(&content, &opts(), &Context::default(), &mut refs()).unwrap();
        for line in out.lines() {
            if let Some(path) = match_path(line) {
                let file_lines: Vec<&str> = content
                    .lines()
                    .filter(|l| match_path(l) == Some(path))
                    .collect();
                assert!(out.contains(file_lines[0]), "first hit of {path} lost");
                assert!(
                    out.contains(file_lines[file_lines.len() - 1]),
                    "last hit of {path} lost"
                );
            }
        }
    }

    #[test]
    fn a_signal_line_outranks_a_quiet_one() {
        assert!(score("src/a.rs:3: log.error(\"boom\")") > score("src/a.rs:4: pass"));
        assert!(score("a.rs:1: // TODO: revisit") > score("a.rs:2: let x = 1;"));
        // the boundary is a real word boundary: `RuntimeError` is a symbol name,
        // not a report that something failed
        assert_eq!(
            score("src/a.rs:3: raise RuntimeError(x)"),
            score("src/a.rs:4: pass")
        );
    }

    #[test]
    fn scoring_does_not_read_the_question_that_prompted_the_search() {
        // Query-dependent output rewrites bytes the provider is caching, which
        // costs more than any selection win.
        let mut lines = Vec::new();
        for i in 0..20 {
            lines.extend(hits(&format!("src/module_{i}.rs"), 5));
        }
        let content = lines.join("\n");
        let asking_about_five = Context {
            preceding_user_text: "what happens in module_5 exactly",
            ..Default::default()
        };
        let asking_about_nine = Context {
            preceding_user_text: "what happens in module_9 exactly",
            ..Default::default()
        };
        assert_eq!(
            crush_search(&content, &opts(), &asking_about_five, &mut refs()),
            crush_search(&content, &opts(), &asking_about_nine, &mut refs()),
        );
    }

    #[test]
    fn search_declines_as_lossless_but_compresses_as_recoverable() {
        let mut lines = Vec::new();
        for i in 0..20 {
            lines.extend(hits(&format!("src/module_{i}.rs"), 5));
        }
        let content = lines.join("\n");

        let lossless = EngineOptions {
            mode: crate::types::Mode::Lossless,
            ..opts()
        };
        assert!(crush_search(&content, &lossless, &Context::default(), &mut refs()).is_none());

        let mut collector = refs();
        let out = crush_search(&content, &opts(), &Context::default(), &mut collector).unwrap();
        assert!(chars_len(&out) < chars_len(&content));
        assert!(
            !collector.is_empty(),
            "recoverable mode must retain what it drops"
        );
        assert!(out.contains("ref:k_"), "got {out}");
    }

    #[test]
    fn a_flat_listing_of_deeply_similar_paths_is_what_production_sends_most() {
        // Distilled from real traffic rather than imagined. Across the two
        // 2026-09-29 Harbor runs, `search` is 18% of everything the proxy
        // elided into handles — 104 of 589 fragments — and the bench corpus
        // carries exactly ONE compressible sample of it, so the quality floor
        // for the second-largest real category rests on n=1.
        //
        // The real shape is not grep output. There is no `path:line:match`:
        // it is a flat list of long paths sharing a deep prefix, which is what
        // `find` and `ls -R` produce over an experiment tree. One measured
        // payload went 35,770 chars -> 3,300 (90.8%) behind a single handle.
        let mut lines = Vec::new();
        for shards in [10, 32] {
            for mult in ["1", "5p0"] {
                for seed in 0..12 {
                    lines.push(format!(
                        "./exp_data/models/cc_v4_rw_v2_bff_minngram20_{shards}shards_shard3_\
                         OH_eli5_vs_bigram_200k_train_0.1-open_lm_1b_swiglutorch-warm=5000-\
                         lr=0p003-wd=0p033-cd=3e-05-bs=256-mult={mult}-seed={seed}-\
                         tokens=143979520000.json"
                    ));
                }
            }
        }
        let content = lines.join("\n");

        let mut collector = refs();
        let out = crush_search(&content, &opts(), &Context::default(), &mut collector)
            .expect("a long flat path listing is compressible");
        assert!(
            chars_len(&out) < chars_len(&content) / 2,
            "the shared prefix is most of the bytes: {} of {}",
            chars_len(&out),
            chars_len(&content)
        );
        assert!(
            !collector.is_empty() && out.contains("ref:k_"),
            "what is dropped must stay retrievable: {out}"
        );
    }

    #[test]
    fn the_output_is_never_larger_than_the_input() {
        let mut lines = Vec::new();
        for i in 0..20 {
            // one-character hits: every marker costs more than the line it hides
            lines.push(format!("f{i}.rs:1:x"));
            lines.push(format!("f{i}.rs:2:y"));
            lines.push(format!("f{i}.rs:3:z"));
        }
        let content = lines.join("\n");
        if let Some(out) = crush_search(&content, &opts(), &Context::default(), &mut refs()) {
            assert!(chars_len(&out) < chars_len(&content));
        }
    }

    #[test]
    fn the_same_payload_compresses_to_the_same_bytes() {
        let mut lines = Vec::new();
        for i in 0..15 {
            lines.extend(hits(&format!("src/module_{i}.rs"), 7));
        }
        let content = lines.join("\n");
        let first = crush_search(&content, &opts(), &Context::default(), &mut refs()).unwrap();
        for _ in 0..4 {
            assert_eq!(
                crush_search(&content, &opts(), &Context::default(), &mut refs()).unwrap(),
                first
            );
        }
    }

    #[test]
    fn pythons_half_to_even_rounding_is_preserved() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(2.4), 2.0);
        assert_eq!(round_half_even(2.6), 3.0);
    }
}
