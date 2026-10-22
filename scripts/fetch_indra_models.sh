#!/bin/sh
# Indra 0.3.2 model set (v3.1.1), pinned to an immutable Hub commit.
# Includes math models; runtime never downloads models.
set -eu
DEST=${1:-models/indra}
BASE=https://huggingface.co/datalab-to/indra-ocr/resolve/ed345c7f158eddbc7dee0c6a7269afb75a94f7e8
mkdir -p "$DEST"
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}
fetch() {
  if [ -f "$DEST/$1" ] && echo "$2  $DEST/$1" | sha256 -c - >/dev/null 2>&1; then return 0; fi
  curl -fsSL --retry 3 -o "$DEST/$1.part" "$BASE/$1"
  echo "$2  $DEST/$1.part" | sha256 -c - >/dev/null
  mv "$DEST/$1.part" "$DEST/$1"
}
fetch LICENSE 50c88a018a7ee322e89f6b09401b237beb0cd331685879405b3db48993950a5f
fetch config.json cd143c66015620865d9d8361b1bb6f9781363b8e010d78c49b10d69009807d1d
fetch det.onnx 46b0050ce4c4692f887a1a9c038480223af1c7a91b3fcced2657d9cf00290dc4
fetch math_dec_init.onnx 919417fde9962c3318aeb478c1fe3955dd2bb8cd34476c68debed2ba39e5dcd6
fetch math_dec_step.onnx 5ff6e6cbaf0dea70314f66222a1469736a8c5e7994fab06a5b4e53c90469239f
fetch math_enc.onnx d4e4a6c40dad1ee988d32c6403b1590ca42864d4483bbe1e671c5dbb999a5a97
fetch math_meta.json 85d9715fb8a77791b55ef89f57fb41e72eb3170ec58a7c3bee3b394268bf2b10
fetch mathcls.onnx 818b1556e9ef337958db6be0c9791299d3f74a4575586b0cd469aaae9534ff96
fetch meta.json ff6ec58f91c93a475ae9505ad21c73c3ecda0b378f1d889dd83408e10832d1bb
fetch order.onnx 23635dd2324b4ccc00c408af961273a2dedd2e4f0386bc54dba678c41bcf6b46
fetch orient.onnx 8d7bfce5d08bcad55955f41ca494a7f9d9d3ed5d5b6da1f6f276d5c509590729
fetch rec.onnx e243be069b357e56a0e867a21d7482e498df42fb19678bfb67a25f48b1f06151
fetch rec_cjk.onnx 4cec840b55e1bb7f375daf30a2975404a867a71ac1a5bc5b1629c4909ce16baa
fetch script.onnx 5ba37030e817affede0097a7503b6829624104166c6e0b6aacecacf919fc7d11
