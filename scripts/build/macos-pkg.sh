#!/usr/bin/env bash
# A macOS installer package (.pkg), built on Linux or on a Mac.
#
#   make macos-pkg-docker                   # the supported route, needs only Docker
#   scripts/build/macos-pkg.sh [universal2|aarch64|x86_64]
#
# There is deliberately no `make macos-pkg`: building this on the host means
# installing xar, mkbom and a zig newer than any distribution ships, and the
# container already has all three. This script is the container's entrypoint,
# and it also runs natively on a Mac, where `cargo build` needs no zig at all.
#
# A .pkg is a `xar` archive holding four members: PackageInfo (XML), Bom (a
# binary file list), Payload and Scripts (both gzipped cpio). None of that is
# Apple-specific tooling, so this works off a Mac entirely — `xar` and `mkbom`
# are the only things needed, and both are open source.
#
# On a Mac, `pkgbuild` does the same job in one command; this script is written
# so a Linux CI runner can produce the same artifact.
#
# NOT SIGNED. Gatekeeper will refuse a double-click on an unsigned package with
# "cannot be opened because it is from an unidentified developer". Installing it
# needs either a right-click → Open, or `installer -pkg … -target /` from a
# terminal, or a Developer ID certificate this script does not have.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

ARCH="${1:-universal2}"
case "$ARCH" in
  universal2) TARGET=universal2-apple-darwin ;;
  aarch64)    TARGET=aarch64-apple-darwin ;;
  x86_64)     TARGET=x86_64-apple-darwin ;;
  *) echo "usage: $0 [universal2|aarch64|x86_64]" >&2; exit 2 ;;
esac

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
IDENT="com.kompressor.cli"
BIN="target/$TARGET/release/kompressor"
OUT="target/macos/kompressor-${VERSION}-${ARCH}.pkg"

missing=()
command -v xar   >/dev/null || missing+=("xar     — https://github.com/mackyle/xar (configure may need ac_cv_lib_crypto_OpenSSL_add_all_ciphers=yes on OpenSSL 3)")
command -v mkbom >/dev/null || missing+=("mkbom   — https://github.com/hogliux/bomutils")
command -v cpio  >/dev/null || missing+=("cpio    — your distribution's package")
if ((${#missing[@]})); then
  echo "cannot build a .pkg; missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  exit 1
fi

# ALWAYS build. Skipping when the binary happens to exist shipped whatever was
# compiled last: edit a source file or bump the version, and the package came
# out with the new version number wrapped around the old executable, silently.
# Cargo's own freshness check is the thing that decides whether work is needed,
# and it is much better at it than a file-existence test.
echo "building $TARGET ..." >&2
if command -v cargo-zigbuild >/dev/null && command -v zig >/dev/null; then
  cargo zigbuild --release -p kompressor-cli --target "$TARGET" >&2
elif [ "$(uname -s)" = "Darwin" ]; then
  # `universal2-apple-darwin` is a cargo-zigbuild invention, not a rustc target:
  # `rustup target add` does not know it and `cargo build --target` rejects it.
  # Natively the two real targets are built and stitched together with `lipo`,
  # which is what cargo-zigbuild does for us elsewhere.
  if [ "$TARGET" = "universal2-apple-darwin" ]; then
    for real in aarch64-apple-darwin x86_64-apple-darwin; do
      cargo build --release -p kompressor-cli --target "$real" >&2
    done
    mkdir -p "$(dirname "$BIN")"
    lipo -create -output "$BIN" \
      target/aarch64-apple-darwin/release/kompressor \
      target/x86_64-apple-darwin/release/kompressor
  else
    cargo build --release -p kompressor-cli --target "$TARGET" >&2
  fi
else
  echo "no way to build $TARGET here" >&2
  echo "  install cargo-zigbuild and a zig >= 0.9, or build in the container:" >&2
  echo "  make macos-pkg-docker" >&2
  exit 1
fi
[ -f "$BIN" ] || { echo "the build produced no $BIN" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ROOTDIR="$STAGE/root"
SCRIPTDIR="$STAGE/scripts"
FLAT="$STAGE/flat"
mkdir -p "$ROOTDIR/usr/local/bin" "$ROOTDIR/usr/local/share/kompressor" \
         "$ROOTDIR/usr/local/share/doc/kompressor" "$SCRIPTDIR" "$FLAT"

install -m 755 "$BIN"                                   "$ROOTDIR/usr/local/bin/kompressor"
install -m 644 packaging/macos/config.toml.in           "$ROOTDIR/usr/local/share/kompressor/config.toml.in"
install -m 644 packaging/macos/com.kompressor.proxy.plist "$ROOTDIR/usr/local/share/kompressor/com.kompressor.proxy.plist.in"
install -m 755 packaging/macos/install-agent.sh         "$ROOTDIR/usr/local/share/kompressor/install-agent.sh"
install -m 644 README.md                                "$ROOTDIR/usr/local/share/doc/kompressor/README.md"
install -m 644 docs/QUICKSTART.md                       "$ROOTDIR/usr/local/share/doc/kompressor/QUICKSTART.md"
install -m 644 LICENSE                                  "$ROOTDIR/usr/local/share/doc/kompressor/LICENSE"
install -m 755 packaging/macos/scripts/postinstall       "$SCRIPTDIR/postinstall"

# `--format odc` is the old portable cpio format, which is what the macOS
# Installer reads; the default GNU format is not accepted.
( cd "$ROOTDIR"   && find . | cpio -o --format odc --quiet ) | gzip -9 > "$FLAT/Payload"
( cd "$SCRIPTDIR" && find . | cpio -o --format odc --quiet ) | gzip -9 > "$FLAT/Scripts"
mkbom -u 0 -g 80 "$ROOTDIR" "$FLAT/Bom"

NFILES=$(find "$ROOTDIR" | wc -l | tr -d ' ')
# KiB, which is the unit PackageInfo's installKBytes wants.
KBYTES=$(du -k -s "$ROOTDIR" | cut -f1)

cat > "$FLAT/PackageInfo" <<XML
<?xml version="1.0" encoding="utf-8"?>
<pkg-info overwrite-permissions="true" relocatable="false" identifier="$IDENT"
          postinstall-action="none" version="$VERSION" format-version="2"
          generator-version="kompressor macos-pkg.sh" auth="root">
    <payload numberOfFiles="$NFILES" installKBytes="$KBYTES"/>
    <bundle-version/>
    <scripts>
        <postinstall file="./postinstall"/>
    </scripts>
</pkg-info>
XML

mkdir -p "$(dirname "$OUT")"
# Member order matters to the Installer: it reads PackageInfo first, and the
# TOC must not be compressed with anything it does not expect.
( cd "$FLAT" && xar --compression none -cf "$ROOT/$OUT" PackageInfo Bom Payload Scripts )

echo "$OUT"
ls -lh "$OUT" | awk '{print "  " $5}'
echo
echo "  install on a Mac:  sudo installer -pkg $(basename "$OUT") -target /"
echo "  unsigned, so a double-click is refused by Gatekeeper; the command above works,"
echo "  as does right-click -> Open in Finder."
