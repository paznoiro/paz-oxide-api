# Oxide API

One small, fast HTTP API that extracts **text** and **markdown** from **PDF**, **Office** (DOCX,
XLSX, PPTX, DOC, XLS, PPT), **OpenDocument** (ODT, ODS, ODP) and **image** files (PNG, JPEG, TIFF,
WebP, GIF, BMP, JPEG 2000, PNM, TGA), with OCR in **English, Hindi and Arabic**. Built on
[`pdf_oxide`](https://github.com/yfedoseev/pdf_oxide) and
[`office_oxide`](https://github.com/yfedoseev/office_oxide). See [PLAN.md](PLAN.md) for the design.

It ships as **one container**, `oxide-api`. The same request handles everything: Office and
OpenDocument files and text-layer PDF pages are extracted natively; scanned or legacy-font PDF
pages and images go through OCR.
The image is `distroless/cc` with ONNX Runtime 1.28.3, the PaddleOCR models and Liberation fonts for
page rendering: ≈ 103 MB, 55 MB compressed. It runs as UID 65532 with a read-only root filesystem,
no shell, and a built-in `healthcheck`.

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
  "ocr": { "status": "ocr", "lang": "en", "lang_source": "probe",
           "pages": [1], "pages_needing_ocr": [], "failed_pages": [],
           "reasons": [{ "page": 1, "reason": "scanned" }] }
}
```

### Endpoints

| Endpoint | Office, OpenDocument | PDF | Images |
|---|---|---|---|
| `POST /v1/extract/text` | plain text | per-page native text, OCR for pages that need it | OCR text per page |
| `POST /v1/extract/markdown` | headings, lists, tables | per-page markdown with heading detection | OCR lines and paragraphs |
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
| `lang` | PDF, images | `auto` (default), `en`, `hi`, `ar` |
| `detect_headings` | PDF markdown | `true` (default) / `false` |
| `filename` | any | last-resort format hint from the extension |

Parameters that don't apply to the detected document kind are ignored and listed in
`ignored_params`. The full contract is in [openapi.yaml](openapi.yaml) (served at `/openapi.json`,
with an interactive reference at `/docs`). A Postman collection with examples for every format is in
[docs/postman](docs/postman/README.md).

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
- **Startup.** Each OCR worker builds one detector plus `en`/`hi`/`ar` recognizers once, then runs a
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
  exists in ONNX form.

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
| `OXIDE_API_OCR_LANGUAGES` | `en,hi,ar` | recognizers to load and smoke-test |
| `OXIDE_API_OCR_MODEL_DIR` | `/models` | model directory (flat upstream layout) |
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
- **Metrics** (`/metrics`, Prometheus): RED per route, admission wait, in-flight, 429s, documents per
  kind/format, OCR pages and latency per language, `lang_source`, engine panics and timeouts.
- **Measured performance** is in [docs/performance.md](docs/performance.md). Integration recipes for
  n8n and Dify are in [docs/integrations.md](docs/integrations.md).

## Deployment (zd)

`deployments/` follows the zap-infra layout and deploys with the zap CLI (`zd`, `zb`, `zbd`) to the
"docs" VPS, the same server, Caddy and `web` network that run Collabora
(`zap-infra/collabora/deployments/production.docs.yaml`).

| File | Purpose |
|---|---|
| `deployments/production.docs.yaml` | target host, domain, image and `OXIDE_API_*` settings |
| `docker-compose.yml` | the server compose: pulls `ghcr.io/zaporion/oxide-api`, no host port, hardened (local run: `./scripts/local.sh`) |
| `deployments/caddy-sites/oxide-api.caddy` | Caddy site: TLS, 40 MB body cap, `/metrics` kept off the internet |
| `scripts/local.sh` | local run of the server compose on `localhost:8080`, no SSH or Caddy |

```sh
export OXIDE_API_API_KEY=$(openssl rand -hex 32)          # keep it: clients need it
zd  deployments/production.docs.yaml --dry-run            # preview
zbd deployments/production.docs.yaml --tag 0.1.0          # build linux/amd64, push to GHCR, deploy
zd  deployments/production.docs.yaml --tag 0.1.0          # redeploy a tag that's already pushed
URL=https://oxide-api.zaporion.com make smoke             # end-to-end checks against the server
./scripts/local.sh                                       # local run of the server compose on localhost:8080
```

- **Before the first deploy:** add a DNS record for `oxide-api.zaporion.com` pointing at the server,
  and run `docker login ghcr.io` locally with a token that can write packages. The server is already
  logged in to GHCR.
- **The API key** is read from your shell when `zd` runs, and `zd` stops if it's unset. To keep it in
  Doppler instead, add a `doppler:` block (see the comment in the yaml).
- **Building on a Mac** is fast even though the server is x86_64: every `RUN` stage executes on the
  build machine and the Rust stage cross-compiles, so nothing runs under emulation.
- **The OCR deadline is 90 s** on the server (instead of 120 s), because Cloudflare's proxy ends
  responses that take longer than 100 s. Slow scans come back as partial results.
- **On the server**, the container is `oxide-api` in `/opt/oxide-api`. The image has no shell; use
  `docker logs oxide-api`, and `docker exec oxide-api oxide-api healthcheck` for a probe.

## Development

No local Rust is needed: everything runs in containers.

```sh
make test         # unit + API + OCR tests; fetches ONNX Runtime + models (sha256-pinned) once
make lint         # clippy -D warnings, with and without the `ocr` feature
make image        # oxide-api:latest
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

Drop them once upstream ships fixes.

## Deviations from PLAN.md

| Plan | Built | Why |
|---|---|---|
| Two images (`:slim` < 20 MB, `:ocr` ≈ 100 MB) | **one image** with OCR | one container that does everything is simpler to run |
| PDF and Office input | adds **OpenDocument** and **images** | requested later; ODF goes through an own reader into `office_oxide`'s document model, images straight to OCR |
| ONNX Runtime 1.23.x | **1.28.3** | `ort` 2.0.0-rc.13 (pinned by `pdf_oxide`) enables `api-27` and refuses older runtimes |
| Stock `pdf_oxide` | two small patches | see above |
| OCR reads the page's largest image | same, **plus page rendering** for legacy-font pages, `ocr=force` and pages without a usable scan | Kruti Dev pages are vector text with no image to OCR |
| Image has no fonts | adds Liberation Sans/Serif/Mono (≈ 2.4 MB) | rendering a page whose Helvetica/Times/Courier isn't embedded otherwise produces a blank image |
| `page_font_face_lookups()` | reads each requested page's `/Resources /Font` | the upstream call walks every page of the document |
| Plain upstream recognition | adds word-gap segmentation (`hi`, `ar`), visual→logical `ि` reordering, PaddleOCR's Arabic run reversal | the v3 recognizers emit visual order and few spaces (measured above) |
| `cargo-chef` | BuildKit cache mounts | simpler with the vendored path dependency; same rebuild speed locally |
| k6 | `scripts/loadtest.py` (stdlib) | no extra tooling needed |

## Status

Done and verified locally on arm64: all of M0–M2; M3/M4 except publishing, plus OpenDocument and
image input. `make test` passes 45 unit, 20 API and 13 OCR tests, `make smoke` passes all 68 checks
against the container, and clippy passes with `-D warnings` with and without the `ocr` feature. Six
concurrent 48-megapixel JPEGs OCR correctly in under 3 s, with the container peaking at about
1.8 GiB of its 3 GiB limit (two images are decoded at a time, one per OCR worker). The cross-compiled `linux/amd64` image
also passes `make smoke` under emulation, run with `docker-compose.yml` behind the Caddy
site from `deployments/`.

Not done yet:

- The GitHub Actions workflow (`.github/workflows/ci.yml`) has never been run.
- Nothing has been pushed to GHCR or deployed to the server yet, and nothing is cosign-signed.
- Container limits (4 CPU / 3 GiB) are still the plan's starting values, because peaks were measured with small
  fixtures rather than the largest real documents.
- There's no real Kruti Dev font fixture (the fixture renames a Latin font, so it only exercises
  routing) and no JBIG2 scan fixture.
