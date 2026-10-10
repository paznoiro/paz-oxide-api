# Oxide API — Document Extraction Service: Final Plan

> One lightweight, fast, low-memory HTTP API that extracts `text` and `markdown` from **PDF** and
> **Office** documents, with OCR for scanned PDFs in **English, Hindi, and Arabic**.
>
> Engines: [`office_oxide`](https://github.com/yfedoseev/office_oxide) (DOCX/XLSX/PPTX/DOC/XLS/PPT) and
> [`pdf_oxide`](https://github.com/yfedoseev/pdf_oxide) (PDF + OCR). Service patterns follow
> [`pdf_oxide_api`](https://github.com/yfedoseev/pdf_oxide_api). We build a new binary and a new image
> on the engine libraries; we don't reuse that image.
>
> Final · 2026-10-05 · engine facts verified against `pdf_oxide` 0.3.78 and `office_oxide` 0.1.13 source.
> Workspace: `/Users/dvpandian/git/general/oxide-api`.

---

## 1. Decisions

| Area | Decision |
|---|---|
| Images | `oxide-api:slim` (static musl on Chainguard `static`, **< 20 MB**) and `oxide-api:ocr` (glibc on `distroless/cc` + ONNX Runtime + models, **≈ 100 MB**). Both shell-less, nonroot |
| API surface | `POST /v1/extract/text`, `POST /v1/extract/markdown`, `POST /v1/info` + ops endpoints. The file type is detected from content; one route serves PDF and Office |
| Upload cap | **32 MiB** per request |
| Concurrency | **8** in-flight requests per container; excess waits ≤ 1 s, then `429` |
| OCR engine | ONNX Runtime via `pdf_oxide`'s `ocr` feature. One shared detector + `en`/`hi`/`ar` recognizers per OCR worker; 2 OCR workers per `:ocr` container |
| OCR language | `lang=auto` by default: legacy-Hindi-font check → native-text script → detector-shared confidence probe → `en` |
| Output | JSON by default; `Accept: text/plain` / `text/markdown` returns the raw text with metadata in headers |
| Auth | Bearer token required on `/v1/*`. The service refuses to start on a non-loopback bind without a key |
| Distribution | GHCR (private), multi-arch `amd64` + `arm64` built on native runners, cosign keyless signatures + SBOM |
| Starting resources | `:slim` 2 CPU / 1 GiB · `:ocr` 4 CPU / 3 GiB. Tightened to measured peak + 25% after M3/M4 |

---

## 2. Engine facts that shape the design

Verified in upstream source. Each one drives a concrete choice below.

**Office (`office_oxide`)**
- `Document::from_reader(R: Read + Seek + Send + 'static, DocumentFormat)` accepts `Cursor<Bytes>`: zero-copy, no disk.
- `pdf_oxide` already depends on `office_oxide`, so linking both adds almost nothing. Pin `office_oxide` to the version `pdf_oxide` resolves so `cargo tree -d` shows one copy.
- `sniff_format` is private and path-based, so we write our own byte sniffer (§5.3). The `zip` crate is already in the graph for ZIP entry inspection.
- Encrypted OOXML (CFB with `EncryptionInfo` + `EncryptedPackage`) is unsupported → `422`.
- `limits::set_max_text_chars` bounds output; parsing runs on a 16 MB-stack thread.

**PDF (`pdf_oxide`)**
- `PdfDocument::from_bytes(Vec<u8>)` works in memory. `PdfDocument` is `Send + Sync`, so it's shared as `Arc<PdfDocument>` between the CPU and OCR pools.
- Owner-password-only encrypted PDFs open automatically (empty user password). **A truly locked PDF returns empty text rather than an error.** Only `page_count()` returns `Error::EncryptedPdf`, so it's always called first.
- Default features (`icc`, `legacy-crypto`) stay on. `legacy-crypto` is required for R≤4 encryption, which covers most real-world encrypted PDFs.
- `page_font_face_lookups()` gives each page's font face names in one call, which is cheap and powers legacy-Hindi-font detection.

**OCR (`pdf_oxide` `ocr` feature)**
- OCR reads the page's **largest embedded image**; it never rasterizes the page, so **no fonts are needed**.
- The `rendering` feature is still required, because JBIG2-compressed scans don't decode without it. It's pure Rust.
- `extract_text_auto` rebuilds an OCR engine from disk on **every call**, so the service never uses it.
- ONNX Runtime sessions sit behind a `Mutex`, so each OCR worker owns its own sessions and there's no cross-worker contention.
- `TextDetector` and `TextRecognizer` are public and separately constructible. We compose **one detector with three recognizers** instead of three full `OcrEngine`s, which saves two detector sessions per worker (the detector dominates OCR memory).
- `OcrSpan.confidence` and `OcrOutput.total_confidence` are public, which enables the language probe.
- The recognizer appends PaddleOCR's space class itself, so dictionaries are used exactly as published.
- ONNX Runtime's thread default (4) ignores cgroups, so threads are set explicitly per worker.

---

## 3. Architecture

### 3.1 Images

| | `oxide-api:slim` | `oxide-api:ocr` |
|---|---|---|
| Cargo features | `pdf_oxide` defaults | + `pdf_oxide/ocr`, `pdf_oxide/rendering` |
| Builder | Rust stable on Alpine (musl), digest-pinned | Rust stable on Debian bookworm (glibc), digest-pinned |
| Runtime base | `cgr.dev/chainguard/static` (digest-pinned) | `gcr.io/distroless/cc-debian12:nonroot` (digest-pinned) |
| Extra layers | — | `libonnxruntime.so` 1.23.x (official release tarball, sha256-pinned) + ~30 MB models |
| Fonts | none | none |
| Size target | < 20 MB | ≈ 100 MB |
| Role | all native PDF + Office extraction | scanned / legacy-font PDFs in `en`, `hi`, `ar` |

Both images: UID 65532, read-only rootfs, no shell, no package manager, and `HEALTHCHECK CMD ["/usr/local/bin/oxide-api","healthcheck"]` (a binary subcommand). The service writes no files, so there's no tmpfs.

**Routing between images:** `:slim` still classifies pages. A PDF that needs OCR returns `200` with native text, `ocr.status: "unavailable"`, and `pages_needing_ocr`. Callers resend those documents to `:ocr`.

### 3.2 Request pipeline

```
accept ─► admission layer (/v1/*): 8 permits · wait ≤ 1 s · else 429 + Retry-After
   │       └ the body has NOT been read yet: a waiting request costs a socket, not 32 MiB
   ▼
read body ≤ 32 MiB (slow client > 30 s → 408) · base64 decoded, raw JSON dropped
   ▼
phase 1 · CpuPool (rayon, CPU_THREADS workers, 16 MiB stacks) · permit moved INTO the job
   │   sniff → open → PDF: page_count() → per page: classify + legacy-font check + native text
   │   deadline checked between pages
   ▼
phase 2 · :ocr only · pages needing OCR, submitted one at a time per request → OcrPool (2 workers)
   │   the worker decodes the page's scan image, detects, recognizes, merges
   │   requests interleave page by page, so all 8 make progress
   ▼
normalize (Arabic presentation forms) → serialize once (JSON or raw) → respond
permit released only when the job really ends
```

Rules:

- **Admission before the body is read.** That's what makes "8 × 32 MiB" a real memory bound.
- **The permit belongs to the job.** Rayon work can't be cancelled. A timed-out request keeps its permit until its job finishes, so work never piles up beyond 8.
- **16 MiB worker stacks.** A stack overflow aborts the process, and `catch_unwind` can't catch it. Every engine call is also wrapped in `catch_unwind` (`panic = "unwind"`).
- **Native work never blocks on OCR.** Phase 1 returns to the async handler, which awaits phase 2 on the separate pool.
- **One decoded scan per OCR worker.** A 300-DPI A4 page is ~26 MB as RGB. Decoding inside the worker bounds this to 2 images.
- **Deadlines are measured from admission.** Native: 30 s. Any request with OCR pages: 120 s. PDFs return what's done with `incomplete: true` and the list of extracted pages, so a caller can request the rest with `pages=`. An Office parse past its deadline → `422 processing-timeout`.
- **Watchdog:** a job running > 10 × its deadline flips `/healthz` to failing, and the orchestrator restarts the container.
- **The tokio runtime has 2 worker threads.** It only does I/O; cores belong to rayon and ONNX Runtime.

### 3.3 OCR subsystem

**Per-worker stack (built once at startup, never per request):**

```
OcrStack
  detector     TextDetector(det.onnx)          shared by all languages
  recognizers  { en, hi, ar }: TextRecognizer  one per language
  config       OcrConfig::default()            V4 detector, MaxSide 960
               num_threads = CPU_THREADS / OCR_WORKERS (min 1)
```

**Page OCR** reproduces upstream's `OcrEngine::ocr_image` from public parts:
1. Take the largest image on the page (`extract_images` → `to_dynamic_image`).
2. `detector.detect` → `crop_text_region` per box → `recognizer.recognize`, keeping results ≥ `rec_threshold`.
3. Build an `OcrOutput` and take `text_in_reading_order()`.

**Page routing** (`ocr=auto`):

| Page signal | Action |
|---|---|
| `classify_page` → native text, normal fonts | native text only |
| Legacy Hindi font face (Kruti Dev, Chanakya, DevLys, Walkman-Chanakya, Shusha, Agra, APS-DV) | OCR with `hi`; reason `legacy_font` |
| `classify_page` → scanned | OCR |
| `classify_page` → hybrid | native text + OCR lines not already contained in the native text (upstream's whitespace-normalized, case-insensitive merge rule, reimplemented) |

`ocr=force` OCRs every requested page. `ocr=off` skips classification entirely.

**Language resolution** (`lang=auto`, once per document, cached for all its pages):
1. Explicit `lang` param → used for recognition, always.
2. Legacy Hindi font present → `hi`.
3. The document has native text → Unicode-block count over up to 8,000 chars: Arabic (U+0600–06FF, U+0750–077F) or Devanagari (U+0900–097F) wins when count ≥ 4 and ≥ half the Latin count (upstream's heuristic).
4. Pure scan → **probe**: detect once on the first OCR page and recognize its 8 largest boxes with each recognizer. The highest mean confidence wins, provided its output is dominated by its own script (Latin for `en`). The detection is reused for that page's OCR, so the probe costs only 24 small recognitions.
5. Otherwise → `en`.

The response reports `lang` and `lang_source: param|font|text|probe|default`.

**Arabic normalization:** NFKC applied only to Arabic Presentation Forms (U+FB50–U+FDFF, U+FE70–U+FEFF), on both native and OCR output. It uses `unicode-normalization`, already in the dependency graph. Nothing else is rewritten.

**Models** are baked into the image in upstream's flat layout, at pinned Hugging Face revisions, sha256-verified at build time:

| `lang` | Files | Source |
|---|---|---|
| shared | `det.onnx` | `deepghs/paddleocr` · `ch_PP-OCRv4_det` (~4.7 MB) |
| `en` | `rec.onnx`, `en_dict.txt` | `monkt/paddleocr-onnx` · PP-OCRv5 English (~7.8 MB) |
| `hi` | `rec_devanagari.onnx`, `devanagari_dict.txt` | `deepghs/paddleocr` · `devanagari_PP-OCRv3_rec` + PaddleOCR dict |
| `ar` | `rec_arabic.onnx`, `arabic_dict.txt` | `deepghs/paddleocr` · `arabic_PP-OCRv3_rec` + PaddleOCR dict |

These are the exact packs `pdf_oxide`'s `OcrLanguage::spec()` provisions and tests against. File names are read from `OcrLanguage::from_code(..).spec()` so loader and fetch script can't drift.

**Startup:** build both stacks, then run a **golden smoke test per language**. A tiny embedded image must recognize to its known string, which catches a missing ONNX Runtime, a corrupt model, or a dict/model mismatch. `/readyz` turns green only after this. A failed language is logged by name and reported as `unavailable` in `/version`; requests that need it get native text with `ocr.status: "unavailable"`. If ONNX Runtime or the detector is missing, all OCR is `unavailable` and native extraction still works.

### 3.4 Memory and CPU budget

| | `:slim` | `:ocr` |
|---|---|---|
| Idle RSS (target) | ~10–20 MB | baseline + 2 OCR stacks (measured in M4) |
| Upload buffers, worst case | 8 × 32 MiB = 256 MiB | same |
| Output, worst case | 8 × (4 Mi chars × ≤ 4 B + one JSON copy) | same |
| OCR | — | 2 stacks + 2 decoded scans |
| Starting container limit | **1 GiB**, 2 CPU | **3 GiB**, 4 CPU |

M3/M4 measure peaks with the largest real documents at concurrency 8, then set each limit to measured peak + 25%. Scale-out is horizontal: both images are stateless.

---

## 4. API

### 4.1 Endpoints

| Endpoint | Office | PDF |
|---|---|---|
| `POST /v1/extract/text` | `Document::plain_text()` | per-page native text + OCR routing |
| `POST /v1/extract/markdown` | `Document::to_markdown_with(MarkdownOptions::default())` | per-page `to_markdown` with `detect_headings` |
| `POST /v1/info` | format, metadata, structure (sheet names + dimensions, slide count) from `to_ir()` | metadata, `page_count`, `encrypted`, per-page geometry |
| `GET /healthz` `/readyz` `/version` `/metrics` `/openapi.json` `/openapi.yaml` `/docs` | ops, unauthenticated | |

### 4.2 Request and response

```
POST /v1/extract/{text|markdown}
  input   : multipart (file=<binary>) | JSON {"file_base64": "<b64>"} | raw body
  params  : top-level multipart parts / JSON keys / query params, never nested
    any     format?=pdf|docx|xlsx|pptx|doc|xls|ppt       hint; the content sniff wins
    pdf     pages?=1-3,5                                  1-based
            ocr?=auto|force|off                           default auto
            lang?=auto|en|hi|ar                           default auto; unknown → 400
            detect_headings?=bool                         markdown only, default true
    Params that don't apply to the detected kind are ignored and listed in `ignored_params`.
  Accept  : application/json (default) | text/plain (text) | text/markdown (markdown)

→ 200 application/json — PDF
  { "kind": "pdf", "format": "pdf", "format_source": "sniff",
    "text": "...", "char_count": 18234, "truncated": false,
    "page_count": 12, "pages_extracted": [1, 2, 3], "incomplete": false,
    "ocr": { "status": "native|ocr|hybrid|unavailable|off",
             "lang": "hi", "lang_source": "probe",
             "pages": [2, 3], "pages_needing_ocr": [], "failed_pages": [] } }

→ 200 application/json — Office
  { "kind": "office", "format": "docx", "format_source": "sniff",
    "text": "...", "char_count": 1234, "truncated": false }

→ 200 text/plain | text/markdown — raw output, metadata in headers:
  X-Oxide-Kind, X-Oxide-Format, X-Oxide-Truncated, X-Oxide-Incomplete, X-Oxide-Ocr-Status,
  X-Oxide-Lang, X-Oxide-Page-Count

Every data response:
  X-Engine: pdf_oxide/<ver> | office_oxide/<ver>
  Server-Timing: queue;dur=…, parse;dur=…, extract;dur=…, ocr;dur=…
  Cache-Control: no-store

POST /v1/info
  → { "kind": "office", "format": "xlsx", "metadata": {…},
      "sheets": [ { "name": "Q1", "rows": 120, "cols": 8 } ] }
  → { "kind": "pdf", "page_count": 12, "encrypted": false, "metadata": {…},
      "pages": [ { "page": 1, "width": 612.0, "height": 792.0, "rotation": 0 } ] }
```

### 4.3 Format detection (content first)

1. **Magic bytes pick the family:** `%PDF-` within the first 1 KiB → PDF; `50 4B 03 04` → OOXML; `D0 CF 11 E0 A1 B1 1A E1` → CFB.
2. **Inside the family:**
   - ZIP entries: `word/` → docx, `xl/` → xlsx, `ppt/` → pptx.
   - CFB streams: `WordDocument` → doc, `Workbook`/`Book` → xls, `PowerPoint Document` → ppt.
   - `EncryptionInfo` + `EncryptedPackage` → encrypted OOXML → `422`.
3. **Only if the content is inconclusive:** use the `format` param, then the multipart filename extension.

`format` and `format_source` (`sniff|param|filename`) are always echoed.

### 4.4 Errors (RFC 9457 `application/problem+json`)

| Condition | Status | Notes |
|---|---|---|
| Malformed input, bad params, unknown `lang` | `400` | Names the accepted values |
| Missing or wrong bearer token | `401` | Constant-time compare |
| Upload slower than 30 s | `408` | Only this case |
| Body over 32 MiB | `413` | Via axum `DefaultBodyLimit`, works without `Content-Length` |
| No admission permit within 1 s | `429` | `Retry-After` (1 s `:slim`, 10 s `:ocr`) |
| Unsupported, corrupt, encrypted OOXML, locked PDF | `422` | Friendly upstream detail |
| Office parse or a single PDF page exceeds the deadline | `422` | `type: …/processing-timeout` |
| PDF deadline reached between pages | `200` | `incomplete: true`, `pages_extracted` lists what's done |
| OCR not available | `200` | Native text, `ocr.status: "unavailable"` |
| OCR error on one page | `200` | That page uses native text and is listed in `failed_pages` |
| `ocr=force` and every page's OCR failed | `422` | — |
| Draining for shutdown | `503` | `/readyz` flips first |

Request bytes, filenames, and extracted text are **never** logged.

---

## 5. Configuration

| Variable | Default | Meaning |
|---|---|---|
| `OXIDE_API_ADDR` | `0.0.0.0:8080` | Bind address |
| `OXIDE_API_API_KEY` | *(required)* | Bearer token for `/v1/*`. Startup fails without it on a non-loopback bind |
| `OXIDE_API_CPU_THREADS` | `available_parallelism()` (cgroup-aware) | Rayon pool size |
| `OXIDE_API_MAX_INFLIGHT` | `8` | Admission permits; max request bodies in memory |
| `OXIDE_API_QUEUE_WAIT_MS` | `1000` | Max wait for a permit before `429` |
| `OXIDE_API_RETRY_AFTER_SECS` | `1` (`:slim`), `10` (`:ocr`) | `Retry-After` on `429` |
| `OXIDE_API_MAX_BODY_BYTES` | `33554432` (32 MiB) | Upload ceiling |
| `OXIDE_API_BODY_TIMEOUT_SECS` | `30` | Slow upload → `408` |
| `OXIDE_API_PROCESS_TIMEOUT_SECS` | `30` | Native extraction deadline from admission |
| `OXIDE_API_MAX_TEXT_CHARS` | `4194304` (4 Mi chars) | Per-document output budget (≈ 1,300 dense pages) |
| `OXIDE_API_MAX_PDF_PAGES` | `2000` | PDF-bomb guard |
| `OXIDE_API_OCR_WORKERS` | `2` | OCR workers, each with its own detector + 3 recognizers |
| `OXIDE_API_OCR_TIMEOUT_SECS` | `120` | Deadline for requests that OCR any page |
| `OXIDE_API_OCR_LANGUAGES` | `en,hi,ar` | Recognizers to load and smoke-test |
| `OXIDE_API_OCR_MODEL_DIR` | `/models` | Flat upstream model layout |
| `ORT_DYLIB_PATH` | `/usr/lib/libonnxruntime.so` | Read by the `ort` crate (set in the `:ocr` image) |
| `RUST_LOG` | `info` | Structured JSON logs; never content |

OCR variables are ignored by `:slim`, which has no OCR compiled in.

---

## 6. Code layout

```
src/
  main.rs        # tokio (2 workers), build_router(), ops handlers, security headers,
                 #   graceful drain, `healthcheck` subcommand
  config.rs      # OXIDE_API_* parsing, defaults, fail-closed auth check
  auth.rs        # bearer on /v1/*, constant-time compare
  admission.rs   # tower layer: semaphore permit acquired before the body is read
  blocking.rs    # CpuPool (rayon, 16 MiB stacks), catch_unwind, permit-in-job, watchdog
  detect.rs      # magic bytes → ZIP entries / CFB streams → DocKind + format_source
  io.rs          # multipart / JSON base64 / raw body → Bytes + params
  respond.rs     # JSON vs raw (Accept), X-Oxide-*, X-Engine, Server-Timing
  error.rs       # ApiError → RFC 9457; office_oxide + pdf_oxide error mapping
  text.rs        # Arabic presentation-form normalization, char budget, truncation flag
  handlers/
    extract.rs   # /v1/extract/{text,markdown}: phase 1 + phase 2 orchestration
    info.rs      # /v1/info
  pdf/
    route.rs     # classify_page + legacy-font check (page_font_face_lookups) → page plan
    lang.rs      # lang=auto: font → text script → probe → en
  ocr/           # #[cfg(feature = "ocr")]
    stack.rs     # OcrStack: 1 TextDetector + {en,hi,ar} TextRecognizer
    pool.rs      # OcrPool: OCR_WORKERS threads, each owning one OcrStack
    page.rs      # largest image → detect → crop → recognize → reading order; hybrid merge
    smoke.rs     # golden per-language startup test (fixtures via include_bytes!)
  metrics.rs     # RED + queue wait, in-flight, 429s, per kind/format, OCR pages by lang,
                 #   OCR page latency, lang_source, engine builds
openapi.yaml / openapi.json   # hand-authored contract, served via include_str!, contract-tested
Dockerfile                    # stages: chef-musl, chef-glibc, models, runtime-slim, runtime-ocr
docker-compose.yml            # oxide-api (slim) + oxide-api-ocr (profile "ocr")
scripts/fetch_ocr_models.sh   # det + en/hi/ar rec + dicts, pinned revisions + sha256
tests/fixtures/               # see §9
```

---

## 7. Build and supply chain

**Cargo**

```toml
[dependencies]
pdf_oxide    = "0.3.78"
office_oxide = "=0.1.13"          # the version pdf_oxide resolves; one copy in the graph
axum         = { version = "0.8", features = ["multipart"] }
tokio        = { version = "1", features = ["rt-multi-thread", "macros", "signal", "sync"] }
tower-http   = { version = "0.6", features = ["trace", "timeout", "request-id", "sensitive-headers"] }
rayon        = "1"
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
base64       = "0.22"
unicode-normalization = "0.1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
metrics      = "0.24"
metrics-exporter-prometheus = { version = "0.18", default-features = false }
mimalloc     = { version = "0.1", default-features = false }   # global allocator; musl's is slow under threads

[features]
default = ["docs"]
docs = []                                               # /docs (Scalar from CDN; no bytes in the image)
ocr  = ["pdf_oxide/ocr", "pdf_oxide/rendering"]

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = "symbols"
panic = "unwind"        # LOAD-BEARING: catch_unwind in the worker pools
```

- MSRV **1.88** (both engines). CI checks MSRV; images build with current stable.
- No UPX: packed binaries decompress into anonymous memory on every start.
- The builder and runtime images are pinned by digest. Renovate bumps the digests.

**Dockerfile stages**
- `chef-musl` → `runtime-slim`: `cargo-chef` cook, then `--target <arch>-unknown-linux-musl`. Copy only the binary into Chainguard `static`.
- `chef-glibc` → `runtime-ocr`: build with `--features ocr`. Copy the binary, `libonnxruntime.so*` (from the official `onnxruntime-linux-{x64,aarch64}-1.23.x.tgz`, sha256-checked), and `/models` into `distroless/cc-debian12:nonroot`. Set `ORT_DYLIB_PATH`.
- `models`: runs `fetch_ocr_models.sh` (curl + sha256sum) on a throwaway stage. Only `/models` is copied out.
- OCI labels: source, license, `pdf_oxide` version, `office_oxide` version, ONNX Runtime version, model revisions.

**CI and release**
- Native `amd64` and `arm64` runners (no QEMU), merged into one multi-arch manifest.
- Push to GHCR (private): `ghcr.io/<owner>/oxide-api:{0.1.0-slim, slim, 0.1.0-ocr, ocr}`.
- Cosign keyless signature + SBOM attestation for every tag.
- Gates: `cargo clippy -D warnings`, tests, OpenAPI contract tests, image-size budget check (`:slim` < 20 MB).

**Compose:** `oxide-api` (default) and `oxide-api-ocr` (profile `ocr`), each with:
- `read_only: true`, `cap_drop: [ALL]`, `security_opt: [no-new-privileges:true]`
- `pids_limit: 256`, `restart: unless-stopped`
- `cpus` and `mem_limit` from §3.4
- `OXIDE_API_API_KEY` from `.env`

---

## 8. Milestones

| | Scope | Est. |
|---|---|---|
| **M0** scaffold | Cargo project at MSRV 1.88; one `office_oxide` in `cargo tree -d`; smoke-test all 7 formats from in-memory bytes; confirm `Bytes` → `Vec<u8>` handoff into `PdfDocument::from_bytes` reuses the allocation (otherwise count one extra body copy in §3.4); Dockerfile skeleton + `healthcheck` subcommand | 0.5 d |
| **M1** core API | `/v1/extract/{text,markdown}`, `/v1/info`, 3 input encodings, JSON + raw output, `detect.rs`, error mapping, ops endpoints, OpenAPI + contract tests, page routing incl. legacy-font detection and `pages_needing_ocr` | 1.5 d |
| **M2** hardening | admission layer, permit-in-job, 16 MiB stacks, deadlines + partial results, watchdog, bearer auth fail-closed, security headers, graceful drain, `/docs`, README + `docs/` (quickstart, n8n/Dify) | 1 d |
| **M3** slim release | image build + size gate; fuzz loop; k6 at concurrency 8 and 80; measure idle/peak RSS and p99; set `:slim` limits; publish signed `0.1.0-slim` | 1 d |
| **M4** OCR release | ONNX Runtime + models stages, `OcrStack`/`OcrPool`, golden smoke, `lang=auto` incl. probe, hybrid merge, Arabic normalization; per-language page latency + RSS at concurrency 8; set `:ocr` limits; publish signed `0.1.0-ocr` | 3 d |

---

## 9. Acceptance criteria

**Fixtures:** all 6 Office formats; native PDF; scanned PDF per language (`en`, `hi`, `ar`); JBIG2-compressed scan; hybrid PDF; Kruti Dev Hindi PDF; Arabic native PDF with presentation forms; owner-password-only PDF; locked PDF; encrypted OOXML; wrong-extension and filename-less uploads; corrupt/truncated set; deeply nested PDF.

**`:slim`**
- [ ] All 6 Office formats + PDF extract text and markdown through all 3 encodings, in both JSON and raw output.
- [ ] Wrong-extension and filename-less uploads resolve correctly with `format_source: "sniff"`.
- [ ] Encrypted OOXML → `422`. Locked PDF → `422` (never an empty `200`). Owner-password-only PDF → `200` with text.
- [ ] 100-file corrupt/truncated fuzz loop + deeply nested PDF: zero 5xx, process stays up.
- [ ] Image < 20 MB; `/readyz` green < 1 s after start; p99 < 50 ms on small documents at concurrency 8 (excluding upload).
- [ ] 80 concurrent 32 MiB uploads: at most 8 bodies buffered at any moment, the rest get `429` after 1 s, RSS stays under the limit, no OOM.
- [ ] Scanned and Kruti Dev PDFs → `200` with `ocr.status: "unavailable"` and correct `pages_needing_ocr`.
- [ ] Missing API key on a `0.0.0.0` bind → refuses to start.
- [ ] All ops endpoints live; `HEALTHCHECK` passes in the shell-less image; README curl examples verified.

**`:ocr`**
- [ ] Scanned `en`, `hi`, `ar` fixtures return materially more text with `ocr=auto` than `ocr=off`, verified by eye.
- [ ] `lang=auto` picks the right language for the pure-scan `hi` and `ar` fixtures via the probe (`lang_source: "probe"`).
- [ ] The Kruti Dev PDF returns Unicode Devanagari under `ocr=auto` (`reason: legacy_font`, no `force` needed).
- [ ] JBIG2 scan OCRs. Hybrid page text isn't duplicated.
- [ ] Arabic reading order is correct; presentation forms come out as base letters.
- [ ] Text-layer pages are never sent through inference (`ocr.status: "native"` + metrics).
- [ ] Engine builds happen only at startup (`OCR_WORKERS × 4` sessions), never per request.
- [ ] Golden smoke runs per language at startup; a deliberately broken dict marks that language `unavailable` by name.
- [ ] 8 concurrent multi-page scanned requests all make progress; none runs past 120 s without returning partial results.
- [ ] Missing models or ONNX Runtime → native text + `ocr.status: "unavailable"`, process healthy.
- [ ] Image ≈ 100 MB; per-language per-page latency and peak RSS are published in `docs/`.

---

## 10. Out of scope for v0.1

HTML and IR output; `embed_images`; legacy → OOXML conversion; Office editing; PDF forms, merge, split, and pipelines; response compression; CORS (this is a server-to-server API); OCR languages beyond `en`/`hi`/`ar`; GPU inference; async job queue; persistence; UI; auth beyond a bearer token.

---

## 11. References

- `pdf_oxide/Cargo.toml`: features `ocr`, `rendering`, `legacy-crypto`; unconditional `office_oxide` dependency.
- `pdf_oxide/src/document.rs`: `from_bytes`, `page_count`, `is_encrypted_unreadable`, `classify_page`, `page_font_face_lookups`, `Send + Sync` assertion.
- `pdf_oxide/src/ocr/{engine,detector,recognizer,preprocessor,backend,mod}.rs`: `OcrEngine::ocr_image`, `TextDetector`, `TextRecognizer`, `crop_text_region`, `OcrOutput`/`OcrSpan`, `OrtBackend` mutex, `ocr_page`, hybrid merge rule, dictionary space handling.
- `pdf_oxide/src/extractors/auto.rs`: `OcrLanguage::{from_code, spec}`, the language heuristic, per-call engine loading in `extract_text_auto` (avoided).
- `pdf_oxide/src/extractors/images.rs`: JBIG2 decode requires `rendering`.
- `pdf_oxide/docs/OCR_GUIDE.md`: ONNX Runtime setup, `ORT_DYLIB_PATH`, detector/recognizer version guidance.
- `office_oxide/src/lib.rs` (`Document::from_reader`), `src/ir.rs`, `src/limits.rs`.
- `pdf_oxide_api/Cargo.toml` and `Dockerfile`: dependency set, release profile, per-arch musl build, Chainguard runtime.
