#!/bin/sh
# openapi.yaml is the hand-authored contract; regenerate openapi.json from it.
# (tests/api.rs fails if the two drift apart.)
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
docker run --rm -v "$ROOT:/work" -w /work python:3.12-slim sh -c \
  'pip install -q pyyaml==6.0.2 2>/dev/null && python -c "import yaml,json; json.dump(yaml.safe_load(open(\"openapi.yaml\")), open(\"openapi.json\",\"w\"), indent=2, ensure_ascii=False); print(\"openapi.json written\")"'
