#!/usr/bin/env python3
"""Paired A/B over the two arms, alternating, resumable, with a spend cap.

Why this exists: a single compressed-vs-control pair cannot answer whether CCR
pays. Two control runs of one `smoke` task eleven minutes apart came to $2.2234
and $1.5151 -- a 46.7% spread, 5.3x the saving the pair was meant to measure.
The noise is how many turns the agent took, and the only way through it is more
runs compared on a metric that divides that term out.

`docs/BENCHMARK.md` has the power table. On cost per run this stage needs ~655
runs per arm; on cost per request it needs ~16, which is what this drives.

Three things it does that a shell loop would not:

**Alternates arms.** Sixteen control runs followed by sixteen compressed ones
would confound the comparison with whatever the provider was doing over three
hours. Alternating costs nothing and removes it.

This is not a precaution, it is the measurement. Across 49 compressed runs on
five days the day means of cost per request span 0.03727 to 0.04517 -- a drift
of 0.0079 under one unchanged treatment, which is the same size as the 0.0078
effect being looked for. So the 49 runs already on disk can estimate the
variance and cannot serve as the comparison arm, and a sweep must start its own
ledger rather than counting them. `runs/ab-history.csv` holds them for the
first purpose only.

**Resumes.** Thirty-two runs is hours. The ledger is the source of truth, so an
interrupted sweep continues where it stopped rather than starting over or
double-counting.

**Stops.** A spend cap and a consecutive-failure limit, because the failure mode
of an unattended loop that costs $2 a turn is not that it crashes.
"""

import argparse
import csv
import itertools
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SUITE = REPO / "scripts" / "harbor" / "suite.sh"
FIELDS = [
    "n", "arm", "stamp", "dump", "ok", "resolved", "attempted", "cost_usd",
    "requests", "billed_prompt", "cache_read_tokens", "completion_tokens",
    "prompt_tokens", "requests_compressed", "cache_lost", "flags", "seconds",
]


ARMS = ("off", "recoverable")


def plan_runs(done, per_arm):
    """What to run next, keeping the arms level.

    Alternating is the measurement, not a precaution: across 49 compressed runs
    on five days the day means of cost per request span 0.03727 to 0.04517, a
    drift of 0.0079 under one unchanged treatment and the same size as the
    0.0078 effect being looked for. An unpaired run measures that drift.

    So the plan is driven by each arm's deficit rather than by position in a
    fixed `[off, recoverable] * n` sequence. Filtering that sequence by index
    looked equivalent and was not: a run can fail, failures are still written to
    the ledger, and `done` counts only successes, so the arms drift apart. With
    ten `off` and eight `recoverable` the filtered sequence began
    `rec, rec, off, rec, ...` — two consecutive unpaired runs, which is exactly
    what alternating exists to prevent. Resume is the normal path here, not the
    exception.

    The arm that is behind goes first, so an interrupted sweep levels up before
    it resumes pairing. Control leads a tie, as it does from empty.

    **The catch-up runs are paired in count, not in time.** Their partners
    already ran, possibly on another day, so they carry the drift this design
    exists to remove. Nothing here can fix that — the only sweep with every run
    time-paired is one that starts from an empty ledger and is not interrupted.
    Levelling is the best available repair, not an equivalent one.
    """
    counts = {arm: done.get(arm, 0) for arm in ARMS}
    left = {arm: max(0, per_arm - counts[arm]) for arm in ARMS}
    plan = []
    while any(left[arm] > 0 for arm in ARMS):
        behind = [arm for arm in ARMS if left[arm] > 0]
        arm = min(behind, key=lambda a: (counts[a], ARMS.index(a)))
        plan.append((arm, counts[arm]))
        counts[arm] += 1
        left[arm] -= 1
    return plan


def runs_root() -> Path:
    return Path(os.environ.get("KOMPRESSOR_RUNS", REPO / "runs")).expanduser()


def out_dir_for(stage: str, arm: str) -> Path:
    suffix = arm if arm != "recoverable" else "recoverable"
    return runs_root() / f"harbor_{stage}_{suffix}"


def newest_dump(parent: Path, after: float) -> Path | None:
    """The run directory this invocation created.

    By mtime and not by name: the stamp is the harness's, and reading it back
    out of stdout couples this to the exact wording of a log line.
    """
    if not parent.is_dir():
        return None
    fresh = [d for d in parent.iterdir()
             if d.is_dir() and d.stat().st_mtime >= after - 1
             and (d / "kompressor_rs_harbor_summary.json").exists()]
    return max(fresh, key=lambda d: d.stat().st_mtime, default=None)


def read_summary(dump: Path) -> dict:
    with open(dump / "kompressor_rs_harbor_summary.json", encoding="utf-8") as fh:
        return json.load(fh)


def cache_lost_tokens(summary: dict) -> int:
    for flag in summary.get("validity_flags") or []:
        if flag.startswith("PROVIDER CACHE LOSS"):
            for part in flag.replace(",", "").split():
                if part.isdigit() and int(part) > 1:
                    return int(part)
    return 0


