#!/usr/bin/env python3
"""Record golden compression vectors for `crates/kompressor-engine/tests/golden.rs`.

Compression behaviour is a pile of interacting thresholds, so the regression that
matters is a change in *which payloads compress and to what*. A recorded snapshot
catches that where unit tests do not.

This runs a generating engine over two corpora — the committed replay fixture of
real agent tool output, and a set of hand-written micro payloads that reach each
detector branch — and records, per payload and per mode, exactly what it
produced.

What is recorded is the full contract the engine has to reproduce:

* the detected content type and the metadata that steered it;
* the compressed bytes;
* the reported kind and the unfolded applied types;
* the ref handles emitted, in order.

Handles are content-addressed, so equal handles prove equal fragments without
storing a byte of them.

Usage::

    python3 tools/export_golden.py --engine-root /path/to/generator \\
        --corpus fixtures/corpus.jsonl.gz --out fixtures/golden.json.gz

``--engine-root`` is a directory placed on ``sys.path`` that provides the Python
``kompressor`` package used to generate the vectors. Regenerating is a deliberate
act: it moves the snapshot the test compares against, so it should be done only
when the recorded behaviour is meant to change.
"""

from __future__ import annotations

import argparse
import gzip
import json
import sys
from pathlib import Path

MODES = ("aggressive", "lossless", "recoverable")


def load_engine(engine_root: Path):
    sys.path.insert(0, str(engine_root))
    from kompressor.kompressor_server.engine import (  # noqa: E402
        EngineOptions,
        RefCollector,
        compress_content,
    )
    from kompressor.kompressor_server.engine.router import detect_result  # noqa: E402
    from kompressor.kompressor_server.engine.types import CompressionContext  # noqa: E402

    return EngineOptions, RefCollector, compress_content, detect_result, CompressionContext


