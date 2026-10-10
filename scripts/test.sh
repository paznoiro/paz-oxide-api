#!/bin/sh
# Runs the full test suite (unit, API, OCR) in a Rust container with ONNX
# Runtime and the models mounted. Downloads both (sha256-pinned) into
# ./target/ocr-assets once. Extra args go to `cargo test`.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
ASSETS="$ROOT/target/ocr-assets"
ORT_VERSION=1.28.3
case "$(uname -m)" in
  arm64|aarch64) ORT_ARCH=aarch64; ORT_SHA=6c6b1ae96d7b0be9f555092857c6a4f2b0b5587a5298d732d9e52e8253038ce7 ;;
  *)             ORT_ARCH=x64;     ORT_SHA=db14e4863bd37893fc59729d986ab2a0d043d10b7d44da1913c4982b7e3d009c ;;
esac
mkdir -p "$ASSETS/ort"
"$ROOT/scripts/fetch_ocr_models.sh" "$ASSETS/models"
if [ ! -f "$ASSETS/ort/lib/libonnxruntime.so" ]; then
  curl -fsSL --retry 3 -o "$ASSETS/ort.tgz" \
    "https://github.com/microsoft/onnxruntime/releases/download/v$ORT_VERSION/onnxruntime-linux-$ORT_ARCH-$ORT_VERSION.tgz"
  echo "$ORT_SHA  $ASSETS/ort.tgz" | sha256sum -c - >/dev/null
  tar -xzf "$ASSETS/ort.tgz" -C "$ASSETS/ort" --strip-components=1
  rm "$ASSETS/ort.tgz"
fi
docker run --rm \
  -v "$ROOT:/work" -v oxide-cargo-registry:/usr/local/cargo/registry \
  -v "$ASSETS:/assets:ro" \
  -e ORT_DYLIB_PATH=/assets/ort/lib/libonnxruntime.so -e OXIDE_API_OCR_MODEL_DIR=/assets/models \
  -w /work rust:1-bookworm \
  sh -c "./scripts/vendor_pdf_oxide.sh && cargo test ${*:-}"
