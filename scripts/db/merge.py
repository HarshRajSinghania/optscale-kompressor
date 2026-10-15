#!/usr/bin/env python3
"""Merge per-run statistics databases into one store you can report on.

    scripts/db/merge.py --out ~/kompressor-all.db <runs-root>
    kompressor stats report --stats ~/kompressor-all.db > report.html

Each Harbor run writes its own `kompressor-stats.db` beside its dump, which is
what makes a dump self-contained and reproducible — and also why
`kompressor stats report` against the *global* store shows only whatever ran
under the global config. The data is not missing, it is in 68 places.

# Why this is column-aware rather than a plain `INSERT ... SELECT *`

The dumps span every schema version the project has had. A run recorded before
`skipped_tokens_by_reason` existed has fewer columns than one recorded after,
and positional inserts would either fail or, worse, shift values into the wrong
columns. So each table is merged over the *intersection* of the source's and
the target's columns, and anything the source never recorded lands as NULL —
which is what "this run predates that measurement" should look like.

`id` is deliberately not carried across: every database numbers from 1, so
keeping them would collide. Rows stay distinguishable by `request_id` and
`time_ms`, and runs stay separable by their time window.
"""

import argparse
import os
import shutil
import sqlite3
import sys

# Every table worth carrying. `schema_migrations` and the views are recreated by
# the skeleton, and `daily_rollups` is a derived cache with its own key, so it
# is merged permissively rather than being allowed to abort the batch.
TABLES = [
    ("optimization_events", "INSERT"),
    ("retrieval_events", "INSERT"),
    ("protected_ref_events", "INSERT"),
    ("proxy_requests", "INSERT"),
    ("store_samples", "INSERT OR IGNORE"),
    ("daily_rollups", "INSERT OR IGNORE"),
]


def columns(conn, table, schema="main"):
    try:
        rows = conn.execute(f"PRAGMA {schema}.table_info({table})").fetchall()
    except sqlite3.Error:
        return []
    return [r[1] for r in rows]


def user_version(path):
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        v = con.execute("PRAGMA user_version").fetchone()[0]
        con.close()
        return v
    except sqlite3.Error:
        return -1


def find_dbs(roots):
    found = []
    for root in roots:
        if os.path.isfile(root):
            found.append(root)
            continue
        for dirpath, _, names in os.walk(root):
            for name in names:
                if name == "kompressor-stats.db":
                    found.append(os.path.join(dirpath, name))
    return sorted(set(found))


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("roots", nargs="+", help="directories to walk, or database paths")
    ap.add_argument("--out", required=True, help="the merged database to write")
    ap.add_argument("--force", action="store_true", help="overwrite --out if it exists")
    args = ap.parse_args()

    dbs = find_dbs(args.roots)
    if not dbs:
        sys.exit("no kompressor-stats.db found under: " + ", ".join(args.roots))

    # The skeleton is the newest schema present, emptied: it brings the tables,
    # indexes and views with it, so the merged store is reportable without
    # needing the binary to migrate anything.
    newest = max(dbs, key=user_version)
    version = user_version(newest)
    if version < 1:
        sys.exit(f"no usable schema in {newest}")
    if os.path.exists(args.out) and not args.force:
        sys.exit(f"{args.out} exists; pass --force to overwrite")
    shutil.copyfile(newest, args.out)
    out = sqlite3.connect(args.out)
    for table, _ in TABLES:
        try:
            out.execute(f"DELETE FROM {table}")
        except sqlite3.Error:
            pass
    out.commit()
    print(f"skeleton from {newest} (schema v{version}), emptied")

    totals = {t: 0 for t, _ in TABLES}
    skipped = []
    for db in dbs:
        v = user_version(db)
        if v < 1:
            skipped.append((db, "unreadable"))
            continue
        out.execute("ATTACH DATABASE ? AS src", (f"file:{db}?mode=ro",))
        try:
            for table, verb in TABLES:
                target = columns(out, table, "main")
                source = columns(out, table, "src")
                if not target or not source:
                    continue
                shared = [c for c in target if c in source and c != "id"]
                if not shared:
                    continue
                cols = ", ".join(f'"{c}"' for c in shared)
                before = out.execute(f"SELECT COUNT(*) FROM main.{table}").fetchone()[0]
                out.execute(
                    f"{verb} INTO main.{table} ({cols}) SELECT {cols} FROM src.{table}"
                )
                after = out.execute(f"SELECT COUNT(*) FROM main.{table}").fetchone()[0]
                totals[table] += after - before
            out.commit()
        finally:
            out.execute("DETACH DATABASE src")

    print(f"merged {len(dbs) - len(skipped)} of {len(dbs)} databases")
    for db, why in skipped:
        print(f"  skipped ({why}): {db}")
    for table, _ in TABLES:
        print(f"  {totals[table]:>8,}  {table}")
    span = out.execute(
        "SELECT MIN(time_ms), MAX(time_ms), COUNT(DISTINCT api_family) FROM proxy_requests"
    ).fetchone()
    if span[0]:
        import datetime as dt

        fmt = lambda ms: dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m-%d %H:%M")
        print(f"  window {fmt(span[0])} .. {fmt(span[1])}, {span[2]} api families")
    out.close()
    print(f"\nreport it with:\n  kompressor stats report --stats {args.out} > report.html")


if __name__ == "__main__":
    main()