def micro_fixtures() -> list[dict]:
    """Payloads that reach detector branches the real corpus may not hold.

    Each is deliberately just over the 512-character gate, because a payload
    under it is skipped and proves nothing about routing.
    """

    def pad(text: str, target: int = 700) -> str:
        return text if len(text) >= target else text

    cases: list[tuple[str, str]] = []

    cases.append((
        "json_array_of_objects",
        json.dumps([{"id": i, "status": "ok", "ms": 10 + i % 7} for i in range(60)]),
    ))
    cases.append((
        "json_array_with_errors",
        json.dumps(
            [{"id": i, "status": 200 if i % 11 else 500, "msg": "timeout" if i == 17 else "fine"}
             for i in range(60)]
        ),
    ))
    cases.append((
        "json_map_of_records",
        json.dumps({f"user_{i}": {"name": f"n{i}", "age": 20 + i % 40, "city": "x"} for i in range(40)}),
    ))
    cases.append((
        "json_ndjson",
        "\n".join(json.dumps({"seq": i, "event": "tick", "value": i * 3}) for i in range(60)),
    ))
    cases.append((
        "json_nested_long_string",
        json.dumps({"stdout": "\n".join(f"2026-01-02 03:04:{i:02d} INFO worker {i} ready" for i in range(60))}),
    ))
    diff_files = []
    for n in range(12):
        diff_files.append("\n".join(
            [f"diff --git a/f{n}.py b/f{n}.py", "index 1111111..2222222 100644",
             f"--- a/f{n}.py", f"+++ b/f{n}.py", "@@ -1,8 +1,8 @@"]
            + [f" context line {i} in file {n}" for i in range(6)]
            + [f"-old value {n}", f"+new value {n}"]
        ))
    cases.append(("git_diff", "\n".join(diff_files)))
    cases.append((
        "html_document",
        "<!doctype html>\n<html><head><title>t</title></head><body>\n"
        + "\n".join(f"<div class='row'><span>cell {i}</span></div>" for i in range(40))
        + "\n</body></html>",
    ))
    cases.append((
        "search_grep_hits",
        "\n".join(f"src/module_{i % 9}/file.py:{100 + i}:    result = compute(value_{i})" for i in range(60)),
    ))
    cases.append((
        "search_with_signal",
        "\n".join(
            f"src/mod_{i % 6}/a.py:{10 + i}:    raise RuntimeError('failed to connect')"
            if i % 7 == 0
            else f"src/mod_{i % 6}/a.py:{10 + i}:    value = {i}"
            for i in range(60)
        ),
    ))
    cases.append((
        "log_timestamped",
        "\n".join(
            f"2026-01-02T03:04:{i % 60:02d}Z INFO worker heartbeat seq={i} latency_ms={i % 13}"
            for i in range(120)
        ),
    ))
    cases.append((
        "log_with_errors",
        "\n".join(
            (f"2026-01-02T03:04:{i % 60:02d}Z ERROR db connection refused attempt={i}"
             if i % 23 == 0 else
             f"2026-01-02T03:04:{i % 60:02d}Z INFO request served id={i} status=200")
            for i in range(140)
        ),
    ))
    cases.append((
        "log_apt_untimestamped",
        "\n".join(
            [f"Get:{i} http://archive.ubuntu.com/ubuntu noble/main amd64 pkg{i} amd64 1.{i}.0 [{i}00 kB]"
             for i in range(20)]
            + [f"Unpacking pkg{i} (1.{i}.0) ..." for i in range(20)]
            + [f"Setting up pkg{i} (1.{i}.0) ..." for i in range(20)]
            + ["Processing triggers for man-db (2.12.0-1) ..."]
        ),
    ))
    gcc = []
    for i in range(14):
        gcc += [f"src/main.c:{10 + i}:5: warning: unused variable 'x{i}' [-Wunused-variable]",
                f"   {10 + i} |     int x{i} = 0;",
                "      |         ^~"]
    cases.append(("build_diagnostics_gcc", "\n".join(gcc)))
    cases.append((
        "tabular_csv",
        "id,name,region,amount\n"
        + "\n".join(f"{i},name{i},region{i % 5},{i * 13}" for i in range(80)),
    ))
    cases.append((
        "tabular_markdown",
        "| id | name | amount |\n|----|------|--------|\n"
        + "\n".join(f"| {i} | name{i} | {i * 7} |" for i in range(60)),
    ))
    cases.append((
        "listing_paths",
        "\n".join(f"/app/logs/2026-01-{(i % 28) + 1:02d}_service_{i}.log" for i in range(120)),
    ))
    cases.append((
        "listing_ls_long",
        "\n".join(
            f"-rw-r--r-- 1 root root {1000 + i * 7} Jan {(i % 28) + 1:02d} 10:00 file_{i}.txt"
            for i in range(120)
        ),
    ))
    cases.append((
        "listing_offset_rows",
        "\n".join(f"{i * 16:08x}: 4865 6c6c 6f20 776f 726c 6420 6162 6364  Hello world abcd" for i in range(80)),
    ))
    cases.append((
        "listing_uniform_stream",
        "\n".join(f"  checking cst:compcert.backend.Linearize.Nodeset.elt_{i}" for i in range(120)),
    ))
    py_src = []
    for n in range(4):
        py_src += [f"def function_{n}(value):"] \
            + [f"    step_{i} = value + {i}" for i in range(30)] + ["    return step_29", ""]
    cases.append(("source_python", "\n".join(py_src)))
    c_src = []
    for n in range(3):
        c_src += [f"int function_{n}(int a) {{"] \
            + [f"    int t{i} = a + {i};" for i in range(30)] + ["    return 0;", "}"]
    cases.append(("source_c_header",
                  "#ifndef GRAPHX_H\n#define GRAPHX_H\n#include <stdio.h>\n"
                  + "\n".join(c_src) + "\n#endif"))
    cpp_src = []
    for n in range(3):
        cpp_src += [f"template <typename T> T func_{n}(T a) {{"] \
            + [f"    T t{i} = a + {i};" for i in range(30)] + ["    return a;", "}"]
    cases.append(("source_cpp",
                  "#include <vector>\n#include <iostream>\nusing namespace std;\n"
                  + "\n".join(cpp_src)))
    yaml_lines = []
    for i in range(40):
        yaml_lines += [f"# setting {i}", "", f"key_{i}: value_{i}"]
    cases.append(("config_yaml", "\n".join(yaml_lines)))
    toml_lines = ["[server]"]
    for i in range(30):
        toml_lines += [f"# comment {i}", f'key_{i} = "value_{i}"', ""]
    cases.append(("config_toml", "\n".join(toml_lines)))
    ini_lines = ["[main]"]
    for i in range(30):
        ini_lines += [f"; comment {i}", f"key_{i} = value_{i}"]
    cases.append(("config_ini", "\n".join(ini_lines)))
    cases.append((
        "mixed_prose_and_json",
        "Here is the API response I received from the upstream service, which I "
        "would like you to analyse carefully for anomalies:\n\n"
        + json.dumps([{"id": i, "status": "ok", "latency": i} for i in range(50)])
        + "\n\nPlease tell me whether anything looks wrong with these numbers.",
    ))
    cases.append((
        "mixed_fenced_python",
        "The failing module is below.\n\n```python\n"
        + "\n".join(["def handler(request):"] + [f"    value_{i} = request.get('{i}')" for i in range(40)] + ["    return None"])
        + "\n```\n\nWhat is wrong with it?",
    ))
    cases.append((
        "mixed_log_and_prose",
        "I ran the build and it printed this, please summarise the failures:\n\n"
        + "\n".join(f"2026-01-02T03:04:{i % 60:02d}Z INFO compiling module_{i}" for i in range(80))
        + "\n\nThat is the whole log; what went wrong?",
    ))
    cases.append((
        "redraw_progress_meter",
        "Downloading the dataset now\n"
        + "".join(f"\r  {i}% [{'=' * (i // 5)}{' ' * (20 - i // 5)}] {i * 37} kB" for i in range(0, 101, 5))
        + "\ndownload complete\n"
        + "\n".join(f"extracted file_{i}.bin" for i in range(40)),
    ))
    cases.append((
        "harness_preamble_log",
        "Chunk ID: abc123\nWall time: 1.2345 seconds\nProcess exited with code 1\n"
        "Original token count: 900\nOutput:\n"
        + "\n".join(f"2026-01-02T03:04:{i % 60:02d}Z INFO step {i} done" for i in range(100)),
    ))
    cases.append((
        "plain_text_prose",
        " ".join(
            "The quick brown fox jumps over the lazy dog and then considers its options carefully."
            for _ in range(12)
        ),
    ))
    cases.append((
        "text_with_duplicates",
        "\n".join(["identical line of output"] * 60 + [f"unique tail {i}" for i in range(10)]),
    ))
    cases.append((
        "lossless_reserved_shape",
        json.dumps({"payload": {"__cols__": ["a", "b"], "__rows__": [[1, 2], [3, 4]]},
                    "items": [{"x": i, "y": i * 2} for i in range(30)]}),
    ))
    cases.append((
        "tiny_below_gate",
        "short payload, nothing to do here",
    ))

    return [{"content_id": name, "role": "tool", "content": pad(text),
             "preceding_user_text": "", "source_tool": "exec_command"}
            for name, text in cases]


