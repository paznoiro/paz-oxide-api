# Oxide API

One small, fast HTTP API that extracts **text** and **markdown** from **PDF**, **Office** (DOCX,
XLSX, PPTX, DOC, XLS, PPT), **OpenDocument** (ODT, ODS, ODP) and **image** files (PNG, JPEG, TIFF,
WebP, GIF, BMP, JPEG 2000, PNM, TGA), with OCR in **English, Hindi, Arabic and Tamil**. Built on
[`pdf_oxide`](https://github.com/yfedoseev/pdf_oxide) and
[`office_oxide`](https://github.com/yfedoseev/office_oxide). See [PLAN.md](PLAN.md) for the design.

It ships as **one container**, `oxide-api`. The same request handles everything: Office and
OpenDocument files and text-layer PDF pages are extracted natively; scanned or legacy-font PDF
pages and images go through OCR.
The image includes Python 3.12, the pinned Indra CLI, ONNX Runtime 1.28.3, PaddleOCR models and Liberation fonts for
page rendering: ≈ 103 MB, 55 MB compressed. It runs as UID 65532 with a read-only root filesystem,
no shell, and a built-in `healthcheck`.

## Container images (GHCR)

Every push or merge to `main` runs `.github/workflows/docker-release.yml`: reserve the next
patch version, run Clippy, native/OCR tests and the Rust 1.92 compatibility checks, smoke-test
the container, then publish a multi-arch (`linux/amd64`, `linux/arm64`) image to GHCR.
Pull requests run checks without publishing. You can also rerun the release workflow manually
on `main`; retries reuse the version already allocated to that commit.

`scripts/release_version.py --reserve` creates a `vX.Y.Z` Git tag at the source commit, using
`Cargo.toml` as the minimum version and incrementing the latest release's patch number.
Versions are reserved before checks, so a failed push can leave a gap. Builds stamp the same
version into Cargo, OpenAPI and the image label; `/version` reports the published version.
No version-bump commit is written back to `main`. Runs queue instead of cancelling earlier pushes.
The workflow uses `GITHUB_TOKEN` for tags and GHCR; no additional registry secret is needed.

```sh
docker pull ghcr.io/paznoiro/paz-oxide-api:0.3.1
docker run -p 8080:8080 -e OXIDE_API_API_KEY=$OXIDE_API_API_KEY --read-only ghcr.io/paznoiro/paz-oxide-api:0.3.1
```

Images are tagged `:X.Y.Z`, `:sha-<full-commit-sha>` and `:latest`. Browse all versions at
<https://github.com/paznoiro/paz-oxide-api/pkgs/container/paz-oxide-api>.

## Quickstart

```sh
./scripts/local.sh               # oxide-api on localhost:8080 (pulls the image, prints its key)
export OXIDE_API_API_KEY=<key-from-script>  # key: shell env, else .env, else a dev default
make smoke                       # 68 end-to-end checks against it
```

Or from local source without compose:

```sh
docker build -t oxide-api .
docker run -p 8080:8080 -e OXIDE_API_API_KEY=$OXIDE_API_API_KEY --read-only oxide-api
```

### Requests

The file type is detected from the content, so one route serves every format. Send the document
as multipart, as base64 JSON, or as the raw body:

```sh
AUTH="Authorization: Bearer $OXIDE_API_API_KEY"

# multipart
curl -H "$AUTH" -F file=@report.docx http://localhost:8080/v1/extract/markdown

# raw body, pages 1-3 only
curl -H "$AUTH" --data-binary @scan.pdf 'http://localhost:8080/v1/extract/text?pages=1-3'

# an image goes straight to OCR (a multi-page TIFF is one page per frame)
curl -H "$AUTH" --data-binary @receipt.jpg http://localhost:8080/v1/extract/text

# JSON (handy for n8n / Dify / Zapier)
curl -H "$AUTH" -H 'Content-Type: application/json' \
     -d "{\"file_base64\":\"$(base64 < sheet.xlsx | tr -d '\n')\", \"format\":\"xlsx\"}" \
     http://localhost:8080/v1/extract/text

# raw text output instead of JSON; metadata moves into X-Oxide-* headers
curl -H "$AUTH" -H 'Accept: text/plain' --data-binary @doc.pdf http://localhost:8080/v1/extract/text

# structure and metadata only
curl -H "$AUTH" -F file=@sheet.xlsx http://localhost:8080/v1/info
```

A PDF response (`/v1/extract/text`):

```json
{
  "kind": "pdf", "format": "pdf", "format_source": "sniff",
  "text": "Scanned Invoice 4471\nTotal amount due: 1,250.00\n...",
  "char_count": 104, "truncated": false,
  "page_count": 1, "pages_extracted": [1], "incomplete": false,
  "ocr": { "status": "ocr", "engine": "paddle", "lang": "en", "lang_source": "probe",
           "pages": [1], "pages_needing_ocr": [], "failed_pages": [],
           "reasons": [{ "page": 1, "reason": "scanned" }] }
}
```

### Endpoints

| Endpoint | Office, OpenDocument | PDF | Images |
|---|---|---|---|
| `POST /v1/extract/text` | plain text | per-page native text, OCR for pages that need it | OCR text per page |
| `POST /v1/extract/markdown` | headings, lists, tables | per-page markdown with heading and table detection | OCR lines and paragraphs |
| `POST /v1/extract/document` | — | page text, positioned blocks, reading order and OCR confidence (JSON only) | — |
| `POST /v1/extract/fields` | named values from the text, by regular expression | the same, over native + OCR text | the same, over OCR text |
| `POST /v1/info` | metadata, sheet names + dimensions, slide count | metadata, page count, encryption, page geometry | page count, pixel dimensions |
| `GET /healthz` `/readyz` `/version` `/metrics` `/openapi.json` `/openapi.yaml` `/docs` | unauthenticated ops endpoints | | |

An image response has the same shape as a PDF response, with `"kind": "image"` and `"format"`
set to `png`, `jpeg`, `tiff`, `webp`, `gif`, `bmp`, `jp2`, `pnm` or `tga`.

Parameters (query string, multipart parts or JSON keys; never nested):

| Param | Applies to | Values |
|---|---|---|
| `format` | any | `pdf docx xlsx pptx doc xls ppt odt ods odp png jpeg tiff webp gif bmp jp2 pnm tga` (aliases like `jpg`, `tif`, `xlsm`, `ott` work too): a hint, used only when the content is inconclusive |
| `pages` | PDF, images | `1-3,5` (1-based; TIFF pages) |
| `ocr` | PDF, images | `auto` (default), `force`, `off` |
| `ocr_engine` | PDF, images | `paddle` (default), `indra` |
| `include_debug` | extraction | `false` (default); `true` returns flow steps and forces JSON |
| `lang` | PDF, images | `auto` (default), `en`, `hi`, `ar`, `ta` |
| `detect_headings` | PDF markdown | `true` (default) / `false` |
| `clean` | any extraction | `false` (default) / `true`: collapse extra spaces/tabs to one space, trailing whitespace removed, 3+ newlines to one blank line |
| `filename` | any | last-resort format hint from the extension |

Parameters that don't apply to the detected document kind are ignored and listed in
`ignored_params`. The full contract is in [openapi.yaml](openapi.yaml) (served at `/openapi.json`,
with an interactive reference at `/docs`). A Postman collection with examples for every format is in
[docs/postman](docs/postman/README.md).

### Fields

`/v1/extract/fields` reads the document exactly as `/v1/extract/text` does, then runs named
regular-expression fields over that one text. OCR happens once, however many fields there are.
Send `text` instead of a document to run fields over text you already have:

```sh
curl -H "$AUTH" -H 'Content-Type: application/json' http://localhost:8080/v1/extract/fields -d '{
  "file_base64": "'"$(base64 < scan.pdf | tr -d '\n')"'",
  "fields": [
    {"name": "invoice_number", "patterns": ["(?i)invoice\\s*(?:no|number|#)?[:.]?\\s*([A-Z0-9 _./-]+)"], "required": true, "max_length": 40},
    {"name": "total", "patterns": ["(?i)total amount due:?\\s*([\\d,]+\\.\\d{2})", "(?i)total\\s+([\\d,]+\\.\\d{2})"], "type": "amount"},
    {"name": "invoice_date", "patterns": ["(?i)invoice date:?\\s*([^\\n]+)"], "type": "date"},
    {"name": "po_number", "patterns": ["(?i)PO Number\\s+([^\\n]+)"]}
  ]}'
```

```json
{ "kind": "pdf", "format": "pdf", "format_source": "sniff",
  "fields": { "invoice_number": "4471", "total": "1250.00", "invoice_date": "2026-10-14", "po_number": null },
  "matched": { "invoice_number": "4471", "total": "1,250.00", "invoice_date": "14 Oct 2026", "po_number": null },
  "missing_required": [], "char_count": 104, "truncated": false,
  "page_count": 1, "pages_extracted": [1], "incomplete": false, "ocr": { "status": "ocr", ... } }
```

| Spec key | Default | Meaning |
|---|---|---|
| `name` | *(required)* | key in `fields`: a letter, then letters, digits, `_` (≤ 64) |
| `patterns` | *(required)* | 1–10 regular expressions, tried in order; the first valid match wins |
| `capture_group` | group 1, or the whole match when the pattern has no group | a number (`0` = whole match) or a group name; must exist in every pattern |
| `required` | `false` | a required field that matches nothing is listed in `missing_required` (still `200`) |
| `type` | `text` | `text`, `date`, `amount`, `gstin`, `pan`, `iban`: a match that doesn't read as the type is skipped |
| `format` | `dmy` / `point` | `date`: `dmy`, `mdy`, `ymd` for all-numeric dates; `amount`: decimal mark `point` or `comma` |
| `normalization` | `trim` | `trim` or `none`; `text` fields only |
| `max_length` | none | a longer value is skipped (the next one is tried), never cut down |
| `multiple` | `false` | every distinct valid match, in order, as an array |
| `selection` | `first` | the only value today |

- **Typed fields check and rewrite the value.** `date` → `2026-10-14` from `14/10/2026`,
  `14 Oct 2026`, `Oct 14, 2026` and the like (English month names; no 31 April). `amount` → a plain
  decimal from `₹ 1,25,000.50`, `USD 1,250`, `(1,250.00)`, `1 250,00 €` (`format: comma`): digits
  grouped in threes or the Indian way, decimals never rounded. `gstin` must pass its check
  character, `iban` its mod-97 check, `pan` its shape. So "Date: see overleaf" is skipped and the
  next date is taken, and a misread GSTIN is dropped rather than stored. `matched` returns what each
  value was read from, as printed, so a caller can mark it in the text.
- **Every requested field is in the response**, `null` when nothing matched. (The other endpoints
  leave optional keys out; here the absence is the answer.) Unknown spec keys are a `400`.
- **Rust `regex` syntax**, linear time: `(?i)`, `(?m)`, `(?s)` work; look-around and backreferences
  don't and are refused with the engine's reason. A value on the next line is `Label\s+([^\n]+)`.
  `\d`, `\w` and `(?i)` are Unicode-aware. Values are reported as read: no `O`→`0` correction.
- **Specs are checked first**, so a bad pattern is a `400` (with `parameter: fields[1].patterns[0]`)
  and costs no OCR. Limits: 50 fields, 100 patterns, 1000 characters per pattern. Broad counted
  classes compile large (`\w{1,200}` ≈ 10 MiB; `[A-Za-z0-9]{1,200}` ≈ 0.2 MiB), and a request's
  larger patterns share a 40 MiB budget.
- **`include_text=true`** also returns the text, so an indexer gets text and fields from one read.
- **Matching has its own deadline**: `OXIDE_API_PROCESS_TIMEOUT_SECS`, started once the text is read,
  since OCR may have used the request's. A request that OCRs can therefore run past
  `OXIDE_API_OCR_TIMEOUT_SECS` by up to that much (matching normally takes milliseconds).
- In a JSON body `fields` is the array; as a multipart part or query param it's the array as a JSON
  string. `Accept: text/plain` isn't negotiated here: the response is always JSON.

### Selectable Indra CLI and response diagnostics

All PDF/image extraction endpoints accept `ocr_engine=paddle|indra`. Paddle remains
the default. `ocr=auto|force|off` and `pages` retain the same meaning. Indra's CLI
receives only selected OCR pages, as bounded grayscale PNG batches (four pages,
one decoded page at a time). Native PDF text is retained and hybrid pages are
merged with OCR using the shared flow. The CLI uses `--format json --mode ocr
--cores 1 --keep-furniture`; **math is enabled by default**. Disable it with
`OXIDE_API_INDRA_MATH=false` in the container environment/deployment properties.
`lang` selects Paddle recognizers; Indra selects scripts itself, so a supplied
`lang` is listed in `ignored_params`. Script support depends on the pinned Indra
model set, independent of Paddle's `en,hi,ar,ta` recognizers.

```sh
curl -H "$AUTH" -F file=@invoice.pdf \
  'http://localhost:8080/v1/extract/document?ocr_engine=indra&include_debug=true'
curl -H "$AUTH" -F file=@receipt.png \
  'http://localhost:8080/v1/extract/markdown?ocr_engine=indra&include_debug=true'
```

`ocr.engine` identifies the selected backend, including when OCR is skipped.
Structured output preserves line confidence (null if unavailable) and transforms
Indra's oriented image boxes back into displayed PDF coordinates. Markdown uses
Indra's markdown, including LaTeX math when recognized. The structured document
endpoint remains PDF-only; images use text, markdown or fields endpoints.

With `include_debug=true`, JSON includes `debug.steps`, in execution order:

- `pdf_oxide_parse` (or `image_decode_plan`): page selection/settings and complete
  native page text, structured blocks where requested, and classification results.
- `ocr_routing`: selected engine, settings and pages/reasons requiring OCR.
- `indra_cli` per batch: actual CLI settings, original page numbers and image
  dimensions, followed by Indra JSON lines with text, boxes, confidence, markdown,
  layout blocks and internal timings.
- `ocr_parse`: normalized per-page text/blocks, failures and timed-out pages.
- `assemble`: merge/cleanup settings and final text or structured pages and metadata.

Each step contains `status`, `elapsed_ms`, `input` and `output`. OCR errors after
routing also carry the trace in their problem JSON. Binary uploads, base64,
filenames, credentials, subprocess stderr and local file paths are excluded;
trace content is returned to the caller and never logged. Inputs/outputs share
`OXIDE_API_MAX_DEBUG_BYTES`. Oversized sections become
`{truncated:true, original_bytes:..., preview:...}`, and `debug.truncated` is true.
`include_debug=true` forces JSON even with a raw-text Accept header. Keep it off
for ordinary calls because it duplicates extracted document content.

The image packages `indra-ocr==0.3.2` and immutable, checksum-verified model files,
including math. Python dependencies are hash-locked in `requirements-indra.txt`.
Models stay read-only, while temporary images use `/tmp` and are deleted after
inference. Queueing, page preparation and CLI execution share the OCR deadline;
timeout/cancellation kills the CLI. Indra is independent of Paddle readiness:
selecting Indra when its CLI/models are missing returns `503 ocr-unavailable`
when OCR is needed. A non-OCR build still supports native parsing, but cannot
run Indra. The CLI reloads models per batch; concurrency is bounded separately
from the existing request admission limit. Verify a running image with
`python3 scripts/smoke_indra.py --base-url http://127.0.0.1:8080` after exporting
`OXIDE_API_API_KEY`; use `--expect-math false` for a math-disabled instance.

### Structured document extraction

`POST /v1/extract/document` accepts **PDF only**, in the same multipart, base64 JSON and raw
body forms as `/v1/extract/text`. It supports digital, scanned and mixed PDFs, including pages
with both native text and scanned content. Parameters: `pages`, `ocr=auto|force|off`,
`lang=auto|en|hi|ar|ta`, `clean`, `format`, `filename`. The response is always JSON.

```sh
curl -H "$AUTH" -F file=@invoice.pdf 'http://localhost:8080/v1/extract/document?lang=en'
```

A one-block response (illustrative OCR text and geometry):

```json
{
  "schema_version": "1.0",
  "kind": "pdf", "format": "pdf", "format_source": "sniff",
  "coordinate_system": {"unit": "pt", "origin": "top_left", "x_direction": "right", "y_direction": "down"},
  "page_count": 1, "pages_extracted": [1], "char_count": 12,
  "truncated": false, "incomplete": false,
  "ocr": {"status": "ocr", "engine": "paddle", "lang": "en", "lang_source": "param", "pages": [1],
          "pages_needing_ocr": [], "failed_pages": [], "reasons": [{"page": 1, "reason": "scanned"}]},
  "pages": [{
    "page": 1, "width": 595.44, "height": 842.04, "rotation": 0,
    "text": "Invoice 4471", "truncated": false,
    "blocks": [{"id": "p1-b0", "reading_order": 0, "kind": "text", "text": "Invoice 4471",
                "bbox": {"x": 43.2, "y": 48.0, "width": 210.0, "height": 20.0},
                "source": "ocr", "confidence": null}]
  }]
}
```

`confidence` is null when unavailable; the example omits a numeric score deliberately. This
service's PaddleOCR recognizers provide real recognition scores in `[0,1]`, which are retained
for OCR blocks. A word-split region uses the existing character-weighted mean of those scores.
Native blocks always have `confidence: null`, even for a scanner's existing invisible OCR layer:
PDF text does not carry the original OCR score. No document/page quality scores are invented.

- **Text and layout.** Each page's `text` is its `blocks` joined with newlines. `char_count` sums
  those page texts. Blocks contain PDF-native spans or OCR-detected text regions, `source`, and
  zero-based `reading_order`; array order is the reading order. Native blocks also include
  `font` (`name`, `size`, `bold`, `italic`) and a `heading_level` when supplied by PDF-Oxide.
  `kind` can be `text`, `heading`, `header`, `footer`, `page_number`, or `artifact`.
- **Geometry.** `bbox` is `{x,y,width,height}` in PDF points from the displayed page's top-left
  corner (x right, y down), after CropBox clipping and page rotation. Page `width`/`height` use
  that same frame; `rotation` reports the already-applied source rotation. Missing geometry is
  null. A shortened final block retains the full original region's box.
- **Reading order.** Digital pages use PDF-Oxide's positioned-span order (geometric column
  handling and tagged table ordering where supported). OCR uses the shared line/RTL ordering.
  Mixed pages retain native blocks, discard matching OCR text using the shared line comparison,
  then order combined blocks geometrically. Order and heading roles are best effort; this does
  not reconstruct table cells or recover exact visual styling.
- **Cost.** The shared PDF reader, classifier, language resolver and OCR worker/model pool are
  reused. Digital pages require no inference. Only pages routed to OCR are rendered, once, so
  every image on the page is included and OCR/native boxes share a frame. No external services,
  model downloads per request, new dependencies or LLM calls are added.
- **Limits and partial results.** Existing upload/page/deadline/text limits apply. Text counts
  once against the budget; the final block can be shortened. `pages_extracted` lists exactly
  the returned pages. `incomplete` marks a deadline; `truncated` marks a text budget. Empty
  pages remain present. When OCR is unavailable, native content is retained and
  `ocr.pages_needing_ocr` identifies the pages requiring another attempt. OCR failures are
  reported through `ocr.failed_pages` with the existing `ocr=force` error behavior.

Pass the returned `pages` (or just their `blocks` to reduce duplicate text), together with your
user-provided JSON schema, to your downstream LLM. Ask it to return schema-conforming values
and cite page/block IDs for evidence; treat document text as untrusted data, not instructions.
IDs such as `p1-b0` identify positions within this extraction, not permanent document objects.
This API does not accept a target JSON schema or perform LLM mapping. The complete contract is
in [openapi.yaml](openapi.yaml) and served at `/docs`.

### Errors

RFC 9457 `application/problem+json`, with `type: /problems/<slug>`.

| Status | When |
|---|---|
| 400 | malformed input or an invalid parameter (`parameter` and `accepted` name the fix) |
| 401 | missing or wrong bearer token |
| 408 | the upload took longer than 30 s |
| 413 | body over 32 MiB (for JSON this counts the base64 text) |
| 422 | unsupported, corrupt or encrypted document (`encrypted-document`, `corrupt-document`, `unsupported-format`, `too-many-pages`, `image-too-large`), `processing-timeout`, or `ocr-failed` when every page's OCR fails with `ocr=force` or on an image |
| 429 | all 8 processing slots busy for 1 s (honour `Retry-After`) |
| 503 | the instance is draining for shutdown |

A PDF or image that runs out of time returns `200` with `incomplete: true` and the pages done so far
in `pages_extracted`, so the caller can request the rest with `pages=`. A locked PDF, or a
password-protected DOCX or ODF file, is always a `422`, never an empty `200`. An
owner-password-only PDF opens normally.

### Formats

- **OpenDocument** (ODT, ODS, ODP and their templates) is read by this service's own reader
  (`src/odf.rs`), which builds the same document model `office_oxide` uses, so the text and markdown
  read like DOCX/XLSX/PPTX output: headings, lists, tables, one `## Sheet` table per sheet, one
  section per slide with speaker notes, footnotes at the end. Tracked deletions, comments and
  headers/footers are left out. `content.xml` is parsed as it inflates, empty spreadsheet rows and
  cells are never expanded, and a document stops at the text budget or 2 million cells.
- **Images** (PNG, JPEG, TIFF, WebP, GIF, BMP, JPEG 2000, PNM, TGA) skip native extraction and go
  straight to OCR, one page per TIFF page (including CCITT G4 fax TIFFs). EXIF/TIFF orientation is
  applied, so a sideways phone photo reads correctly, and transparent backgrounds are composited
  onto white. Animated GIF/WebP use their first frame. TGA has no signature, so it needs a `.tga`
  filename or `format=tga`.
- **Not supported:** HEIC/HEIF (iPhone photos), AVIF and JPEG XL. They're recognized and rejected
  with a `422` that names the format, because there's no pure-Rust decoder for them. Convert to
  JPEG or PNG first. ODG drawings, flat XML ODF (`.fodt`) and the OpenOffice 1.x formats (`.sxw`)
  aren't supported either.

## OCR

- **Routing (`ocr=auto`).** Every image page is OCR'd. PDF pages classified as scanned are OCR'd. Hybrid pages (native text plus a
  text-bearing image) keep their native text and gain only the OCR lines that aren't already in it.
  Pages set in a legacy Hindi font (Kruti Dev, Chanakya, DevLys, Walkman-Chanakya, Shusha, Agra,
  APS-DV) are rendered and OCR'd with `hi`, because their text layer is mojibake. A scan carrying a
  scanner's invisible OCR layer keeps that layer unless its words are malformed (mixed case,
  symbols inside words, as when a scanner OCRs handwriting); then the junk is dropped and the page
  is OCR'd as `scanned`. Text-layer pages never go through inference. If OCR recognizes nothing on a
  page that has native text, the native text is kept and the page is listed in `ocr.failed_pages`;
  a page whose junk layer was dropped comes back empty there, or in `ocr.pages_needing_ocr`
  where OCR is unavailable.
