//! `kompressor stats`: what the service has been doing, read from `stats.db`.
//!
//! Every report here prints net alongside gross. Gross is
//! `Σ(tokens_in - tokens_out)` and it is the number that makes compression look
//! good; net subtracts what retrieval put back, and it can be negative. A
//! caller shown only gross is being shown a number this workspace knows to be
//! wrong, so the table form never prints one without the other.

use std::path::PathBuf;

use kompressor_core::Config;
use kompressor_stats::{Bucket, StatsReader};
use serde_json::json;

use crate::compress::print_json;
use crate::error::{CliError, CliResult};

/// Which report to print.
pub enum Report {
    Summary,
    Histogram {
        bucket: Bucket,
    },
    ByType,
    ByProvider,
    Retrievals,
    Request {
        id: String,
    },
    Store,
    /// One self-contained HTML page, to stdout.
    Html {
        bucket: Bucket,
    },
}

/// Arguments of `kompressor stats`.
pub struct StatsArgs {
    pub report: Report,
    pub stats_path: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub tenant: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub json: bool,
}

/// Run one statistics report.
pub fn stats(args: StatsArgs) -> CliResult {
    // The window is parsed before anything is opened, so a misspelled `--from`
    // reports itself as the usage error it is even when the database is also
    // missing — the first error a caller sees should be the one they can fix.
    //
    // The default window is all of time. A window that defaulted to "today"
    // would report zeros on a database written yesterday, which reads as "the
    // service is doing nothing".
    let from = parse_instant(args.from.as_deref(), 0)?;
    let to = parse_instant(args.to.as_deref(), i64::MAX)?;
    if to < from {
        return Err(CliError::Usage(format!("--from {from} is after --to {to}")));
    }

    // `--stats` wins; otherwise the configured path, read the same way every
    // other command reads it. Taking `Config::default()` here instead made the
    // command report "no statistics database" against `./data/stats.db` while
    // the running server wrote the path its config named — the one case where
    // this is asked, since a default-configured server is answered by either.
    let path = match args.stats_path {
        Some(path) => path,
        None => {
            Config::load(args.config.as_deref())
                .map_err(|e| CliError::Usage(e.to_string()))?
                .stats
                .path
        }
    };
    if !path.exists() {
        return Err(CliError::Runtime(format!(
            "{}: no statistics database. Nothing has recorded yet, or --stats points elsewhere.",
            path.display()
        )));
    }
    let reader = StatsReader::open(&path)
        .map_err(|e| CliError::runtime(&format!("opening {}", path.display()), e))?;
    let tenant = args.tenant.as_str();

    match args.report {
        Report::Summary => {
            let summary = reader
                .summary(tenant, from, to)
                .map_err(|e| CliError::runtime("reading the summary", e))?;
            if args.json {
                print_json(&summary)?;
            } else {
                println!("tenant              {}", summary.tenant);
                println!("requests            {}", summary.requests);
                println!(
                    "messages            {} total, {} compressed",
                    summary.messages_total, summary.messages_compressed
                );
                println!(
                    "tokens              {} in, {} out",
                    summary.tokens_in, summary.tokens_out
                );
                println!("saved gross         {}", summary.saved_gross);
                println!(
                    "retrieved back      {} over {} retrievals ({} hits, {:.1}% hit rate)",
                    summary.retrieved_tokens,
                    summary.retrievals,
                    summary.retrieval_hits,
                    summary.hit_rate * 100.0
                );
                println!("saved net           {}", summary.saved_net);
                for mode in &summary.by_mode {
                    println!(
                        "  mode {:<12} {} requests, {} gross",
                        mode.mode, mode.requests, mode.saved_gross
                    );
                }
                for surface in &summary.by_surface {
                    println!(
                        "  from {:<12} {} requests, {} gross",
                        surface.surface, surface.requests, surface.saved_gross
                    );
                }
            }
        }
        Report::Histogram { bucket } => {
            let rows = reader
                .histogram(tenant, from, to, bucket)
                .map_err(|e| CliError::runtime("reading the histogram", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!(
                    "{:<22}  {:>8}  {:>12}  {:>12}  {:>12}",
                    "bucket", "requests", "gross", "retrieved", "net"
                );
                for row in &rows {
                    println!(
                        "{:<22}  {:>8}  {:>12}  {:>12}  {:>12}",
                        row.label,
                        row.requests,
                        row.saved_gross,
                        row.retrieved_tokens,
                        row.saved_net
                    );
                }
            }
        }
        Report::ByType => {
            let rows = reader
                .by_type(tenant, from, to)
                .map_err(|e| CliError::runtime("reading type coverage", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!("{:<14}  {:>10}  {:>8}", "type", "messages", "share");
                for row in &rows {
                    println!(
                        "{:<14}  {:>10}  {:>7.2}%",
                        row.content_type,
                        row.messages,
                        row.share * 100.0
                    );
                }
            }
        }
        Report::ByProvider => {
            let rows = reader
                .by_provider(tenant, from, to)
                .map_err(|e| CliError::runtime("reading provider coverage", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!(
                    "{:<14}  {:>8}  {:>12}  {:>12}  {:>7}  sent to",
                    "provider", "requests", "tokens in", "removed", "share"
                );
                for row in &rows {
                    let name = if row.provider.is_empty() {
                        "(unnamed)"
                    } else {
                        row.provider.as_str()
                    };
                    println!(
                        "{:<14}  {:>8}  {:>12}  {:>12}  {:>6.2}%  {}",
                        name,
                        row.requests,
                        row.tokens_in,
                        row.saved,
                        row.ratio * 100.0,
                        row.hosts.join(", ")
                    );
                }
            }
        }
        Report::Retrievals => {
            let summary = reader
                .retrievals(tenant, from, to)
                .map_err(|e| CliError::runtime("reading retrievals", e))?;
            if args.json {
                print_json(&summary)?;
            } else {
                println!("retrievals     {}", summary.retrievals);
                println!(
                    "hits           {} ({:.1}%)",
                    summary.hits,
                    summary.hit_rate * 100.0
                );
                println!(
                    "misses         {} of which {} expired",
                    summary.misses, summary.expiries
                );
                println!(
                    "ranged         {} ({:.1}%)",
                    summary.ranged,
                    summary.ranged_share * 100.0
                );
                println!("tokens back    {}", summary.retrieved_tokens);
                // Printed whenever it is non-zero, and never folded into the
                // rate above. A hit rate of 100% beside eleven withheld asks is
                // a true statement about the store and a misleading one about
                // the request, and the only cure is to say both.
                if summary.withheld > 0 {
                    println!(
                        "withheld       {} — the per-request token budget refused these \
                         before the store was read; raise proxy.retrieve_max_tokens_per_request",
                        summary.withheld
                    );
                }
                for reference in &summary.top_refs {
                    print!(
                        "  {} {} asks, {} hits, {} tokens",
                        reference.reference,
                        reference.retrievals,
                        reference.hits,
                        reference.returned_tokens
                    );
                    if reference.withheld > 0 {
                        print!(", {} withheld", reference.withheld);
                    }
                    println!();
                }
            }
        }
        Report::Request { id } => {
            let economy = reader
                .request(tenant, &id)
                .map_err(|e| CliError::runtime("reading the request", e))?;
            match economy {
                Some(economy) => {
                    if args.json {
                        print_json(&economy)?;
                    } else {
                        println!("request        {}", economy.request_id);
                        println!("tenant         {}", economy.tenant);
                        println!(
                            "mode           {}",
                            economy.mode.as_deref().unwrap_or("(never compressed)")
                        );
                        println!("compressions   {}", economy.compressions);
                        println!(
                            "tokens         {} in, {} out",
                            economy.tokens_in, economy.tokens_out
                        );
                        println!("saved gross    {}", economy.saved_gross);
                        println!("saved net      {}", economy.saved_net);
                        if let Some(proxy) = &economy.proxy {
                            println!(
                                "provider       {} prompt tokens billed, {} from cache",
                                proxy.prompt_tokens.unwrap_or(0),
                                proxy.cache_read_tokens.unwrap_or(0)
                            );
                        }
                    }
                }
                None => {
                    if args.json {
                        print_json(&json!({"request_id": id, "found": false}))?;
                    }
                    return Err(CliError::Runtime(format!(
                        "no events recorded for request {id} under tenant '{tenant}'"
                    )));
                }
            }
        }
        Report::Html { bucket } => {
            // `--format json` is meaningless for a page, and silently ignoring
            // a flag the caller passed is how they end up wondering why their
            // pipeline got HTML.
            if args.json {
                return Err(CliError::Usage(
                    "stats report renders HTML; --format json is not available for it. The \
                     other reports take --format json and carry the same numbers."
                        .into(),
                ));
            }
            let data =
                crate::report::ReportData::gather(&reader, tenant, from, to, bucket, now_ms())
                    .map_err(|e| CliError::runtime("building the report", e))?;
            print!("{}", crate::report::render(&data));
        }
        Report::Store => {
            let sample = reader
                .store(tenant)
                .map_err(|e| CliError::runtime("reading the store sample", e))?;
            match sample {
                Some(sample) => {
                    if args.json {
                        print_json(&json!({
                            "time_ms": sample.time_ms,
                            "live_refs": sample.live_refs,
                            "live_bytes": sample.live_bytes,
                        }))?;
                    } else {
                        println!("sampled at     {}", sample.time_ms);
                        println!("live refs      {}", sample.live_refs);
                        println!("live bytes     {}", sample.live_bytes);
                    }
                }
                None => {
                    if args.json {
                        print_json(&json!({"found": false}))?;
                    } else {
                        println!("no store sample has been recorded yet");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse a window bound.
///
/// Three spellings, because the three questions asked of this are different:
/// `1700000000000` when copying a timestamp out of a row, `2026-08-28` when
/// looking at a day, and `7d` when asking what happened recently.
fn parse_instant(value: Option<&str>, default: i64) -> Result<i64, CliError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(default);
    };

    if let Ok(millis) = value.parse::<i64>() {
        return Ok(millis);
    }
    if let Some(ago) = parse_relative(value) {
        return Ok(now_ms().saturating_sub(ago));
    }
    if let Some(millis) = parse_date(value) {
        return Ok(millis);
    }
    Err(CliError::Usage(format!(
        "'{value}' is not a time: use epoch milliseconds, YYYY-MM-DD, or a span like 7d"
    )))
}

/// `30s`, `15m`, `24h`, `7d` as milliseconds.
fn parse_relative(value: &str) -> Option<i64> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    let amount: i64 = digits.parse().ok()?;
    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    amount.checked_mul(scale)
}

/// `YYYY-MM-DD` as milliseconds since the epoch, UTC midnight.
fn parse_date(value: &str) -> Option<i64> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400_000)
}

/// Days from 1970-01-01 to `y-m-d`, by Howard Hinnant's civil-date algorithm.
///
/// Written out rather than pulled in as a dependency: this is the only date
/// arithmetic in the CLI, and a calendar crate is a large surface to carry for
/// one function whose correctness is checkable against known dates in a test.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Wall clock in milliseconds, or zero if the clock is before the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_window_bound_takes_its_default() {
        assert_eq!(parse_instant(None, 0).unwrap(), 0);
        assert_eq!(parse_instant(Some("  "), i64::MAX).unwrap(), i64::MAX);
    }

    #[test]
    fn epoch_milliseconds_are_taken_literally() {
        assert_eq!(
            parse_instant(Some("1700000000000"), 0).unwrap(),
            1_700_000_000_000
        );
    }

    #[test]
    fn a_calendar_date_is_utc_midnight() {
        // 2023-11-14T00:00:00Z, checkable against any epoch converter.
        assert_eq!(
            parse_instant(Some("2023-11-14"), 0).unwrap(),
            1_699_920_000_000
        );
        // the epoch itself, and a leap day
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800_000));
    }

    #[test]
    fn a_span_is_measured_backwards_from_now() {
        let day_ago = parse_instant(Some("1d"), 0).unwrap();
        let delta = now_ms() - day_ago;
        assert!((86_400_000..86_400_000 + 5_000).contains(&delta), "{delta}");
    }

    #[test]
    fn an_unparseable_bound_is_a_usage_error() {
        assert_eq!(
            parse_instant(Some("last tuesday"), 0).unwrap_err().code(),
            2
        );
    }
}
