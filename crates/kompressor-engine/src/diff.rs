//! Conservative unified-diff selection: whole files, then context inside them.
//!
//! A patch is two things at once — a list of files that changed and, per file, a
//! set of hunks with surrounding context. Both are sampled here, and both
//! omissions are announced, because a diff that quietly loses a file reads
//! exactly like a diff where that file did not change.

use std::collections::HashSet;

use crate::markers::omission_marker;
use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::refs::RefCollector;
use crate::text::{chars_len, is_blank, join_lines, split_lines};
use crate::types::Context;

/// Below this, a patch is small enough that sampling buys nothing worth the
/// markers it costs.
const MIN_LINES: usize = 50;

/// Unchanged lines kept on each side of a change.
const CONTEXT: usize = 2;

/// A kept patch has to lose at least this share of its lines to be worth
/// shipping; a marginal shrink is churn on a cached prefix.
const MAX_KEPT_RATIO: f64 = 0.8;

/// Compress a unified diff by dropping the least-changed files and narrowing
/// the context around each surviving hunk.
pub fn crush_diff(
    content: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // Neither dropped hunks nor narrowed context can be rebuilt from the output,
    // so lossless declines. The reference engine had no mode check here, which
    // made lossless mode silently lose diff context.
    if opts.declines_as_lossless() {
        return None;
    }

    // The harness frame is not diff content, so `reduce_context` classes it as
    // neither a header nor a changed line and trims it away — taking `Process
    // exited with code N` with it. Hold it out, as the log and code paths do.
    let (preamble, body) = split_harness_preamble(content);
    if !preamble.is_empty() {
        let crushed = crush_body(body, opts, ctx, refs)?;
        let out = format!("{preamble}{crushed}");
        return (chars_len(&out) < chars_len(content)).then_some(out);
    }
    crush_body(content, opts, ctx, refs)
}

fn crush_body(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    let lines = split_lines(content);
    if lines.len() < MIN_LINES {
        return None;
    }

    let mut files: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in &lines {
        if line.starts_with("diff --git ") && !current.is_empty() {
            files.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        files.push(current);
    }
    // A single-file patch has no file to choose between, and narrowing its
    // context alone is not worth the loss.
    if files.len() <= 1 {
        return None;
    }

    let mut ranked: Vec<usize> = (0..files.len()).collect();
    ranked.sort_by_key(|&index| (std::cmp::Reverse(changes(&files[index])), index));
    let keep: HashSet<usize> = ranked.into_iter().take(opts.diff_max_files).collect();

    let mut out: Vec<String> = Vec::new();
    let mut omitted: Vec<&str> = Vec::new();
    for (index, chunk) in files.iter().enumerate() {
        if !keep.contains(&index) {
            omitted.extend(chunk.iter().copied());
            continue;
        }
        let (kept, trimmed) = reduce_context(chunk, CONTEXT);
        out.extend(kept.into_iter().map(str::to_string));
        if !trimmed.is_empty() {
            // Narrowing a hunk is an omission like any other. Only whole *files*
            // used to be marked, so context and `index <blob>..<blob>` lines
            // dropped inside a kept file left no trace — 20 context lines went
            // from one payload on a real run with nothing in the output saying
            // so. The surviving `@@` headers already misreport their lengths
            // once a hunk is narrowed, so the patch is not applicable either
            // way; what must not happen is the model being unable to tell.
            out.push(omission_marker(
                "diff",
                trimmed.len(),
                &join_lines(&trimmed),
                refs,
                &format!(" of context in {}", path_of(chunk)),
            ));
        }
    }
    if !omitted.is_empty() {
        out.push(omission_marker(
            "diff",
            omitted.len(),
            &join_lines(&omitted),
            refs,
            "",
        ));
    }

    let result = join_lines(&out);
    let kept_lines = split_lines(&result).len() as f64;
    let accept =
        kept_lines < lines.len() as f64 * MAX_KEPT_RATIO && chars_len(&result) < chars_len(content);
    accept.then_some(result)
}

/// Whether `line` is an added or removed line rather than a file header.
fn is_change(line: &str) -> bool {
    (line.starts_with('+') || line.starts_with('-'))
        && !line.starts_with("+++")
        && !line.starts_with("---")
}

fn changes(lines: &[&str]) -> usize {
    lines.iter().filter(|line| is_change(line)).count()
}

/// The path a chunk is about, for the marker that narrows its context.
fn path_of<'a>(chunk: &[&'a str]) -> &'a str {
    for line in chunk {
        if let Some(rest) = line.strip_prefix("+++ ") {
            return rest.strip_prefix("b/").unwrap_or(rest);
        }
        if line.starts_with("diff --git ") {
            return match line.rfind(" b/") {
                Some(at) => &line[at + 3..],
                None => line,
            };
        }
    }
    "the patch"
}

