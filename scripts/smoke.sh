#!/usr/bin/env bash
# End-to-end checks against the running container (docker compose up).
#   scripts/smoke.sh                        # http://localhost:8080
#   URL=http://host:8080 scripts/smoke.sh
# Needs curl and python3. Reads OXIDE_API_API_KEY from the env or .env.
set -uo pipefail
cd "$(dirname "$0")/.."
# Read literally, never sourced: a `$` in the key must survive.
if [ -z "${OXIDE_API_API_KEY:-}" ] && [ -f .env ]; then
  OXIDE_API_API_KEY="$(sed -n 's/^OXIDE_API_API_KEY=//p' .env | tail -n 1 | tr -d '\r')"
fi
KEY=${OXIDE_API_API_KEY:?set OXIDE_API_API_KEY}
URL=${URL:-http://localhost:8080}
F=tests/fixtures
AUTH="Authorization: Bearer $KEY"
pass=0; fail=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

ok()  { pass=$((pass + 1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '       %s\n' "${2:0:300}"; }

# req <name> <expected-status> <python-assert-or-empty> <curl args...>
# The python expression sees `j` (parsed JSON or None), `t` (body text), `h` (headers, lower-case keys).
req() {
  local name=$1 want=$2 check=$3; shift 3
  local code
  code=$(curl -s -o "$TMP/body" -D "$TMP/head" -w '%{http_code}' "$@")
  if [ "$code" != "$want" ]; then bad "$name (HTTP $code, want $want)" "$(cat "$TMP/body")"; return; fi
  if [ -z "$check" ]; then ok "$name"; return; fi
  if python3 - "$check" "$TMP/body" "$TMP/head" <<'PY'
import json, sys
check, body, head = sys.argv[1], open(sys.argv[2], encoding="utf-8").read(), open(sys.argv[3]).read()
h = {l.split(":", 1)[0].strip().lower(): l.split(":", 1)[1].strip() for l in head.splitlines() if ":" in l}
try: j = json.loads(body)
except Exception: j = None
t = body
sys.exit(0 if eval(check) else 1)
PY
  then ok "$name"; else bad "$name" "$(head -c 300 "$TMP/body")"; fi
}

wait_ready() {
  for _ in $(seq 1 120); do curl -sf "$1/readyz" >/dev/null && return 0; sleep 0.5; done
  return 1
}

# ---------------------------------------------------------------------------
echo "== $URL"
wait_ready "$URL" || { echo "not ready after 60 s"; exit 1; }

req "healthz"                     200 "j['status']=='ok'"                              "$URL/healthz"
req "readyz"                      200 "j['status']=='ready'"                           "$URL/readyz"
req "version: auth on, OCR en/hi/ar/ta ready" 200 "j['auth'] and j['ocr']['status']=='ready' and all(j['ocr']['languages'][l]=='available' for l in ('en','hi','ar','ta'))" "$URL/version"
# A public deployment keeps /metrics behind Caddy (404); skip those checks there.
metrics=$(curl -s -o /dev/null -w '%{http_code}' "$URL/metrics")
skip_metrics() { [ "$metrics" = 404 ] && printf '  \033[33mSKIP\033[0m %s (/metrics not exposed at %s)\n' "$1" "$URL"; }
skip_metrics "metrics" || \
req "metrics"                     200 "'oxide_http_requests_total' in t"               "$URL/metrics"
req "openapi.json"                200 "'/v1/extract/text' in j['paths']"               "$URL/openapi.json"
req "docs page"                   200 "'api-reference' in t"                           "$URL/docs"
req "security headers"            200 "h.get('x-content-type-options')=='nosniff' and 'x-request-id' in h" "$URL/healthz"

req "no token → 401"              401 "j['type']=='/problems/unauthorized'"  --data-binary @$F/native.pdf "$URL/v1/extract/text"
req "wrong token → 401"           401 ""  -H 'Authorization: Bearer nope-nope-nope-nope' --data-binary @$F/native.pdf "$URL/v1/extract/text"

for spec in sample.docx:docx:docx-alpha sample.xlsx:xlsx:xlsx-alpha sample.pptx:pptx:pptx-alpha \
            sample.doc:doc:docx-alpha sample.xls:xls:xlsx-alpha sample.ppt:ppt:pptx-alpha \
            sample.odt:odt:docx-alpha sample.ods:ods:xlsx-alpha sample.odp:odp:pptx-alpha native.pdf:pdf:page-one-alpha; do
  IFS=: read -r file fmt marker <<<"$spec"
  req "$fmt multipart text"  200 "j['format']=='$fmt' and j['format_source']=='sniff' and '$marker' in j['text']" \
      -H "$AUTH" -F "file=@$F/$file" "$URL/v1/extract/text"
  req "$fmt raw markdown"    200 "j['format']=='$fmt' and '$marker' in j['text']" \
      -H "$AUTH" --data-binary "@$F/$file" "$URL/v1/extract/markdown"
done
b64=$(base64 < $F/sample.xlsx | tr -d '\n')
printf '{"file_base64":"%s"}' "$b64" > "$TMP/req.json"
req "xlsx JSON base64"            200 "j['format']=='xlsx' and 'xlsx-alpha' in j['text']" \
    -H "$AUTH" -H 'Content-Type: application/json' --data-binary @"$TMP/req.json" "$URL/v1/extract/text"
req "wrong extension → sniffed"   200 "j['format']=='xlsx' and j['format_source']=='sniff'" \
    -H "$AUTH" -F "file=@$F/sample.xlsx;filename=report.pdf" "$URL/v1/extract/text"
req "Accept: text/plain"          200 "h['content-type'].startswith('text/plain') and h['x-oxide-page-count']=='3' and 'page-two-beta' in t and 'page-one-alpha' not in t" \
    -H "$AUTH" -H 'Accept: text/plain' --data-binary @$F/native.pdf "$URL/v1/extract/text?pages=2"
req "Accept: text/markdown"       200 "h['content-type'].startswith('text/markdown') and '# ' in t" \
    -H "$AUTH" -H 'Accept: text/markdown' --data-binary @$F/sample.docx "$URL/v1/extract/markdown"
req "pages=1,3"                   200 "j['pages_extracted']==[1,3] and j['page_count']==3" \
    -H "$AUTH" --data-binary @$F/native.pdf "$URL/v1/extract/text?pages=1,3"
req "Hindi native PDF"            200 "'भारत' in j['text'] and j['ocr']['status']=='native'" \
    -H "$AUTH" --data-binary @$F/native_hi.pdf "$URL/v1/extract/text"
req "Arabic presentation forms normalized" 200 "not any(0xFE70<=ord(c)<=0xFEFE or 0xFB50<=ord(c)<=0xFDFF for c in j['text']) and any(0x600<=ord(c)<=0x6FF for c in j['text'])" \
    -H "$AUTH" --data-binary @$F/arabic_forms.pdf "$URL/v1/extract/text"
req "owner-password PDF opens"    200 "'page-one-alpha' in j['text']" -H "$AUTH" --data-binary @$F/owner_only.pdf "$URL/v1/extract/text"
req "locked PDF → 422"            422 "j['type']=='/problems/encrypted-document'" -H "$AUTH" --data-binary @$F/locked.pdf "$URL/v1/extract/text"
req "encrypted DOCX → 422"        422 "j['type']=='/problems/encrypted-document'" -H "$AUTH" --data-binary @$F/encrypted.docx "$URL/v1/extract/text"
req "nested PDF: no 5xx"          422 "" -H "$AUTH" --data-binary @$F/nested.pdf "$URL/v1/extract/text"
req "misfiled xref recovered"     200 "'Misfiled xref recovered' in j['text']" -H "$AUTH" --data-binary @$F/misfiled_xref.pdf "$URL/v1/extract/text"
req "garbage → 422"               422 "j['type']=='/problems/unsupported-format'" -H "$AUTH" --data-binary 'hello world' "$URL/v1/extract/text"
req "lang=fr → 400"               400 "j['parameter']=='lang'" -H "$AUTH" --data-binary @$F/native.pdf "$URL/v1/extract/text?lang=fr"
req "empty body → 400"            400 "" -H "$AUTH" -X POST "$URL/v1/extract/text"
req "Kruti Dev → legacy_font, hi OCR" 200 "j['ocr']['reasons'][0]['reason']=='legacy_font' and j['ocr']['lang']=='hi' and j['ocr']['lang_source']=='font' and j['ocr']['pages']==[1]" \
    -H "$AUTH" --data-binary @$F/legacy_krutidev.pdf "$URL/v1/extract/text"
req "info xlsx sheets"            200 "[s['name'] for s in j['sheets']]==['Q1','Q2'] and j['sheets'][0]['rows']==3" \
    -H "$AUTH" -F file=@$F/sample.xlsx "$URL/v1/info"
req "info pdf pages"              200 "j['page_count']==3 and j['metadata']['title']=='Native Fixture'" \
    -H "$AUTH" --data-binary @$F/native.pdf "$URL/v1/info"
req "info ods sheets"             200 "j['format']=='ods' and [s['name'] for s in j['sheets']]==['Q1','Q2'] and j['sheets'][0]['rows']==3" \
    -H "$AUTH" --data-binary @$F/sample.ods "$URL/v1/info"
req "info tiff pages"             200 "j['kind']=='image' and j['page_count']==2 and j['pages'][0]['width']==1654" \
    -H "$AUTH" --data-binary @$F/scan_pages.tif "$URL/v1/info"
printf '\000\000\000\030ftypheic\000\000\000\000mif1heic' > "$TMP/photo.heic"   # an iPhone photo's header
req "HEIC → 422, named"           422 "j['type']=='/problems/unsupported-format' and 'HEIC' in j['detail']" \
    -H "$AUTH" --data-binary @"$TMP/photo.heic" "$URL/v1/extract/text"
head -c 34000000 /dev/zero > "$TMP/big.bin"
req "33 MB body → 413"            413 "j['type']=='/problems/payload-too-large'" -H "$AUTH" --data-binary @"$TMP/big.bin" "$URL/v1/extract/text"

# --- OCR -------------------------------------------------------------------
req "scan en (probe)"   200 "j['ocr']['status']=='ocr' and j['ocr']['lang']=='en' and j['ocr']['lang_source']=='probe' and 'Invoice 4471' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_en.pdf "$URL/v1/extract/text"
req "scan hi (probe)"   200 "j['ocr']['lang']=='hi' and j['ocr']['lang_source']=='probe' and 'हम भारत' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_hi.pdf "$URL/v1/extract/text"
req "scan ar (probe)"   200 "j['ocr']['lang']=='ar' and j['ocr']['lang_source']=='probe' and 'مرحبا بالعالم' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_ar.pdf "$URL/v1/extract/text"
req "scan ta (probe)"   200 "j['ocr']['lang']=='ta' and j['ocr']['lang_source']=='probe' and 'தமிழ்நாடு அரசு' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_ta.pdf "$URL/v1/extract/text"
req "mixed en+ar scan: Arabic lines kept" 200 "j['ocr']['lang']=='en' and 'Invoice number 4471' in j['text'] and 'فاتورة ضريبية' in j['text'] and 'شكرا لتعاملكم معنا' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_mixed_en_ar.pdf "$URL/v1/extract/text"
req "lang=ar param"     200 "j['ocr']['lang']=='ar' and j['ocr']['lang_source']=='param'" \
    -H "$AUTH" --data-binary @$F/scan_ar.pdf "$URL/v1/extract/text?lang=ar"
req "ocr=off skips OCR" 200 "j['ocr']['status']=='off' and len(j['text'])<20" \
    -H "$AUTH" --data-binary @$F/scan_en.pdf "$URL/v1/extract/text?ocr=off"
req "hybrid: native + stamp, no dupes" 200 "j['ocr']['status']=='hybrid' and 'APPROVED BY FINANCE' in j['text'] and j['text'].count('Purchase order 5521')==1" \
    -H "$AUTH" --data-binary @$F/hybrid.pdf "$URL/v1/extract/text"
req "native PDF never OCR'd" 200 "j['ocr']['status']=='native' and j['ocr']['pages']==[]" \
    -H "$AUTH" --data-binary @$F/native.pdf "$URL/v1/extract/text"
req "ocr=force renders page" 200 "j['ocr']['status']=='ocr' and 'quick brown fox' in j['text']" \
    -H "$AUTH" --data-binary @$F/native.pdf "$URL/v1/extract/text?ocr=force&pages=1"
req "OCR markdown, raw output" 200 "h['x-oxide-ocr-status']=='ocr' and h['x-oxide-lang']=='en' and 'ocr;dur=' in h['server-timing'] and 'Invoice 4471' in t" \
    -H "$AUTH" -H 'Accept: text/markdown' --data-binary @$F/scan_en.pdf "$URL/v1/extract/markdown"
# --- images (straight to OCR) -----------------------------------------------
req "png image → OCR"   200 "j['kind']=='image' and j['format']=='png' and j['ocr']['status']=='ocr' and j['ocr']['lang']=='en' and 'Invoice 4471' in j['text']" \
    -H "$AUTH" -F "file=@$F/scan_en.png" "$URL/v1/extract/text"
req "jpeg, EXIF-rotated" 200 "j['format']=='jpeg' and 'Invoice 4471' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_en_exif.jpg "$URL/v1/extract/text"
req "2-page G4 TIFF"    200 "j['page_count']==2 and j['ocr']['pages']==[1,2] and 'Invoice 4471' in j['text'] and 'tiff-beta' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_pages.tif "$URL/v1/extract/text"
req "TIFF pages=2"      200 "j['pages_extracted']==[2] and 'tiff-beta' in j['text'] and 'Invoice' not in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_pages.tif "$URL/v1/extract/text?pages=2"
req "webp Arabic"       200 "j['format']=='webp' and j['ocr']['lang']=='ar' and 'مرحبا بالعالم' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_ar.webp "$URL/v1/extract/text"
req "JPEG 2000 Hindi"   200 "j['format']=='jp2' and j['ocr']['lang']=='hi' and 'भारत' in j['text']" \
    -H "$AUTH" --data-binary @$F/scan_hi.jp2 "$URL/v1/extract/text"
req "transparent PNG"   200 "'Transparent label 8812' in j['text']" \
    -H "$AUTH" --data-binary @$F/transparent.png "$URL/v1/extract/text"
req "TGA by filename, raw markdown" 200 "h['x-oxide-kind']=='image' and h['x-oxide-format']=='tga' and 'Format check 3141' in t" \
    -H "$AUTH" -H 'Accept: text/markdown' -F "file=@$F/line.tga" "$URL/v1/extract/markdown"
# 2 workers × (1 detector + en/hi/ar/ta recognizers) = 10 ONNX sessions.
skip_metrics "OCR metrics" || \
req "OCR metrics"       200 "'oxide_ocr_pages_total' in t and 'oxide_ocr_engine_builds_total 10' in t" "$URL/metrics"

echo
echo "passed: $pass  failed: $fail"
[ "$fail" -eq 0 ]
