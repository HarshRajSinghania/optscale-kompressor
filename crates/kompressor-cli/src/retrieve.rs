//! `kompressor retrieve`: read back what a compression elided.

use std::path::PathBuf;

use kompressor_core::retrieve::{slice, RetrieveRange};
use kompressor_core::Config;
use kompressor_engine::refs::RefId;
use kompressor_store::types::{Lookup, RefStore, Tenant};
use kompressor_store::SqliteRefStore;
use serde_json::json;

use crate::compress::print_json;
use crate::error::{CliError, CliResult};

/// Arguments of `kompressor retrieve`.
pub struct RetrieveArgs {
    pub reference: String,
    pub refs_path: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub tenant: String,
    pub lines: Option<String>,
    pub pointer: Option<String>,
    pub grep: Option<String>,
    pub json: bool,
}

/// Fetch a fragment by its handle.
///
/// A miss exits non-zero and prints the hint the model would be given, because
/// the two ways a handle fails to resolve need different fixes: an expiry is a
/// retention setting, a plain miss is a handle from another tenant or another
/// conversation entirely.
pub fn retrieve(args: RetrieveArgs) -> CliResult {
    // From the configuration layer, not the compiled-in defaults: `compress`
    // writes its fragments to the store the configuration names, so reading
    // them back from `./data/refs.db` finds nothing on any install that ships
    // a config — which is every packaged one. `--refs` overrides the path, it
    // is not the only way to reach it.
    let config =
        Config::load(args.config.as_deref()).map_err(|e| CliError::Usage(e.to_string()))?;
    let path = args
        .refs_path
        .clone()
        .unwrap_or_else(|| config.refs.path.clone());
    let id = RefId::parse(args.reference.trim()).ok_or_else(|| {
        CliError::Usage(format!(
            "'{}' is not a handle: expected k_ followed by 12 url-safe characters",
            args.reference
        ))
    })?;
    let range = parse_range(&args)?;

    let store = SqliteRefStore::open(&path)
        .map_err(|e| CliError::runtime(&format!("opening {}", path.display()), e))?;
    let tenant = Tenant::new(args.tenant);

    // Touching: a fragment somebody is still asking for is a fragment worth
    // keeping, and the idle window is what the store measures.
    let lookup = store
        .get(&tenant, &id, true)
        .map_err(|e| CliError::runtime("reading the fragment store", e))?;

    match lookup {
        Lookup::Hit(record) => {
            let retrieved = slice(
                &record.fragment,
                range.as_ref(),
                config.retrieve.max_retrieve_chars,
            );
            let content = retrieved.content.clone().unwrap_or_default();
            if args.json {
                let mut payload = json!({
                    "reference": id.to_string(),
                    "tenant": tenant.as_str(),
                    "found": true,
                    "ranged": retrieved.ranged,
                    "truncated": retrieved.truncated,
                    "total_lines": retrieved.total_lines,
                    "total_chars": retrieved.total_chars,
                    "kind": record.kind,
                    "content": content,
                });
                // Present only when there is something to say, matching the
                // API and the proxy rather than emitting a null key.
                if let Some(hint) = &retrieved.range_hint {
                    payload["hint"] = json!(hint);
                }
                print_json(&payload)?;
            } else {
                print!("{content}");
                if !content.ends_with('\n') {
                    println!();
                }
                if let Some(hint) = &retrieved.range_hint {
                    eprintln!("note: {hint}");
                }
                if retrieved.truncated {
                    eprintln!(
                        "note: truncated to {} chars of {}; narrow it with --lines, --pointer or \
                         --grep",
                        content.chars().count(),
                        retrieved.total_chars
                    );
                }
            }
            Ok(())
        }
        Lookup::Expired { expired_at } => miss(&args.json, &id, &tenant, true, Some(expired_at)),
        Lookup::Miss => miss(&args.json, &id, &tenant, false, None),
    }
}

/// Report a handle that did not resolve, and fail.
fn miss(
    json_out: &bool,
    id: &RefId,
    tenant: &Tenant,
    expired: bool,
    expired_at: Option<u64>,
) -> CliResult {
    let retrieved = if expired {
        kompressor_core::retrieve::Retrieved::expired()
    } else {
        kompressor_core::retrieve::Retrieved::miss()
    };
    if *json_out {
        print_json(&json!({
            "reference": id.to_string(),
            "tenant": tenant.as_str(),
            "found": false,
            "expired": expired,
            "expired_at": expired_at,
            "hint": retrieved.miss_hint(),
        }))?;
    }
    Err(CliError::Runtime(format!(
        "{id}: {}",
        retrieved.miss_hint()
    )))
}

/// Turn the mutually exclusive range flags into one range.
fn parse_range(args: &RetrieveArgs) -> Result<Option<RetrieveRange>, CliError> {
    if let Some(lines) = &args.lines {
        let (start, end) = lines
            .split_once(':')
            .ok_or_else(|| CliError::Usage(format!("--lines wants START:END, got '{lines}'")))?;
        let start: usize = start
            .trim()
            .parse()
            .map_err(|_| CliError::Usage(format!("--lines start '{start}' is not a number")))?;
        let end: usize = end
            .trim()
            .parse()
            .map_err(|_| CliError::Usage(format!("--lines end '{end}' is not a number")))?;
        if start == 0 {
            return Err(CliError::Usage(
                "--lines is 1-based, the way an editor numbers a file".to_string(),
            ));
        }
        if end < start {
            return Err(CliError::Usage(format!(
                "--lines {start}:{end} ends before it starts"
            )));
        }
        return Ok(Some(RetrieveRange::Lines { start, end }));
    }
    if let Some(pointer) = &args.pointer {
        return Ok(Some(RetrieveRange::JsonPointer {
            pointer: pointer.clone(),
        }));
    }
    if let Some(pattern) = &args.grep {
        return Ok(Some(RetrieveRange::Grep {
            pattern: pattern.clone(),
            context: 2,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> RetrieveArgs {
        RetrieveArgs {
            reference: "k_AAAAAAAAAAAA".to_string(),
            refs_path: None,
            config: None,
            tenant: String::new(),
            lines: None,
            pointer: None,
            grep: None,
            json: false,
        }
    }

    #[test]
    fn a_line_window_is_parsed_as_the_editor_writes_it() {
        let parsed = parse_range(&RetrieveArgs {
            lines: Some("10:20".into()),
            ..args()
        })
        .unwrap();
        assert_eq!(parsed, Some(RetrieveRange::Lines { start: 10, end: 20 }));
    }

    #[test]
    fn a_backwards_line_window_is_a_usage_error_rather_than_an_empty_answer() {
        let error = parse_range(&RetrieveArgs {
            lines: Some("20:10".into()),
            ..args()
        })
        .unwrap_err();
        assert_eq!(error.code(), 2);
    }

    #[test]
    fn a_zero_line_start_is_refused_because_line_numbering_starts_at_one() {
        let error = parse_range(&RetrieveArgs {
            lines: Some("0:10".into()),
            ..args()
        })
        .unwrap_err();
        assert_eq!(error.code(), 2);
    }

    #[test]
    fn a_grep_range_carries_surrounding_context() {
        let parsed = parse_range(&RetrieveArgs {
            grep: Some("ERROR".into()),
            ..args()
        })
        .unwrap();
        assert_eq!(
            parsed,
            Some(RetrieveRange::Grep {
                pattern: "ERROR".into(),
                context: 2
            })
        );
    }
}