- **Language (`lang=auto`)**, resolved once per document: a legacy Hindi font gives `hi`. Otherwise the
  script of the native text decides. On a pure scan, a probe recognizes the 8 largest text boxes of the
  first OCR page with every recognizer, and the most confident one whose output is in its own script
  wins. If nothing else applies, `en`. The response reports `lang` and
  `lang_source: param|font|text|probe|default`.
- **Startup.** Each OCR worker builds one detector plus `en`/`hi`/`ar`/`ta` recognizers once, then runs a
  golden image per language. `/readyz` turns green only after that, about 0.3 s after start. A
  language that fails is reported `unavailable` by name in `/version`, and requests needing it get
  native text with `ocr.status: "unavailable"`. Missing models or a missing ONNX Runtime disable OCR,
  but native extraction keeps working.
- **Quality.** English (PP-OCRv5) and Arabic are accurate on clean scans. Arabic comes out in logical
  order, with presentation forms normalized to base letters. The only published Devanagari recognizer
  (PP-OCRv3) reaches about 0.8 character accuracy: it drops some vowel signs and conjuncts. Both v3
  recognizers rarely emit spaces, so the service splits each line at word gaps and recognizes the words
  separately. On 88 labelled lines that raised space-aware accuracy from 0.68 to 0.82 for Hindi and
  from 0.87 to 0.94 for Arabic. A stronger Devanagari model can be dropped into `/models` when one
  exists in ONNX form. Tamil uses PaddlePaddle's own ONNX export of the PP-OCRv5 mobile recognizer,
  which spaces words and orders vowel signs itself, so it needs neither step. On 48 labelled lines
  (Noto Sans/Serif Tamil, blur + noise) it reached 0.94 character and space-aware accuracy with 32
  lines exact. The PP-OCRv3 Tamil model scored 0.72, emitted no spaces and put ெ/ே/ை before their
  consonant. ௌ comes out as the identical-looking ெ + ள.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `OXIDE_API_ADDR` | `0.0.0.0:8080` | bind address |