/// Keep headers and changed lines plus `limit` lines of context around each
/// change, returning `(kept, dropped)` so the caller can account for the loss.
///
/// Blank lines are left out of `dropped`: they carry nothing to retrieve, and
/// counting them would inflate every marker with whitespace.
fn reduce_context<'a>(lines: &[&'a str], limit: usize) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut keep: HashSet<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.starts_with("diff --git ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("@@ ")
        })
        .map(|(index, _)| index)
        .collect();
    for (index, _) in lines.iter().enumerate().filter(|(_, l)| is_change(l)) {
        for around in index.saturating_sub(limit)..(index + limit + 1).min(lines.len()) {
            keep.insert(around);
        }
    }
    let kept = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, line)| *line)
        .collect();
    let dropped = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !keep.contains(index) && !is_blank(line))
        .map(|(_, line)| *line)
        .collect();
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs() -> RefCollector {
        RefCollector::new(1 << 20)
    }

    /// One file's worth of patch: a header, a hunk, and `context` unchanged
    /// lines trailing the single change.
    fn file_chunk(path: &str, changed: usize, context: usize) -> Vec<String> {
        let mut out = vec![
            format!("diff --git a/{path} b/{path}"),
            "index 1111111..2222222 100644".to_string(),
            format!("--- a/{path}"),
            format!("+++ b/{path}"),
            format!(
                "@@ -1,{} +1,{} @@ fn thing()",
                context + changed,
                context + changed
            ),
        ];
        for i in 0..changed {
            out.push(format!("-    let removed_{i} = {i};"));
            out.push(format!("+    let added_{i} = {i} + 1;"));
        }
        for i in 0..context {
            out.push(format!("     untouched context line number {i}"));
        }
        out
    }

    fn patch(files: usize, changed: usize, context: usize) -> String {
        let mut lines = Vec::new();
        for i in 0..files {
            lines.extend(file_chunk(&format!("src/module_{i}.rs"), changed, context));
        }
        lines.join("\n")
    }

    #[test]
    fn a_short_patch_is_left_alone() {
        let content = patch(2, 2, 2);
        assert!(split_lines(&content).len() < MIN_LINES);
        assert!(crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs()
        )
        .is_none());
    }

    #[test]
    fn a_single_file_patch_is_left_alone() {
        // There is no file to choose between, and narrowing the only hunk is not
        // worth the loss on its own.
        let content = patch(1, 20, 30);
        assert!(split_lines(&content).len() >= MIN_LINES);
        assert!(crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs()
        )
        .is_none());
    }

    #[test]
    fn diff_context_trim_is_counted() {
        // 20 context lines once left a payload with nothing in the output saying
        // so; a narrowed hunk must name its own loss and its file.
        let content = patch(2, 3, 20);
        let out = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.contains("of context in src/module_0.rs"), "got {out}");
        assert!(out.contains("diff lines omitted"), "got {out}");
    }

    #[test]
    fn dropped_files_are_counted_in_one_marker() {
        let options = EngineOptions {
            diff_max_files: 2,
            ..Default::default()
        };
        let content = patch(6, 3, 2);
        let out = crush_diff(&content, &options, &Context::default(), &mut refs()).unwrap();
        // four files leave; none of their headers may remain
        assert!(
            !out.contains("src/module_5.rs"),
            "a dropped file leaked: {out}"
        );
        assert!(out.contains("diff lines omitted"), "got {out}");
    }

    #[test]
    fn diff_keeps_the_harness_frame() {
        let body = patch(4, 3, 12);
        let content = format!(
            "Chunk ID: abc\nWall time: 1.2 seconds\nProcess exited with code 1\nOutput:\n{body}"
        );
        let out = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.starts_with("Chunk ID: abc\n"), "got {out}");
        assert!(
            out.contains("Process exited with code 1"),
            "the outcome line is the most load-bearing line in the payload: {out}"
        );
    }

    #[test]
    fn the_context_around_every_change_survives() {
        let content = patch(3, 2, 25);
        let out = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(
            out.contains("+    let added_0 = 0 + 1;"),
            "a change was lost: {out}"
        );
        assert!(
            out.contains("     untouched context line number 0"),
            "the line adjacent to a change is context, not filler: {out}"
        );
        assert!(
            !out.contains("untouched context line number 20"),
            "distant context should be gone: {out}"
        );
    }

    #[test]
    fn headers_are_kept_even_with_no_change_beside_them() {
        let content = patch(3, 2, 25);
        let out = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.contains("diff --git a/src/module_0.rs b/src/module_0.rs"));
        assert!(out.contains("--- a/src/module_0.rs"));
        assert!(out.contains("+++ b/src/module_0.rs"));
        assert!(out.contains("@@ -1,27 +1,27 @@ fn thing()"));
    }

    #[test]
    fn a_patch_that_barely_shrinks_is_refused() {
        // All context already inside the window: there is nothing to narrow, and
        // every file is kept, so the output would differ only in its newline.
        let content = patch(5, 6, 1);
        assert!(split_lines(&content).len() >= MIN_LINES);
        assert!(crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs()
        )
        .is_none());
    }

    #[test]
    fn the_marker_names_the_file_from_its_own_header() {
        let chunk: Vec<&str> = vec![
            "diff --git a/x/y.rs b/x/y.rs",
            "--- a/x/y.rs",
            "+++ b/x/y.rs",
        ];
        assert_eq!(path_of(&chunk), "x/y.rs");
        assert_eq!(path_of(&["+++ b/only.rs"]), "only.rs");
        assert_eq!(path_of(&["+++ /dev/null"]), "/dev/null");
        assert_eq!(path_of(&["@@ -1 +1 @@"]), "the patch");
    }

    #[test]
    fn diff_declines_as_lossless_but_compresses_as_recoverable() {
        let content = patch(6, 3, 20);

        let lossless = EngineOptions {
            mode: crate::types::Mode::Lossless,
            ..Default::default()
        };
        assert!(crush_diff(&content, &lossless, &Context::default(), &mut refs()).is_none());

        let mut collector = refs();
        let out = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut collector,
        )
        .unwrap();
        assert!(chars_len(&out) < chars_len(&content));
        assert!(
            !collector.is_empty(),
            "recoverable mode must retain what it drops"
        );
    }

    #[test]
    fn the_least_changed_files_are_the_ones_that_leave() {
        let mut lines = file_chunk("src/busy.rs", 10, 2);
        lines.extend(file_chunk("src/quiet.rs", 1, 12));
        lines.extend(file_chunk("src/also_quiet.rs", 1, 12));
        let content = lines.join("\n");
        let options = EngineOptions {
            diff_max_files: 1,
            ..Default::default()
        };
        let out = crush_diff(&content, &options, &Context::default(), &mut refs()).unwrap();
        assert!(out.contains("src/busy.rs"), "got {out}");
        assert!(!out.contains("src/quiet.rs"), "got {out}");
    }

    #[test]
    fn the_same_patch_compresses_to_the_same_bytes() {
        let content = patch(6, 4, 15);
        let first = crush_diff(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        for _ in 0..4 {
            assert_eq!(
                crush_diff(
                    &content,
                    &EngineOptions::default(),
                    &Context::default(),
                    &mut refs()
                )
                .unwrap(),
                first
            );
        }
    }
}
