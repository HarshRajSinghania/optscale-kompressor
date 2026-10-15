#!/usr/bin/env python3
"""One run's column for the three comparison tables.

    scripts/harbor/comparison-column.py <run-dir> [--archive <dir>]

Corpus totals, Subset A (tasks every headroom run passed) and Subset B (tasks
every run passed) are the three views in
`runs/COMPARISON-headroom-vs-kompressor.md`. The subset membership is a
property of the 2026-09 campaign, not of this run, and is reconstructed from
`harbor-full-archive/trials.csv` -- the runs that defined it were deleted.

The archive is run data and lives wherever the campaign's `runs/` root is, so
it is searched for rather than assumed: see `find_archive`. `--archive`, or
`HARBOR_ARCHIVE`, names it outright.
"""
import argparse
import collections
import csv
import importlib.util
import json
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
P_FRESH, P_CACHED, P_OUT = 5.0e-6, 0.5e-6, 30.0e-6
TRIAL = re.compile(r"^(?P<t>[a-z0-9][a-z0-9.-]*)__[A-Za-z0-9]{5,12}$")
ARCHIVE_NAME = "harbor-full-archive"

_spec = importlib.util.spec_from_file_location("attr", os.path.join(HERE, "attribute.py"))
attr = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(attr)


def find_archive(run_dir):
    """Locate the archived per-trial table, without naming anyone's home dir.

    The archive is run data, not source, so it does not live in the checkout --
    but the previous default was one person's absolute path, which made this
    script work in exactly one clone. Searched instead, relative to things the
    caller already named: `runs/` inside this checkout, and the run directory's
    own ancestors, since a run dir and the archive are siblings under whatever
    `runs/` root the campaign wrote into. HARBOR_ARCHIVE skips the search.
    """
    env = os.environ.get("HARBOR_ARCHIVE")
    if env:
        return os.path.abspath(os.path.expanduser(env))
    root = os.path.dirname(os.path.dirname(HERE))     # the repo, from scripts/harbor/
    candidates = [os.path.join(root, "runs", ARCHIVE_NAME)]
    here = os.path.abspath(run_dir)
    while True:
        parent = os.path.dirname(here)
        if parent == here:
            break
        candidates.append(os.path.join(parent, ARCHIVE_NAME))
        here = parent
    for candidate in candidates:
        if os.path.isfile(os.path.join(candidate, "trials.csv")):
            return candidate
    raise SystemExit(
        f"no {ARCHIVE_NAME}/trials.csv found near {os.path.abspath(run_dir)} "
        f"or in {os.path.join(root, 'runs')}.\n"
        "pass --archive /path/to/harbor-full-archive, or set HARBOR_ARCHIVE.")


def subsets(archive):
    """Subset A and B, rebuilt from the archived per-trial table."""
    rows = list(csv.DictReader(open(os.path.join(archive, "trials.csv"))))
    def ok(r): return bool(r["reward"]) and float(r["reward"]) >= 1.0
    hr, kp = collections.defaultdict(list), collections.defaultdict(list)
    for r in rows:
        (hr if r["arm"].startswith("headroom") else kp)[r["task"]].append(ok(r))
    A = sorted(t for t, v in hr.items() if v and all(v))
    B = sorted(t for t in A if kp[t] and all(kp[t]))
    return A, B


def rewards(run_dir):
    out, errored = {}, set()
    for job in os.listdir(run_dir):
        jd = os.path.join(run_dir, job)
        if not os.path.isdir(jd):
            continue
        for t in os.listdir(jd):
            m = TRIAL.match(t)
            if not m:
                continue
            td = os.path.join(jd, t)
            try:
                out[m.group("t")] = float(
                    open(os.path.join(td, "verifier", "reward.txt")).read().strip())
            except (OSError, ValueError):
                out[m.group("t")] = None
            if os.path.exists(os.path.join(td, "exception.txt")):
                errored.add(m.group("t"))
    return out, errored


def column(per, tasks):
    agg = collections.Counter()
    for t in tasks:
        agg.update(per.get(t, collections.Counter()))
    billed, cached = agg["prompt_tokens"], agg["cache_read_tokens"]
    unc, out = billed - cached, agg["completion_tokens"]
    ti, to = agg["tokens_in"], agg["tokens_out"]
    return {
        "turns": agg["turns"], "ctx_in": ti, "removed": ti - to,
        "pct": 100 * (1 - to / ti) if ti else 0.0,
        "billed": billed, "cached": cached, "uncached": unc, "output": out,
        "cost": unc * P_FRESH + cached * P_CACHED + out * P_OUT,
        "hit": 100 * cached / billed if billed else 0.0,
    }


def main():
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    p.add_argument("--archive", default=None,
                   help="the harbor-full-archive directory holding trials.csv; "
                        "found relative to the run dir or this checkout when omitted")
    a = p.parse_args()

    pr = attr.proxy_rows(a.run_dir)
    matched, amb, un = attr.attribute(pr, attr.trial_usage(a.run_dir))
    per = collections.defaultdict(collections.Counter)
    for task, r in matched:
        q = per[task]; q["turns"] += 1
        for k in ("tokens_in", "tokens_out", "prompt_tokens",
                  "cache_read_tokens", "completion_tokens"):
            q[k] += r.get(k) or 0

    rw, errored = rewards(a.run_dir)
    passed = [t for t, v in rw.items() if v is not None and v >= 1.0]
    graded_fail = [t for t, v in rw.items() if v is not None and v < 1.0]
    A, B = subsets(a.archive or find_archive(a.run_dir))

    print(f"{os.path.basename(a.run_dir)}")
    print(f"  attributed {len(matched)}/{len(pr)} requests "
          f"({amb} ambiguous, {un} unmatched)")
    print(f"  tasks: {len(passed)} passed, {len(graded_fail)} failed, "
          f"{len(errored)} errored, {len(rw)} graded")
    print(f"  accuracy {100 * len(passed) / len(rw):.2f}%")
    for label, tasks in (("Corpus", sorted(per)), (f"Subset A ({len(A)})", A),
                         (f"Subset B ({len(B)})", B)):
        c = column(per, tasks)
        denom = len(passed) if label == "Corpus" else len(tasks)
        print(f"\n  --- {label} ---")
        print(f"  turns {c['turns']:,} | ctx {c['ctx_in']:,} -> removed {c['removed']:,}"
              f" = {c['pct']:.2f}%")
        print(f"  billed {c['billed']:,} (cache {c['cached']:,}, "
              f"uncached {c['uncached']:,}) | out {c['output']:,}")
        print(f"  cost ${c['cost']:.2f} | hit {c['hit']:.1f}% | "
              f"per {'passed ' if label == 'Corpus' else ''}task ${c['cost'] / denom:.3f}")


if __name__ == "__main__":
    main()