| `OXIDE_API_API_KEY` | *(required)* | bearer token for `/v1/*`, at least 16 characters. Startup fails without it on a non-loopback bind |
| `OXIDE_API_CPU_THREADS` | cgroup-aware CPU count | CPU pool size (also split across OCR workers for ONNX Runtime) |
| `OXIDE_API_MAX_INFLIGHT` | `8` | admission permits = max request bodies in memory |
| `OXIDE_API_QUEUE_WAIT_MS` | `1000` | max wait for a permit before `429` |
| `OXIDE_API_RETRY_AFTER_SECS` | `10` | `Retry-After` on `429` |
| `OXIDE_API_MAX_BODY_BYTES` | `33554432` | upload ceiling |
| `OXIDE_API_BODY_TIMEOUT_SECS` | `30` | slow upload → `408` |
| `OXIDE_API_PROCESS_TIMEOUT_SECS` | `30` | native deadline, measured from admission |
| `OXIDE_API_MAX_TEXT_CHARS` | `4194304` | per-document output budget |
| `OXIDE_API_MAX_PDF_PAGES` | `2000` | PDF-bomb guard; also caps TIFF pages |
| `OXIDE_API_MAX_IMAGE_PIXELS` | `64000000` | per image page, checked from the headers before anything is decoded (a 48 MP phone photo fits) |
| `OXIDE_API_DRAIN_DELAY_SECS` | `2` | time between `/readyz` flipping and the listener closing on SIGTERM |
| `OXIDE_API_OCR_WORKERS` | `2` | OCR workers, each with its own detector + recognizers |
| `OXIDE_API_OCR_TIMEOUT_SECS` | `120` | deadline for requests that OCR any page |
| `OXIDE_API_OCR_LANGUAGES` | `en,hi,ar,ta` | recognizers to load and smoke-test |
| `OXIDE_API_OCR_MODEL_DIR` | `/models` | Paddle model directory (flat upstream layout) |
| `OXIDE_API_INDRA_CLI` | `indra-ocr` | executable path; invoked directly without a shell |
| `OXIDE_API_INDRA_MODEL_DIR` | `/models/indra` | pinned local Indra models, including math; no runtime downloads |
| `OXIDE_API_INDRA_MATH` | `true` | pass `--math`; set `false` to disable math |
| `OXIDE_API_INDRA_MAX_CONCURRENT` | `1` | concurrent Indra requests; each CLI uses one core |
| `OXIDE_API_MAX_DEBUG_BYTES` | `1048576` | shared byte budget for opt-in diagnostic inputs and outputs |
| `ORT_DYLIB_PATH` | `/usr/lib/libonnxruntime.so` | set in the image |
| `RUST_LOG` | `info,pdf_oxide=error,office_oxide=error,ort=warn` | JSON logs. Request bytes, filenames and extracted text are never logged. Setting it replaces the whole filter, so keep the engine entries |

