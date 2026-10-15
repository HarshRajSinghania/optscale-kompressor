#!/usr/bin/env python3
"""Attribute each proxied request to the terminal-bench task that made it.

    scripts/harbor/attribute.py <run-dir> [<run-dir> ...]

A suite runs several trials at once against one proxy, so `proxy_requests` is
an interleaved stream with no task on it. codex writes a `token_usage_record`
per request into each trial's own rollout, carrying the provider's accounting
for that request. Matching those two on **(input_tokens, cached_input_tokens)**
and then on time recovers the task: a pair of exact token counts is far more
selective than a timestamp, and the timestamp only has to break ties.

Ambiguous matches are dropped and counted rather than guessed at, and the
reconciliation at the end says what fraction of the run was attributed --
a number worth reading before the per-task table.
"""
import bisect
import collections
import datetime as dt
import glob
import json
import os
import sqlite3
import re
import sys

TRIAL_DIR = re.compile(r"^(?P<task>[a-z0-9][a-z0-9.-]*)__[A-Za-z0-9]{5,12}$")


def rows(db, sql):
    if not os.path.exists(db):
        return []
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in con.execute(sql)]
    except sqlite3.Error:
        return []
    finally:
        con.close()


def proxy_rows(run_dir):
    db = os.path.join(run_dir, "kompressor-stats.db")
    return rows(db, """
        SELECT pr.time_ms, pr.request_id, pr.prompt_tokens, pr.cache_read_tokens,
               pr.completion_tokens, pr.retrieve_rounds, pr.error,
               oe.tokens_in, oe.tokens_out, oe.chars_in, oe.chars_out,
               oe.refs_emitted, oe.content_types,
               oe.messages_total, oe.messages_compressed, oe.messages_skipped
          FROM proxy_requests pr
          LEFT JOIN optimization_events oe
                 ON oe.request_id = pr.request_id AND oe.surface = 'proxy'
         ORDER BY pr.time_ms""")


def trial_usage(run_dir):
    """Every codex request, with the task that made it."""
    out = []
    for path in glob.glob(os.path.join(run_dir, "**", "rollout-*.jsonl"),
                          recursive=True):
        # <run>/<job>/<task>__<id>/agent/sessions/...
        # Matched, not split: the run directory is also `<date>__<time>`, and a
        # task name may contain a dot (install-windows-3.11).
        task = None
        for part in path.split(os.sep):
            m = TRIAL_DIR.match(part)
            if m:
                task = m.group("task")
        if not task:
            continue
        try:
            for line in open(path, encoding="utf-8"):
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                if e.get("type") != "token_usage_record":
                    continue
                u = (e.get("payload") or {}).get("usage") or {}
                if not u.get("input_tokens"):
                    continue
                when = dt.datetime.fromisoformat(
                    e["timestamp"].replace("Z", "+00:00")).timestamp() * 1000
                out.append({"task": task, "time_ms": when,
                            "input_tokens": u["input_tokens"],
                            "cached": u.get("cached_input_tokens") or 0})
        except OSError:
            continue
    return out


def attribute(pr, usage):
    """Match on the exact token pair, breaking ties by time."""
    by_key = collections.defaultdict(list)
    for u in usage:
        by_key[(u["input_tokens"], u["cached"])].append(u)
    for v in by_key.values():
        v.sort(key=lambda u: u["time_ms"])

    matched, ambiguous, unmatched = [], 0, 0
    used = set()
    for r in pr:
        key = (r["prompt_tokens"], r["cache_read_tokens"])
        cands = [u for u in by_key.get(key, []) if id(u) not in used]
        if not cands:
            unmatched += 1
            continue
        if len(cands) > 1:
            # the proxy records the request start, codex the response; the
            # right candidate is the nearest one AFTER it
            after = [u for u in cands if u["time_ms"] >= r["time_ms"] - 2000]
            pool = after or cands
            best = min(pool, key=lambda u: abs(u["time_ms"] - r["time_ms"]))
            second = sorted(pool, key=lambda u: abs(u["time_ms"] - r["time_ms"]))
            if len(second) > 1 and abs(second[1]["time_ms"] - r["time_ms"]) \
                    - abs(best["time_ms"] - r["time_ms"]) < 250:
                ambiguous += 1
                continue
        else:
            best = cands[0]
        used.add(id(best))
        matched.append((best["task"], r))
    return matched, ambiguous, unmatched


def report(run_dir):
    pr = proxy_rows(run_dir)
    comp = [r for r in pr if r.get("tokens_in")]
    if not comp:
        print(f"{run_dir}: no compression events")
        return None
    usage = trial_usage(run_dir)
    matched, ambiguous, unmatched = attribute(pr, usage)

    per = collections.defaultdict(lambda: collections.Counter())
    types = collections.defaultdict(collections.Counter)
    for task, r in matched:
        p = per[task]
        p["requests"] += 1
        for k in ("tokens_in", "tokens_out", "chars_in", "chars_out",
                  "prompt_tokens", "cache_read_tokens", "refs_emitted"):
            p[k] += r.get(k) or 0
        try:
            for k, v in json.loads(r.get("content_types") or "{}").items():
                types[task][k] += v
        except (TypeError, ValueError):
            pass

    ti = sum(r["tokens_in"] or 0 for r in comp)
    got = sum(p["tokens_in"] for p in per.values())
    print(f"\n=== {os.path.basename(os.path.dirname(run_dir))}/"
          f"{os.path.basename(run_dir)} ===")
    print(f"  {len(matched)} of {len(pr)} requests attributed "
          f"({ambiguous} ambiguous, {unmatched} unmatched); "
          f"{100 * got / ti if ti else 0:.1f}% of compressible tokens covered")
    return per, types


def main():
    dirs = sys.argv[1:]
    if not dirs:
        sys.exit(__doc__)
    total = collections.defaultdict(lambda: collections.Counter())
    total_types = collections.defaultdict(collections.Counter)
    for d in dirs:
        got = report(d)
        if not got:
            continue
        per, types = got
        for task, p in per.items():
            total[task].update(p)
        for task, t in types.items():
            total_types[task].update(t)

    print(f"\n{'task':<34}{'reqs':>5}{'ctx in':>11}{'-> out':>11}{'removed%':>9}"
          f"{'chars%':>8}{'refs':>7}  top types")
    order = sorted(total.items(),
                   key=lambda kv: -(1 - kv[1]["tokens_out"] / kv[1]["tokens_in"])
                   if kv[1]["tokens_in"] else 0)
    for task, p in order:
        if not p["tokens_in"]:
            continue
        pct = 100 * (1 - p["tokens_out"] / p["tokens_in"])
        cpct = (100 * (1 - p["chars_out"] / p["chars_in"])) if p["chars_in"] else 0
        tops = " ".join(f"{k}:{v}" for k, v in total_types[task].most_common(3))
        print(f"{task:<34}{p['requests']:>5}{p['tokens_in']:>11,}"
              f"{p['tokens_out']:>11,}{pct:>9.1f}{cpct:>8.1f}"
              f"{p['refs_emitted']:>7,}  {tops}")
    agg_in = sum(p["tokens_in"] for p in total.values())
    agg_out = sum(p["tokens_out"] for p in total.values())
    if agg_in:
        print(f"{'ALL ATTRIBUTED':<34}{sum(p['requests'] for p in total.values()):>5}"
              f"{agg_in:>11,}{agg_out:>11,}{100 * (1 - agg_out / agg_in):>9.1f}")


if __name__ == "__main__":
    main()
