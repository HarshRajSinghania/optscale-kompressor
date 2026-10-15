//! Rendering a replay and a comparison as text.
//!
//! The rendering lives here rather than in the CLI because the numbers and
//! their labels belong together: a bucket table whose columns are explained in
//! one crate and produced in another drifts, and these columns mean specific
//! things — `claimed` is not `coverage`, `billed out` is not `chars out`.

use std::fmt::Write as _;

use crate::baseline::Comparison;
use crate::replay::ReplayReport;

impl ReplayReport {
    /// The replay as a table: headline metrics, buckets, timing, invariants.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        let coverage = self.coverage();

        let _ = writeln!(
            out,
            "payloads {} ({} eligible at >= {} chars)",
            self.results.len(),
            self.results.iter().filter(|r| r.eligible).count(),
            self.options.min_compress_chars
        );
        let _ = writeln!(
            out,
            "mode {}  chars {} -> {}",
            self.options.mode, coverage.chars_in, coverage.chars_out
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  coverage  {:>8.2}%   eligible bytes removed",
            coverage.pct
        );
        let _ = writeln!(
            out,
            "  claimed   {:>8.2}%   eligible bytes some compressor took",
            self.claimed_pct()
        );
        match self.billed() {
            Some(billed) => {
                let _ = writeln!(
                    out,
                    "  billed    {:>8.2}%   coverage weighted by requests (cache read {:.2}x)",
                    billed.pct, billed.cache_read_rate
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  billed         n/a   this corpus records no request counts"
                );
            }
        }

        let buckets = self.buckets();
        if !buckets.is_empty() {
            let mut rows: Vec<_> = buckets.into_iter().collect();
            // by what the bucket still costs over a conversation: the top of
            // this list is where the next compressor is worth writing
            rows.sort_by(|a, b| {
                b.1.billed_out
                    .partial_cmp(&a.1.billed_out)
                    .expect("no NaN in a weighted size")
            });
            let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(6).max(6);
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "  {:<width$}  {:>5}  {:>10}  {:>10}  {:>7}  {:>12}",
                "bucket", "n", "chars in", "chars out", "saved", "billed out"
            );
            for (name, stats) in rows {
                let _ = writeln!(
                    out,
                    "  {:<width$}  {:>5}  {:>10}  {:>10}  {:>6.2}%  {:>12.0}",
                    name,
                    stats.payloads,
                    stats.chars_in,
                    stats.chars_out,
                    stats.saved_pct,
                    stats.billed_out
                );
            }
        }

        let bands = self.size_bands();
        if !bands.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "  {:<10}  {:>5}  {:>10}  {:>10}  {:>7}",
                "size", "n", "chars in", "chars out", "saved"
            );
            for (label, stats) in bands {
                let _ = writeln!(
                    out,
                    "  {:<10}  {:>5}  {:>10}  {:>10}  {:>6.2}%",
                    // the sort key is a leading digit; it is not the label
                    label
                        .split_once(' ')
                        .map_or(label.as_str(), |(_, rest)| rest),
                    stats.payloads,
                    stats.chars_in,
                    stats.chars_out,
                    stats.saved_pct
                );
            }
        }

        let timing = self.timing();
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  timing    {:.4} ms/KB   p50 {:.2}  p95 {:.2}  max {:.2}  mean {:.2}",
            timing.ms_per_kb, timing.p50_ms, timing.p95_ms, timing.max_ms, timing.mean_ms
        );

        let violations = self.violations();
        if violations.is_empty() {
            let _ = writeln!(
                out,
                "  safety    no invariant violated{}",
                if self.determinism_verified {
                    " (determinism and query stability verified)"
                } else {
                    " (determinism not checked)"
                }
            );
        } else {
            let _ = writeln!(out, "  SAFETY VIOLATIONS");
            for (name, count) in &violations {
                let _ = writeln!(out, "    {name}: {count} payload(s)");
            }
            for result in self.results.iter().filter(|r| !r.violations.is_empty()) {
                for violation in &result.violations {
                    let _ = writeln!(out, "    {} {}", result.content_id, violation);
                }
            }
        }

        out
    }
}

impl Comparison {
    /// The comparison as text, ending in the verdict.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "  {:<14}  {:>10}  {:>10}  {:>9}",
            "metric", "baseline", "current", "delta"
        );
        for metric in &self.headline {
            let _ = writeln!(
                out,
                "  {:<14}  {:>10.4}  {:>10.4}  {:>+9.4}{}",
                metric.name,
                metric.baseline,
                metric.current,
                metric.delta,
                if metric.regressed { "  REGRESSED" } else { "" }
            );
        }

        if !self.buckets.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "  buckets, over payloads in the same bucket before and after:"
            );
            for bucket in &self.buckets {
                let _ = writeln!(
                    out,
                    "  {:<24}  n={:<4}  {:>8.4} -> {:>8.4}  {:>+8.4}{}",
                    bucket.name,
                    bucket.stable_payloads,
                    bucket.baseline_pct,
                    bucket.current_pct,
                    bucket.delta,
                    if bucket.regressed { "  REGRESSED" } else { "" }
                );
            }
        }

        if !self.membership.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "  {} payload(s) changed bucket; a move is an improvement when its own delta is \
                 positive, whatever it did to either average:",
                self.membership.len()
            );
            for change in self.membership.iter().take(20) {
                let _ = writeln!(
                    out,
                    "    {} {} -> {} ({} chars, {:+.2} points)",
                    change.content_id, change.from, change.to, change.chars_in, change.own_delta
                );
            }
            if self.membership.len() > 20 {
                let _ = writeln!(out, "    ... and {} more", self.membership.len() - 20);
            }
        }

        for note in &self.notes {
            let _ = writeln!(out, "  note: {note}");
        }
        if self.passed {
            let _ = writeln!(out, "  PASS");
        } else {
            for regression in &self.regressions {
                let _ = writeln!(out, "  regression: {regression}");
            }
            let _ = writeln!(out, "  FAIL");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::corpus::Payload;
    use crate::replay::replay;
    use kompressor_engine::options::EngineOptions;

    #[test]
    fn the_table_names_every_headline_metric() {
        let payload = Payload {
            content_id: "a".into(),
            role: "tool".into(),
            content: (0..40)
                .map(|_| "2026-01-01T00:00:00Z INFO heartbeat ok")
                .collect::<Vec<_>>()
                .join("\n"),
            preceding_user_text: String::new(),
            source_tool: "exec_command".into(),
            occurrences: 1,
            requests: 4,
            task: "t".into(),
        };
        let table = replay(&[payload], &EngineOptions::default(), true).render_table();
        for expected in ["coverage", "claimed", "billed", "timing", "safety", "log"] {
            assert!(
                table.contains(expected),
                "{expected} missing from:\n{table}"
            );
        }
    }

    #[test]
    fn the_table_says_billed_is_unmeasured_rather_than_zero() {
        let payload = Payload {
            content_id: "a".into(),
            role: "tool".into(),
            content: "x".repeat(600),
            preceding_user_text: String::new(),
            source_tool: String::new(),
            occurrences: 1,
            requests: 0,
            task: String::new(),
        };
        let table = replay(&[payload], &EngineOptions::default(), false).render_table();
        assert!(table.contains("billed         n/a"), "{table}");
    }
}