## Operations

- **Admission before the body is read.** A waiting request costs a socket, not 32 MiB. Rejected
  uploads (401/429/503) are drained in the background without buffering, so clients receive the
  status instead of a connection reset.
- **The permit belongs to the job.** Engine work can't be cancelled, so a timed-out request keeps its
  slot until its work really ends. Work never piles up beyond `MAX_INFLIGHT`.
- **Crash safety.** Engine calls run under `catch_unwind` on 16 MiB-stack workers. A watchdog fails
  `/healthz` if a job runs past 10× its deadline. `pdf_oxide`'s object parser is patched with a nesting
  limit (see below), because an unbounded recursion would otherwise abort the process.
- **Request logs** are JSON, with method, path, request ID, status and duration in milliseconds.
  `/healthz` calls are silent, including unsuccessful probes. Other requests log at INFO,
  client errors at WARN, and server errors at ERROR. API errors include their problem category
  and static title; query strings, tokens, document bytes and engine error details are omitted.
- **Metrics** (`/metrics`, Prometheus): RED per route, admission wait, in-flight, 429s, documents per
  kind/format, OCR pages and latency per language, `lang_source`, engine panics and timeouts.
- **Measured performance** is in [docs/performance.md](docs/performance.md). Integration recipes for
  n8n and Dify are in [docs/integrations.md](docs/integrations.md).

