#!/usr/bin/env bash
# A statically linked binary, for the container image and for copying onto a
# machine whose libc you do not control.
#
# The tree-sitter grammars are C, so this needs a musl C compiler as well as the
# Rust target. Both are checked up front, because the failure otherwise arrives
# as a linker error with no hint about what to install.
set -euo pipefail

TARGET="${1:-x86_64-unknown-linux-musl}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

missing=()
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
  || missing+=("the Rust target:   rustup target add $TARGET")

case "$TARGET" in
  *musl*)
    command -v musl-gcc >/dev/null || command -v "${TARGET}-gcc" >/dev/null \
      || missing+=("a musl C compiler: apt install musl-tools   (the tree-sitter grammars are C)")
    ;;
esac

if ((${#missing[@]})); then
  echo "cannot build $TARGET; missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo >&2
  echo "Or build without the grammars, which needs no C toolchain:" >&2
  echo "  cargo build --release -p kompressor-cli --no-default-features" >&2
  exit 1
fi

export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
cargo build --release --target "$TARGET" -p kompressor-cli

out="target/$TARGET/release/kompressor"
echo
echo "  $out  ($(stat -c%s "$out" 2>/dev/null || stat -f%z "$out") bytes)"
file "$out" 2>/dev/null || true
