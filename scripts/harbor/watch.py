#!/usr/bin/env python3
"""Live view of a kompressor-rs run while it is still going.

    scripts/harbor/watch.py                  # newest run under $KOMPRESSOR_RUNS
    scripts/harbor/watch.py <run-dir>        # a specific run
    scripts/harbor/watch.py <run-dir> --once # print once and exit
    scripts/harbor/watch.py <run-dir> -n 10  # refresh every 10s (default 5)

Everything here is read-only and safe against a live run: the statistics
database is opened read-only through a URI so the writer's WAL is never
touched, and the HTTP surfaces are plain GETs.
"""
import argparse
import datetime as dt
import glob
import json
import os
import re
import sqlite3
import sys
import time
import urllib.error
import urllib.request

# Where harbor/suite.sh puts its run directories. Overridable, because the
# runs are large and people keep them off the repo's disk.
RUNS = os.environ.get("KOMPRESSOR_RUNS") or os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.realpath(__file__)))), "runs")


def newest_run():
    cands = glob.glob(os.path.join(RUNS, "*", "20*"))
    if not cands:
        sys.exit(f"no run directories under {RUNS} -- pass one explicitly, or set "
                 "KOMPRESSOR_RUNS")
    return max(cands, key=os.path.getmtime)


def listen_addr(run_dir):
    """The address the proxy was told to bind, from the run's own config."""
    path = os.path.join(run_dir, "kompressor.toml")
    try:
        text = open(path, encoding="utf-8").read()
    except OSError:
        return None
    match = re.search(r'^\s*listen\s*=\s*"([^"]+)"', text, re.M)
    return match.group(1) if match else None


def stats_path_of(config_toml):
    """The `path` under [stats] in a TOML document, or None."""
    if not isinstance(config_toml, str):
        return None
    section = re.search(r"^\[stats\]\s*$(.*?)(?=^\[|\Z)", config_toml,
                        re.M | re.S)
    if not section:
        return None
    path = re.search(r'^\s*path\s*=\s*"([^"]+)"', section.group(1), re.M)
    return path.group(1) if path else None


def get(addr, path, timeout=5):
    try:
        with urllib.request.urlopen(f"http://{addr}{path}", timeout=timeout) as resp:
            body = resp.read().decode()
        # Guessing the format from the first character is not safe here:
        # /v1/config answers in TOML, which starts "[server]" and so looks
        # exactly like a JSON array. Try to parse, and keep the text if it is
        # not JSON.
        try:
            return json.loads(body)
        except ValueError:
            return body
    except (urllib.error.URLError, OSError, ValueError, TimeoutError):
        return None


def rows(db, sql, args=()):
    """Read the live database without disturbing the writer."""
    if not os.path.exists(db):
        return []
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
        con.row_factory = sqlite3.Row
        try:
            return [dict(r) for r in con.execute(sql, args)]
        finally:
            con.close()
    except sqlite3.Error:
        return []


def pct(x):
    return "   n/a" if x is None else f"{x * 100:5.1f}%"