## Deployment

The workflow publishes images; deploying a container is a separate operation.
`docker-compose.yml` pulls from GHCR and exposes port 8080 on the external `web` network.
Provide `OXIDE_API_API_KEY` and any other settings in a local, untracked `.env` file, then run:

```sh
docker network create web                  # once, if the network does not exist
IMAGE_TAG=0.3.1 docker compose pull
IMAGE_TAG=0.3.1 docker compose up -d
```

Choose the version from the successful workflow summary. `IMAGE_TAG=latest` follows the newest
successful build when you explicitly pull it. Configure your reverse proxy separately and keep
`/metrics` private. For local use, `./scripts/local.sh` publishes port 8080 without a proxy.
Use `docker logs oxide-api` and
`docker exec oxide-api oxide-api healthcheck` for diagnostics.

## Development

No local Rust is needed: everything runs in containers.

```sh
make test         # unit + API + OCR tests; fetches ONNX Runtime + models (sha256-pinned) once
make lint         # clippy -D warnings, with and without the `ocr` feature
make image        # paz-oxide-api:latest
make smoke        # end-to-end checks against the running container (curl + python3)
make fixtures     # regenerate tests/fixtures and the OCR golden images (LibreOffice, Noto, qpdf)
make openapi      # regenerate openapi.json from the hand-authored openapi.yaml
```