def run(payloads, engine, modes=MODES) -> list[dict]:
    EngineOptions, RefCollector, compress_content, detect_result, CompressionContext = engine
    out = []
    for p in payloads:
        record = {
            "content_id": p["content_id"],
            "role": p["role"],
            "content": p["content"],
            "preceding_user_text": p.get("preceding_user_text", ""),
            "source_tool": p.get("source_tool") or "",
            "modes": {},
        }
        base = EngineOptions()
        det = detect_result(p["content"], p["role"], base)
        record["detected"] = {
            "content_type": det.content_type.value,
            "confidence": round(float(det.confidence), 6),
            "language": det.metadata.get("language"),
            "flavor": det.metadata.get("flavor"),
            "uniform_stream": bool(det.metadata.get("uniform_stream")),
            "untimestamped": bool(det.metadata.get("untimestamped")),
            "diagnostics": bool(det.metadata.get("diagnostics")),
        }
        for mode in modes:
            options = EngineOptions(mode=mode)
            # the production route retains fragments in recoverable mode only
            refs = RefCollector(max_ref_bytes=options.max_ref_bytes) if mode == "recoverable" else None
            applied: list[str] = []
            context = CompressionContext(
                role=p["role"],
                preceding_user_text=p.get("preceding_user_text", ""),
                source_tool=p.get("source_tool") or None,
            )
            try:
                compressed, kind = compress_content(
                    p["content"], p["role"], options, refs=refs, context=context,
                    applied_types=applied,
                )
                err = None
            except Exception as exc:  # a failure is recorded as a decline, not a crash
                compressed, kind, err = p["content"], None, f"{type(exc).__name__}: {exc}"
            record["modes"][mode] = {
                "output": compressed,
                "kind": kind,
                "applied": sorted(set(applied)),
                "refs": [r for r, _ in (refs.pairs if refs else [])],
                "error": err,
            }
        out.append(record)
    return out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--engine-root",
        type=Path,
        required=True,
        help="directory on sys.path providing the generating `kompressor` package",
    )
    ap.add_argument("--corpus", type=Path)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--limit", type=int, default=0)
    args = ap.parse_args(argv)

    engine = load_engine(args.engine_root)

    payloads = micro_fixtures()
    print(f"micro fixtures: {len(payloads)}", file=sys.stderr)

    if args.corpus:
        sys.path.insert(0, str(args.engine_root))
        from kompressor.bench.corpus import load_corpus  # noqa: E402

        corpus = load_corpus(args.corpus)
        if args.limit:
            corpus = corpus[: args.limit]
        payloads += [
            {
                "content_id": p.content_id,
                "role": p.role,
                "content": p.content,
                "preceding_user_text": p.preceding_user_text,
                "source_tool": p.source_tool,
            }
            for p in corpus
        ]
        print(f"corpus payloads: {len(corpus)}", file=sys.stderr)

    records = run(payloads, engine)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(args.out, "wt", encoding="utf-8") as fh:
        json.dump({"format": 1, "records": records}, fh, ensure_ascii=False)
    print(f"wrote {len(records)} records to {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
