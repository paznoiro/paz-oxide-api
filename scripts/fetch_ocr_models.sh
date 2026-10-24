#!/bin/sh
# Downloads the OCR models into $1 (default ./models) in pdf_oxide's flat
# layout, at pinned Hugging Face / GitHub revisions, and verifies sha256.
# File names match pdf_oxide's `OcrLanguage::spec()` (see src/ocr/stack.rs).
set -eu
DEST=${1:-models}
mkdir -p "$DEST"

# Portable sha256: GNU sha256sum on Linux, shasum on stock macOS.
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

DEEPGHS=https://huggingface.co/deepghs/paddleocr/resolve/e7bd63d9c674c96bd4fdab435af0efb383919c18
MONKT=https://huggingface.co/monkt/paddleocr-onnx/resolve/7b02d0a30a07ba2b92ad1ff5a8941ae2c633de65
PADDLE=https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/dab3fe35379033fdcb2d0e9572fac0b36c9a9ebf/ppocr/utils/dict
PADDLE_TA=https://huggingface.co/PaddlePaddle/ta_PP-OCRv5_mobile_rec_onnx/resolve/0db89798af0d5218fad9ea7ace0557526e9077f1
PADDLE_V5_DICT=https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/8ee4a20cc64fe2d73adf96110f9c9b1c3b7b85a5/ppocr/utils/dict

fetch() { # <file> <sha256> <url>
  if [ -f "$DEST/$1" ] && echo "$2  $DEST/$1" | sha256 -c - >/dev/null 2>&1; then
    return 0
  fi
  curl -fsSL --retry 3 -o "$DEST/$1.part" "$3"
  echo "$2  $DEST/$1.part" | sha256 -c - >/dev/null || { echo "sha256 mismatch: $1" >&2; exit 1; }
  mv "$DEST/$1.part" "$DEST/$1"
  echo "fetched $1"
}

# Shared detector (PP-OCRv4 det, ~4.7 MB).
fetch det.onnx             30a86f5731181461d08021402766601e4302a9b9b9666be8aff402696339cdff "$DEEPGHS/det/ch_PP-OCRv4_det/model.onnx"
# en: PP-OCRv5 English recognizer + dictionary.
fetch rec.onnx             4e16deb22c4da6468bdca539b2cd3c8687825538b67109177c47d359ab994cd7 "$MONKT/languages/english/rec.onnx"
fetch en_dict.txt          e025a66d31f327ba0c232e03f407ae8d105e1e709e7ccb3f408aa778c24e70d6 "$MONKT/languages/english/dict.txt"
# hi: Devanagari PP-OCRv3 recognizer + PaddleOCR dictionary.
fetch rec_devanagari.onnx  5a402cae34f84c7d252afbff5d9bbeae292365fc9f8408e862ee44a1e3298c69 "$DEEPGHS/rec/devanagari_PP-OCRv3_rec/model.onnx"
fetch devanagari_dict.txt  b5f1be6d8bbff1a19fb96c5d4ca96a423380234bb7d2ce0e07b5838adb4d18ea "$PADDLE/devanagari_dict.txt"
# ar: Arabic PP-OCRv3 recognizer + PaddleOCR dictionary.
fetch rec_arabic.onnx      1be9c242e6f4cb974ba19e75b55368eee776c8fe411a7150616f82a210b974c0 "$DEEPGHS/rec/arabic_PP-OCRv3_rec/model.onnx"
fetch arabic_dict.txt      637c27c88512c22089bef927b34ada08f748dc132ac70facd68d8202384c2726 "$PADDLE/arabic_dict.txt"
# ta: Tamil PP-OCRv5 mobile recognizer (PaddlePaddle's own ONNX export) + its
# dictionary (byte-identical to the character_dict in the model's inference.yml).
# Chosen over the PP-OCRv3 Tamil model, which emits no spaces and puts the
# left-side vowel signs (ெ ே ை) before their consonant.
fetch rec_ta.onnx          c6d2b682d2a0ea4cb1fccdba295976f93fd439964d16cdc666cadef531accbee "$PADDLE_TA/inference.onnx"
fetch ta_dict.txt          85b541352ae18dc6ba6d47152d8bf8adff6b0266e605d2eef2990c1bf466117b "$PADDLE_V5_DICT/ppocrv5_ta_dict.txt"