def render(run_dir, addr, db):
    out = []
    add = out.append
    now = dt.datetime.now().strftime("%H:%M:%S")
    add(f"\033[1mkompressor-rs\033[0m  {os.path.basename(os.path.dirname(run_dir))}"
        f"/{os.path.basename(run_dir)}   {now}")

    # ---- is it up -----------------------------------------------------------
    health = get(addr, "/healthz") if addr else None
    mine = False
    if health is None:
        add(f"  proxy      \033[33mnot answering on {addr}\033[0m "
            "(finished, or bound elsewhere)")
    else:
        # A port is not an identity. Runs reuse 8817, so a finished run's
        # address is very often a LATER run's live proxy, and reporting that
        # one's live fragment count beside this run's totals invents a number.
        # Ask the proxy which database it is writing and believe it over the
        # port.
        # /v1/config answers in TOML as text/plain, not JSON
        serving = stats_path_of(get(addr, "/v1/config"))
        mine = bool(serving) and os.path.realpath(serving) == os.path.realpath(db)
        if not mine:
            add(f"  proxy      \033[33m{addr} belongs to a DIFFERENT run\033[0m")
            if serving:
                add(f"             \033[2mit is writing {serving}\033[0m")
            add("             \033[2mtotals below come from this run's own "
                "database, on disk\033[0m")
        else:
            checks = (get(addr, "/readyz") or {}).get("checks", {})
            bad = [k for k, v in checks.items() if not v]
            add(f"  proxy      up on {addr}   "
                + (f"\033[31mnot ready: {bad}\033[0m" if bad else "ready"))

    if mine:
        store = get(addr, "/v1/stats/store") or {}
        metrics = get(addr, "/metrics") or ""
        dropped = 0
        if isinstance(metrics, str):
            found = re.search(r"^kompressor_stats_dropped_total (\d+)", metrics, re.M)
            dropped = int(found.group(1)) if found else 0
        add(f"  refs live  {store.get('live_refs', 0):,} fragments, "
            f"{(store.get('live_bytes') or 0) / 1e6:.1f} MB retained")
        if dropped:
            add(f"  \033[31m!! {dropped:,} statistics events dropped -- every number "
                "below is an undercount\033[0m")

    # ---- what the proxy has done -------------------------------------------
    pr = rows(db, "SELECT time_ms,request_id,status,prompt_tokens,cache_read_tokens,"
                  "completion_tokens,retrieve_rounds,retrieve_tokens_injected,"
                  "total_ms,error FROM proxy_requests ORDER BY time_ms")
    oe = rows(db, "SELECT time_ms,request_id,tokens_in,tokens_out,refs_emitted,"
                  "messages_compressed,messages_total,content_types "
                  "FROM optimization_events WHERE surface='proxy' ORDER BY time_ms")
    by_id = {r["request_id"]: r for r in oe}
    done = [r for r in pr if not r.get("error")]
    # abandoned appears two ways: with a disconnect note (current build), or as a
    # compression event with no request row at all (pre-2026-09-18 build)
    gone = [r for r in pr if r.get("error") and "disconnect" in str(r["error"]).lower()]
    failed = [r for r in pr if r.get("error") and r not in gone]
    orphan = [r for r in oe if r["request_id"] not in {p["request_id"] for p in pr}]

    matched = [by_id[r["request_id"]] for r in pr if r["request_id"] in by_id]
    ti = sum(r["tokens_in"] or 0 for r in matched)
    to = sum(r["tokens_out"] or 0 for r in matched)
    prompt = sum(r["prompt_tokens"] or 0 for r in pr)
    cache = sum(r["cache_read_tokens"] or 0 for r in pr)

    # The windowed sum over-counts on agent traffic: every turn re-compresses
    # the same growing history and the same bytes are credited again. The
    # marginal view -- how much of each turn's NEW context was removed -- is the
    # one that survives being quoted. When the two disagree sharply, trust the
    # marginal one.
    growth_in = growth_out = 0
    ordered = [by_id[r["request_id"]] for r in pr if r["request_id"] in by_id]
    for prev, cur in zip(ordered, ordered[1:]):
        if (cur["tokens_in"] or 0) >= (prev["tokens_in"] or 0):
            growth_in += (cur["tokens_in"] or 0) - (prev["tokens_in"] or 0)
            growth_out += (cur["tokens_out"] or 0) - (prev["tokens_out"] or 0)

    elapsed = ((pr[-1]["time_ms"] - pr[0]["time_ms"]) / 60000.0) if len(pr) > 1 else 0.0
    busy = sum((r.get("total_ms") or 0) for r in pr) / 60000.0

    add("")
    add(f"  requests   {len(done):>4} done"
        + (f"   \033[31m{len(gone) + len(orphan)} abandoned\033[0m"
           if gone or orphan else "   0 abandoned")
        + (f"   \033[31m{len(failed)} failed\033[0m" if failed else ""))
    if ti:
        add(f"  context    {ti:>10,} -> {to:<10,} tokens   {pct(1 - to / ti)} removed"
            "  \033[2m(windowed: re-credits each turn)\033[0m")
    if growth_in:
        add(f"  per turn   {growth_in:>10,} -> {growth_out:<10,} new context "
            f"  {pct(1 - growth_out / growth_in)} removed  \033[2m(the honest one)\033[0m")
    if prompt:
        add(f"  billed     {prompt:>10,} prompt tokens, {cache:,} from cache "
            f"({pct(cache / prompt).strip()})")
    minted = sum(r["refs_emitted"] or 0 for r in matched)
    add(f"  handles    {minted:,} emitted")

    # `refs_emitted` counts handle EMISSIONS, not fragments: an agent re-sends
    # its whole history every turn, so the same content-addressed handle is
    # re-emitted on each one. Dividing fetches by it understates reach by the
    # turn count -- 4 of 2,146 reads as 0.19% where the honest figure, against
    # the fragments actually retained, is 4.2%. The ref store knows how many
    # there really are.
    retained = rows(os.path.join(run_dir, "kompressor-refs.db"),
                    "SELECT COUNT(*) AS n FROM refs")
    distinct_refs = retained[0]["n"] if retained else 0
    ret = rows(db, "SELECT ref,found,ranged,returned_tokens FROM retrieval_events")
    if ret:
        distinct = len({r["ref"] for r in ret})
        share = (f"{100 * distinct / distinct_refs:.1f}%" if distinct_refs else "n/a")
        add(f"  retrieved  {len(ret)} fetches ({sum(1 for r in ret if r['found'])} hits, "
            f"{sum(1 for r in ret if r['ranged'])} narrowed), "
            f"{sum(r['returned_tokens'] or 0 for r in ret):,} tokens put back")
        add(f"  \033[2mreach\033[0m      {distinct} of {distinct_refs:,} retained fragments "
            f"ever asked for ({share})")
    elif minted:
        add(f"  \033[2mreach\033[0m      none of {distinct_refs:,} retained fragments has been "
            "asked for -- recoverable is paying to retain and not yet earning it")

    # what the engine is finding, which is the fastest way to see a workload
    # the detector is not reaching
    types = {}
    for r in matched:
        try:
            for k, v in json.loads(r["content_types"] or "{}").items():
                types[k] = types.get(k, 0) + v
        except (TypeError, ValueError):
            pass
    if types:
        top = sorted(types.items(), key=lambda kv: -kv[1])[:7]
        add("  by type    " + "  ".join(f"{k} {v}" for k, v in top))

    if elapsed:
        add(f"  pace       {len(pr) / elapsed:.1f} requests/min over {elapsed:.1f} min; "
            f"{busy:.1f} min of that inside a request")

    # ---- the tail ----------------------------------------------------------
    add("")
    add("  \033[2mlast requests\033[0m")
    add(f"  {'time':<9}{'ctx in':>9}{'-> out':>9}{'refs':>6}{'rnds':>6}"
        f"{'billed':>9}{'cached':>9}{'secs':>7}  note")
    for r in pr[-8:]:
        c = by_id.get(r["request_id"], {})
        when = dt.datetime.fromtimestamp(r["time_ms"] / 1000).strftime("%H:%M:%S")
        note = ""
        if r.get("error"):
            note = f"\033[31m{str(r['error'])[:40]}\033[0m"
        add(f"  {when:<9}{c.get('tokens_in') or 0:>9,}{c.get('tokens_out') or 0:>9,}"
            f"{c.get('refs_emitted') or 0:>6}{r.get('retrieve_rounds') or 0:>6}"
            f"{r.get('prompt_tokens') or 0:>9,}{r.get('cache_read_tokens') or 0:>9,}"
            f"{(r.get('total_ms') or 0) / 1000:>7.1f}  {note}")
    for r in orphan[-3:]:
        when = dt.datetime.fromtimestamp(r["time_ms"] / 1000).strftime("%H:%M:%S")
        add(f"  {when:<9}{r['tokens_in']:>9,}{r['tokens_out']:>9,}"
            f"{r['refs_emitted']:>6}{'':>6}{'':>9}{'':>9}{'':>7}  "
            "\033[31mcompressed, never completed\033[0m")

    # ---- the log, filtered ---------------------------------------------------
    # Only what would change what you do. A run emits one INFO line per request
    # and those are already in the table above; what is not there is anything
    # the proxy warned about.
    log = os.path.join(run_dir, "kompressor-proxy.log")
    if os.path.exists(log):
        size = os.path.getsize(log)
        notable = []
        try:
            with open(log, encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    if '"level":"WARN"' in line or '"level":"ERROR"' in line:
                        try:
                            e = json.loads(line)
                            when = e.get("timestamp", "")[11:19]
                            msg = (e.get("fields") or {}).get("message", "")
                            extra = {k: v for k, v in (e.get("fields") or {}).items()
                                     if k != "message"}
                            notable.append(f"  {when}  {msg} {extra if extra else ''}"[:150])
                        except ValueError:
                            notable.append("  " + line.strip()[:150])
        except OSError:
            pass
        add("")
        if notable:
            add(f"  \033[2mproxy log\033[0m  {len(notable)} warning(s)")
            for line in notable[-5:]:
                add(f"\033[33m{line}\033[0m")
        elif size == 0:
            add("  \033[33mproxy log  0 bytes -- the proxy normally writes one line per "
                "request, so an empty log is itself worth chasing\033[0m")
        else:
            add(f"  \033[2mproxy log  {size / 1024:.0f} KB, no warnings\033[0m")

    # ---- harbor ------------------------------------------------------------
    res = sorted(glob.glob(os.path.join(run_dir, "**", "result.json"), recursive=True),
                 key=os.path.getmtime, reverse=True)
    for path in res:
        try:
            data = json.load(open(path, encoding="utf-8"))
        except (OSError, ValueError):
            continue
        st = data.get("stats") or {}
        if "evals" not in st:
            continue
        add("")
        # Harbor writes result.json while the job is still running, with nulls
        # where it does not know yet.
        cost = st.get("cost_usd")
        add(f"  harbor     {data.get('n_total_trials') or '?'} trials, "
            + (f"${cost:.2f}, " if isinstance(cost, (int, float)) else "cost n/a, ")
            + f"{st.get('n_errored_trials') or 0} errored")
        break
    return "\n".join(out)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("run_dir", nargs="?")
    p.add_argument("-n", "--interval", type=float, default=5.0)
    p.add_argument("--once", action="store_true")
    a = p.parse_args()

    run_dir = a.run_dir or newest_run()
    if not os.path.isdir(run_dir):
        sys.exit(f"not a directory: {run_dir}")
    db = os.path.join(run_dir, "kompressor-stats.db")
    addr = listen_addr(run_dir)

    while True:
        text = render(run_dir, addr, db)
        if a.once:
            print(text)
            return
        print("\033[2J\033[H" + text, flush=True)
        time.sleep(a.interval)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
