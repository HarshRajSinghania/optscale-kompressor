#!/usr/bin/env bash
# Re-record fixtures/golden.json.gz.
#
# The vectors are a snapshot of what the engine produces for every corpus
# payload in every mode, and `cargo test -p kompressor-engine --test golden`
# compares against them. Re-recording moves the thing the test measures, so it
# is a deliberate act: do it when the behaviour is *meant* to change, and read
# the diff before committing it.
#
# It runs a generating engine, not this one — which is the point. Pass the
# directory holding the Python `kompressor` package as --engine-root.
#
# The generator needs its own parser. `engine/code.py` imports
# `tree_sitter_language_pack` inside a try/except ImportError and silently skips
# the code compressor without it, so exporting from an environment that lacks it
# records six `source_code` payloads as untouched — a regression written into the
# file that exists to catch regressions. If the generating python does not have
# it:
#
#   python3 -m venv /tmp/golden-venv
#   /tmp/golden-venv/bin/pip install tree_sitter_language_pack
#   PYTHON=/tmp/golden-venv/bin/python KOMPRESSOR_ENGINE_ROOT=... make golden
#
# Then check the diff: an export that changes an existing record has changed the
# generator, not the corpus, and wants explaining before it is committed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ENGINE_ROOT="${KOMPRESSOR_ENGINE_ROOT:-}"
PYTHON="${PYTHON:-python3}"

if [[ -z "$ENGINE_ROOT" ]]; then
  cat >&2 <<'MSG'
No generating engine given.

  KOMPRESSOR_ENGINE_ROOT=/path/to/generator make golden

The vectors already committed were produced that way; without the generator
there is nothing to record *from*, and the existing file stays as it is.
MSG
  exit 1
fi

echo "This overwrites fixtures/golden.json.gz, which the vector test compares against."
read -r -p "Re-record it? [y/N] " answer
[[ "$answer" == y ]] || { echo aborted; exit 1; }

"$PYTHON" tools/export_golden.py \
  --engine-root "$ENGINE_ROOT" \
  --corpus fixtures/corpus.jsonl.gz \
  --out fixtures/golden.json.gz

echo
echo "recorded. Now run: cargo test -p kompressor-engine --test golden"
echo "and read what changed before committing it."