The layout follows PLAN.md §6. `src/lib.rs` holds the router and state so the tests drive the real
service in-process. `src/office.rs` wraps `office_oxide` and `src/odf.rs`; `src/raster.rs` sniffs,
probes and decodes images. OCR is a default Cargo feature; `--no-default-features` still builds a
native-only binary (images then report `ocr.status: "unavailable"`).

### Patches to `pdf_oxide`

`scripts/vendor_pdf_oxide.sh` fetches `pdf_oxide` 0.3.78 from crates.io (sha256-pinned) into
`vendor/` and applies `patches/` (Cargo uses it through `[patch.crates-io]`):

1. **`office-oxide-0.1.12`**: `pdf_oxide` 0.3.78 builds `office_oxide::DocumentIR` with a struct
   literal that predates a field added in `office_oxide` 0.1.12, so it doesn't compile against 0.1.13.
   0.1.11 compiles, but it lacks `office_oxide::limits`, the spreadsheet fan-out guard. One line:
   `..Default::default()`.
2. **`nesting-limit`**: the object parser recursed without bound on nested arrays and dictionaries. A
   crafted PDF overflowed any thread stack and aborted the process (`tests/fixtures/nested.pdf`).
   Nesting is now capped at 256.
3. **`misfiled-xref`**: an xref entry whose offset holds a different object was used anyway. One stray
   row in an xref stream (written by pdf-lib after stamping a form) shifts every later entry, so the
   ObjStm lookup hit an image XObject and the whole PDF came back `corrupt-document`
   (`tests/fixtures/misfiled_xref.pdf`). Such objects are now found by header scan or in the object
   streams; when neither has them, the old outcome stands.