def one_run(arm: str, stage: str, timeout: int) -> dict:
    env = dict(os.environ, MODE=arm)
    env.setdefault("UPSTREAM", "https://api.openai.com")
    started = time.time()
    proc = subprocess.run([str(SUITE), stage], env=env, cwd=REPO,
                          capture_output=True, text=True, timeout=timeout)
    elapsed = time.time() - started
    dump = newest_dump(out_dir_for(stage, arm), started)
    if dump is None:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()[-4:]
        return {"ok": False, "seconds": round(elapsed), "dump": "",
                "flags": "no dump: " + " / ".join(tail)}

    s = read_summary(dump)
    eff, acc = s.get("efficiency", {}), s.get("accuracy", {})
    stats = acc.get("stats") or {}
    counts = acc.get("counts") or [0, 0]
    return {
        "ok": proc.returncode == 0,
        "stamp": s.get("stamp", ""),
        "dump": str(dump),
        "resolved": counts[0],
        "attempted": counts[1],
        "cost_usd": round(stats.get("cost_usd") or 0.0, 6),
        "requests": eff.get("requests") or 0,
        "billed_prompt": eff.get("billed_prompt") or 0,
        "cache_read_tokens": eff.get("cache_read_tokens") or 0,
        "completion_tokens": eff.get("completion_tokens") or 0,
        "prompt_tokens": eff.get("prompt_tokens") or 0,
        "requests_compressed": eff.get("requests_compressed") or 0,
        "cache_lost": cache_lost_tokens(s),
        "flags": " | ".join(f.split(":")[0] for f in (s.get("validity_flags") or [])),
        "seconds": round(elapsed),
    }


def load_ledger(path: Path) -> list[dict]:
    if not path.exists():
        return []
    with open(path, newline="", encoding="utf-8") as fh:
        return list(csv.DictReader(fh))


def append_ledger(path: Path, row: dict) -> None:
    new = not path.exists()
    with open(path, "a", newline="", encoding="utf-8") as fh:
        writer = csv.DictWriter(fh, fieldnames=FIELDS, extrasaction="ignore")
        if new:
            writer.writeheader()
        writer.writerow(row)
        fh.flush()
        os.fsync(fh.fileno())


def welch(a: list[float], b: list[float]) -> tuple[float, float]:
    """t and the half-width of a rough 95% interval on the difference."""
    if len(a) < 2 or len(b) < 2:
        return float("nan"), float("nan")
    va, vb = statistics.variance(a) / len(a), statistics.variance(b) / len(b)
    se = (va + vb) ** 0.5
    if se == 0:
        return float("nan"), float("nan")
    return (statistics.mean(b) - statistics.mean(a)) / se, 1.96 * se


def report(ledger: list[dict]) -> None:
    good = [r for r in ledger if r["ok"] in ("True", True)]
    arms = {arm: [r for r in good if r["arm"] == arm]
            for arm in ("recoverable", "off")}
    print(f"\n{'':20}{'n':>4}{'cost/run':>11}{'cost/req':>11}"
          f"{'tok/req':>11}{'req':>8}{'cachelost':>11}")
    for arm, rows in arms.items():
        if not rows:
            continue
        cost = [float(r["cost_usd"]) for r in rows]
        req = [int(r["requests"]) for r in rows]
        cpr = [c / q for c, q in zip(cost, req) if q]
        tpr = [int(r["billed_prompt"]) / int(r["requests"])
               for r in rows if int(r["requests"])]
        lost = sum(int(r["cache_lost"]) for r in rows)
        print(f"{arm:20}{len(rows):>4}{statistics.mean(cost):>11.4f}"
              f"{statistics.mean(cpr):>11.5f}{statistics.mean(tpr):>11,.0f}"
              f"{statistics.mean(req):>8.1f}{lost:>11,}")

    c, o = arms["recoverable"], arms["off"]
    if len(c) < 2 or len(o) < 2:
        print("\n  (two runs per arm needed before a comparison means anything)")
        return
    print("\n  compressed vs control, by metric")
    for name, f in (
        ("cost per run", lambda r: float(r["cost_usd"])),
        ("cost per request", lambda r: float(r["cost_usd"]) / int(r["requests"])),
        ("billed prompt per request",
         lambda r: int(r["billed_prompt"]) / int(r["requests"])),
        ("requests per run", lambda r: float(r["requests"])),
    ):
        cv, ov = [f(r) for r in c if int(r["requests"])], [f(r) for r in o if int(r["requests"])]
        t, half = welch(cv, ov)
        delta = (statistics.mean(ov) - statistics.mean(cv))
        pct = delta / statistics.mean(ov) * 100 if statistics.mean(ov) else 0
        verdict = ("separates" if abs(t) > 2 else "does NOT separate")
        print(f"    {name:28} {pct:+6.1f}%   t={t:+5.2f}  "
              f"95% CI on the difference {delta - half:+.5g} .. {delta + half:+.5g}  [{verdict}]")
    print("\n  A per-request win with more requests per run is not a win; "
          "multiply the two to get what was spent.")


