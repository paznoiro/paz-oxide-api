#!/usr/bin/env bash
# Local run of the server compose (docker-compose.yml) without SSH or Caddy.
# Sets the compose env with dev defaults and publishes :8080 for smoke tests.
#
#   ./scripts/local.sh         # start oxide-api on http://localhost:8080
#   ./scripts/local.sh down    # stop and remove the container
#
# Override the defaults from your shell: IMAGE_REPO, IMAGE_TAG,
# OXIDE_API_PORT, OXIDE_API_API_KEY, OXIDE_API_OCR_TIMEOUT_SECS, ZAP_ENV_FILE.
# The API key falls back to OXIDE_API_API_KEY in .env, so callers configured
# with that key (zapfiles-api's local.env) keep working across restarts.
set -euo pipefail
cd "$(dirname "$0")/.."

# Read literally, never sourced: a `$` in the key must survive.
dotenv_key() {
  [ -f .env ] || return 0
  sed -n 's/^OXIDE_API_API_KEY=//p' .env | tail -n 1 | tr -d '\r'
}

IMAGE_REPO="${IMAGE_REPO:-ghcr.io/zaporion/oxide-api}"
IMAGE_TAG="${IMAGE_TAG:-0.1.0}"
OXIDE_API_PORT="${OXIDE_API_PORT:-8080}"
OXIDE_API_API_KEY="${OXIDE_API_API_KEY:-$(dotenv_key)}"
OXIDE_API_API_KEY="${OXIDE_API_API_KEY:-local-dev-key-not-secret-12345678}"
OXIDE_API_OCR_TIMEOUT_SECS="${OXIDE_API_OCR_TIMEOUT_SECS:-90}"
ENV_FILE="${ZAP_ENV_FILE:-.oxide-local.env}"

if [ "${1:-up}" = "down" ]; then
  docker rm -f oxide-api >/dev/null 2>&1 || true
  echo "oxide-api stopped"
  exit 0
fi

# Render the container env the way `zd` does (tmpfs on the server; a
# gitignored file here), so a `$` in a secret survives and restarts keep
# working. Values are inserted literally, never re-expanded.
{
  printf 'IMAGE_REPO=%s\n' "$IMAGE_REPO"
  printf 'IMAGE_TAG=%s\n' "$IMAGE_TAG"
  printf 'OXIDE_API_API_KEY=%s\n' "$OXIDE_API_API_KEY"
  printf 'OXIDE_API_OCR_TIMEOUT_SECS=%s\n' "$OXIDE_API_OCR_TIMEOUT_SECS"
} > "$ENV_FILE"

export IMAGE_REPO IMAGE_TAG
export ZAP_ENV_FILE="$ENV_FILE"
docker network create web >/dev/null 2>&1 || true

# Publish :8080 only for the local run. The server compose exposes the port
# on the `web` network with no host mapping; Caddy stays the only entrypoint.
PORTS_OVERRIDE=$(mktemp)
trap 'rm -f "$PORTS_OVERRIDE"' EXIT
printf 'services:\n  oxide-api:\n    ports:\n      - "%s:8080"\n' "$OXIDE_API_PORT" > "$PORTS_OVERRIDE"
docker compose -f docker-compose.yml -f "$PORTS_OVERRIDE" up -d

echo "oxide-api on http://localhost:${OXIDE_API_PORT} (container: oxide-api)"
echo "export OXIDE_API_API_KEY='${OXIDE_API_API_KEY}'   # for make smoke / curl"
