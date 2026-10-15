# The `tabular` bucket compresses nothing, and the compressor is not why

> **Resolved 2026-09-23.** The ratio is in, behind `enable_tabular_ratio`
> (default on), and the three steps below were carried out:
>
> 1. **Threshold 0.5**, a majority like `search`, `log` and `listing`. The nine
>    payloads top out at 23%, so it clears them with room.
> 2. **They land in `text`, not `log`** — the guess recorded below was wrong.
>    Six of the nine gain (+6 to +77 points); three are too small to compress
>    either way and become `untouched:text`.
> 3. **No payload that is a table stopped being detected.** The six real ones
>    stay `tabular` and now average 76.96% where the bucket used to average
>    28.12%; corpus coverage 68.7817% -> 69.9483%, and nothing lost ground.
>
> A second cause, not diagnosed below, was found at the same time:
> `looks_delimited` counted every delimiter, so an argument list of fixed arity
> — `step_11 = compute(entries, 11, offset=154)` down a file, 91 of 100 lines
> consistent — read as CSV. It now counts only delimiters outside quotes and
> outside any bracket pair. Measured on a live codex result, the half of a
> Python file after a truncation marker went **0.00% -> 86.82%**.
>
> The rest of this document is the original diagnosis, kept because it is the
> measurement the fix was chosen against.

`tabular` removes **0.38%** of the corpus payloads it claims, and has never
appeared in the `by type` line of any Harbor run. That reads as a weak
compressor. It is not: of the nine corpus payloads the detector classifies as
`tabular`, **not one is a table**. The TabularCompressor declines because it is
handed prose, and it is right to.

This is the record of that investigation so nobody repeats it.

## What the nine payloads actually are

```
t01, t03, t05   HuggingFace dataset cards — YAML front matter, prose, one markdown table
t02             a Markdown README fetched over HTTP ("200 # MTEB Leaderboard")
t04, t08        curl progress meter ("% Total  % Received  % Xferd …") then a dataset card
t06, t07, t09   rustc diagnostics ("error[E0758]: unterminated block comment")
```

Three README-shaped, two progress meters, three compiler errors. No CSV, no TSV,
no data table.

## What claims them

[`detect.rs`](../crates/kompressor-engine/src/detect.rs), the rule after `build`:

```rust
if TABLE.find_iter(sample).count() >= 3 || looks_delimited(sample) {
    return Detection::new(ContentType::Tabular, 0.8);
}
```

`looks_delimited` is false for all nine — the first line has no `\t` or `,`
columns. Every one is claimed by the left-hand branch, where
`TABLE` is `^\s*\|.+\|\s*$`, a markdown table row:

| payload | markdown-table rows in the sample |
|---|---|
| t01 / t04 / t08 | 15 |
| t02 | 14 |
| t06 | 3 |

A dataset card is mostly YAML and prose with one small table in it. Fifteen
matching rows out of 183 lines wins the whole payload for `tabular`.

## The asymmetry that allows it

Every neighbouring rule in the same function pairs a count with a *share*:

```rust
if search_lines >= 3 && search_ratio >= SEARCH_RATIO          { … }
if log_lines    >= 5 && log_lines    / line_count >= 0.6      { … }
if build_lines  >= 5 && build_lines  / line_count >= BUILD_RATIO { … }
if TABLE.find_iter(sample).count() >= 3 || looks_delimited(sample) { … }  // no ratio
```

The tabular rule is the only one with no ratio test, so three table rows
anywhere in an 8 KB sample are enough — however little of the payload they are.
t06 reaches the threshold on rustc's `|` gutter alone.

## What it costs

Routing the same nine by content instead — forcing `--type log`, which is what
prose-with-structure would otherwise get:

```
              as detected   as log
t01   8,862       8,862      2,041   (76% removed)
t04   8,984       8,920      2,075   (76%)
t08   8,984       8,891      2,041   (77%)
t02   4,122       4,122      3,517   (14%)
t03   4,241       4,241      4,196   ( 1%)
t05   4,241       4,241      4,196   ( 1%)
t06     566         566        566   ( 0%)
t07     625         625        625   ( 0%)
t09     673         673        673   ( 0%)
TOTAL 41,298      41,141     19,930  (51% removed)
```

**41,298 → 19,930, or 51%, against the 0.38% they get today.** Three payloads
carry almost all of it: the dataset cards and curl transcripts at 76–77%, which
the LogCompressor folds because they are full of repeated warning lines and
progress redraws.

The three rustc diagnostics gain nothing on any route — they are 566–673
characters, below anything worth taking. They are misrouted but the misrouting
is free.

## What to do about it, and what not to

The fix is one condition: require the table rows to be a share of the payload,
the way the three rules above it do. Something like
`rows >= 3 && rows as f32 / line_count >= TABLE_RATIO`.

It is deliberately **not** done here, because the number above is an upper bound
measured by forcing a route, not by re-detecting. The honest sequence is:

1. Add the ratio, pick the threshold against the corpus.
2. Check where the nine land once tabular stops claiming them — `log` is the
   guess, not the measurement, and `t01`'s YAML front matter may well go to
   `config`.
3. Confirm no payload that *is* a table stops being detected. The corpus has
   `untouched:tabular n=7` and `tabular n=8`; those eight are the regression
   risk and they are what `make bench` would catch.

Until that is done the bucket's 0.38% should be read as "the detector is sending
it prose", not as a verdict on the compressor.

## Not to be confused with `diff`

`diff` is the other bucket that looks inert, and its cause is the opposite:
detection is correct on every payload measured (0.95 confidence on six live git
diffs) and three deliberate gates decline all six. That one is working as
designed — see §7.6 of [`DESIGN.md`](DESIGN.md), which also records the trap
that cost an hour here: a 40 KB diff that compresses 91% turned out to be the
over-long-line clip taking one 37 KB line, not the diff compressor. When a
payload compresses, check *which* transform claimed it before crediting the one
the detector named.