4. **`markdown-tables`**: HTML-generated PDFs may draw a separate underline for each cell,
   without vertical rules. Repeated matching border segments now define the actual columns
   and row bands, keeping wrapped descriptions in their cell. Tables participate in Markdown
   reading order as whole blocks, so a side-by-side buyer is read before the item table.
   `tests/fixtures/facturx-basic.pdf` is the InvoiceXML demonstration invoice (test data only)
   used to check the six-column item table and the two-row VAT table through the API.

Drop them once upstream ships fixes. Markdown preserves semantic structure, not the PDF's exact
page styling; scanned pages still have only the lines and paragraphs supplied by OCR.

## Deviations from PLAN.md

| Plan | Built | Why |
|---|---|---|
| Two images (`:slim` < 20 MB, `:ocr` ≈ 100 MB) | **one image** with OCR | one container that does everything is simpler to run |
| PDF and Office input | adds **OpenDocument** and **images** | requested later; ODF goes through an own reader into `office_oxide`'s document model, images straight to OCR |
| OCR in `en`, `hi`, `ar` | adds **`ta`** (Tamil, PP-OCRv5) | requested later; the v5 model needs no word splitting or vowel reordering (measured above) |
| ONNX Runtime 1.23.x | **1.28.3** | `ort` 2.0.0-rc.13 (pinned by `pdf_oxide`) enables `api-27` and refuses older runtimes |
| Stock `pdf_oxide` | four patches | see above |
| OCR reads the page's largest image | same, **plus page rendering** for legacy-font pages, `ocr=force` and pages without a usable scan | Kruti Dev pages are vector text with no image to OCR |
| Image has no fonts | adds Liberation Sans/Serif/Mono (≈ 2.4 MB) | rendering a page whose Helvetica/Times/Courier isn't embedded otherwise produces a blank image |
| `page_font_face_lookups()` | reads each requested page's `/Resources /Font` | the upstream call walks every page of the document |
| Plain upstream recognition | adds word-gap segmentation (`hi`, `ar`), visual→logical `ि` reordering, PaddleOCR's Arabic run reversal | the v3 recognizers emit visual order and few spaces (measured above) |
| `cargo-chef` | BuildKit cache mounts | simpler with the vendored path dependency; same rebuild speed locally |
| k6 | `scripts/loadtest.py` (stdlib) | no extra tooling needed |

## Validation and limitations

The release workflow runs native-only and OCR-enabled tests, Clippy with warnings denied,
Rust 1.92 compatibility checks, and container smoke tests before publishing. See
[GitHub Actions](https://github.com/paznoiro/paz-oxide-api/actions/workflows/docker-release.yml)
for the status of each commit and its published image digest.

Container limits (4 CPU / 3 GiB) are starting values; tune them using production document sizes.
The legacy Hindi font fixture exercises routing rather than a real Kruti Dev font, and there
is no JBIG2 scan fixture. Publishing an image does not deploy it to a server.
