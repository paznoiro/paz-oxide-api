#!/bin/sh
# Regenerates tests/fixtures/* and src/ocr/fixtures/*.png in a throwaway
# toolbox container (LibreOffice, Noto fonts, qpdf, Python libs).
#   scripts/gen_fixtures.sh              # everything
#   scripts/gen_fixtures.sh odf images   # only these groups
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
docker build -q -t oxide-api-fixtures "$ROOT/scripts/fixtures" >/dev/null
docker run --rm -v "$ROOT:/work" oxide-api-fixtures python /work/scripts/fixtures/generate.py "$@"
