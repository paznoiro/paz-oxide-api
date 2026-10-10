#!/bin/sh
# Fetch pdf_oxide from crates.io (sha256-pinned), unpack it into
# vendor/pdf_oxide and apply our compatibility patch. Cargo.toml points a
# [patch.crates-io] entry at the result. Idempotent.
set -eu
VERSION=0.3.78
SHA256=95b1ecc1a60a314fbd1aea5fe8f700ec32af1efa70fef5c6879162444da3437d
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEST="$ROOT/vendor/pdf_oxide"
# Re-vendor whenever the version or any patch changes.
PATCH_SUM=$(cat "$ROOT"/patches/pdf_oxide-$VERSION-*.patch | sha256sum | cut -c1-16)
STAMP="$DEST/.oxide-api-vendored-$VERSION-$PATCH_SUM"

[ -f "$STAMP" ] && exit 0

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL --retry 3 -o "$TMP/pdf_oxide.crate" "https://static.crates.io/crates/pdf_oxide/pdf_oxide-$VERSION.crate"
echo "$SHA256  $TMP/pdf_oxide.crate" | sha256sum -c - >/dev/null
tar -xzf "$TMP/pdf_oxide.crate" -C "$TMP"
for p in "$ROOT"/patches/pdf_oxide-$VERSION-*.patch; do
  patch -s -p1 -d "$TMP/pdf_oxide-$VERSION" < "$p"
done
rm -rf "$DEST"
mkdir -p "$ROOT/vendor"
mv "$TMP/pdf_oxide-$VERSION" "$DEST"
touch "$STAMP"
echo "vendored pdf_oxide $VERSION (patched) into vendor/pdf_oxide"