def self_test() -> int:
    """Checks for the planner, which has no other way of being wrong out loud.

    `ab.py` is the only thing here that spends money, and the planner is the
    part of it that decides whether the result means anything. Run with
    `--self-test`; no provider, no spend.
    """
    failures = []

    def check(name, got, want):
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    check("an empty ledger alternates, control first",
          plan_runs({"off": 0, "recoverable": 0}, 2),
          [("off", 0), ("recoverable", 0), ("off", 1), ("recoverable", 1)])
    check("a finished sweep plans nothing",
          plan_runs({"off": 4, "recoverable": 4}, 4), [])

    # The resume case. Failures are written to the ledger and `done` counts only
    # successes, so the arms drift apart; filtering a fixed sequence by index
    # then produced consecutive unpaired runs.
    plan = plan_runs({"off": 10, "recoverable": 8}, 12)
    check("an imbalanced resume levels the arms first",
          plan[:2], [("recoverable", 8), ("recoverable", 9)])
    check("and alternates once level",
          plan[2:6],
          [("off", 10), ("recoverable", 10), ("off", 11), ("recoverable", 11)])
    check("every arm reaches the target exactly",
          {arm: sum(1 for a, _ in plan if a == arm) for arm in ARMS},
          {"off": 2, "recoverable": 4})
    # No index is planned twice, or a resumed sweep would overwrite a row.
    check("no index is planned twice", len(set(plan)), len(plan))
    # The run that exposed this: never three of one arm in a row once level.
    tail = [arm for arm, _ in plan[2:]]
    check("no arm runs three times running after levelling",
          max((sum(1 for _ in group) for _, group in itertools.groupby(tail)), default=0), 1)

    for failure in failures:
        print(f"  FAIL  {failure}")
    print(f"  self-test: {'FAILED' if failures else 'ok'}"
          f" ({6 - len(failures)}/6 checks)")
    return 1 if failures else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--per-arm", type=int, default=16)
    ap.add_argument("--stage", default="smoke")
    ap.add_argument("--budget", type=float, default=120.0,
                    help="stop before a run that would pass this total, USD")
    ap.add_argument("--ledger", default=str(runs_root() / "ab-sweep.csv"),
                    help="fresh per sweep; historical runs must not be counted in")
    ap.add_argument("--timeout", type=int, default=3600)
    ap.add_argument("--report-only", action="store_true")
    ap.add_argument("--self-test", action="store_true",
                    help="check the planner; no provider, no spend")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    ledger_path = Path(args.ledger).expanduser()
    ledger = load_ledger(ledger_path)
    if args.report_only:
        report(ledger)
        return 0

    if not os.environ.get("OPENAI_API_KEY"):
        print("set OPENAI_API_KEY first -- this spends money and cannot ask", file=sys.stderr)
        return 2

    done = {arm: sum(1 for r in ledger if r["arm"] == arm and r["ok"] in ("True", True))
            for arm in ARMS}
    spent = sum(float(r["cost_usd"] or 0) for r in ledger)
    print(f"ledger {ledger_path}")
    print(f"  have  recoverable {done['recoverable']}  off {done['off']}  "
          f"spent ${spent:.2f}  target {args.per_arm}/arm")

    plan = plan_runs(done, args.per_arm)

    consecutive_failures = 0
    n = len(ledger)
    for arm, i in plan:
        recent = [float(r["cost_usd"] or 0) for r in ledger[-4:]] or [2.0]
        likely = statistics.mean(recent) or 2.0
        if spent + likely > args.budget:
            print(f"\nstopping: ${spent:.2f} spent, next run ~${likely:.2f}, "
                  f"cap ${args.budget:.2f}")
            break
        n += 1
        print(f"\n[{n}] {arm} #{i + 1}/{args.per_arm}  "
              f"(spent ${spent:.2f})", flush=True)
        try:
            row = one_run(arm, args.stage, args.timeout)
        except subprocess.TimeoutExpired:
            row = {"ok": False, "flags": f"timeout after {args.timeout}s", "seconds": args.timeout}
        row.update(n=n, arm=arm)
        append_ledger(ledger_path, row)
        ledger.append({k: str(row.get(k, "")) for k in FIELDS})
        spent += float(row.get("cost_usd") or 0)

        if row.get("ok"):
            consecutive_failures = 0
            print(f"    ok  ${row.get('cost_usd', 0):.4f}  "
                  f"{row.get('requests', 0)} requests  {row.get('seconds', 0)}s"
                  + (f"  [{row['flags']}]" if row.get("flags") else ""))
        else:
            consecutive_failures += 1
            print(f"    FAILED ({consecutive_failures}): {row.get('flags', '')}")
            if consecutive_failures >= 3:
                print("\nstopping: three consecutive failures, "
                      "something is wrong rather than flaky")
                break

    report(ledger)
    print(f"\n  ledger: {ledger_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
