# Performance and memory

Measured 2026-10-06 on an Apple-silicon Mac (Docker Desktop VM: 14 vCPU, 8 GiB) against the single
`oxide-api` container with the compose limits of 4 CPU / 3 GiB and 2 OCR workers × 2 ONNX Runtime
threads. The load generator is `scripts/loadtest.py` over localhost, and memory is the peak sampled
from `docker stats`. Documents are the fixtures in `tests/fixtures/`: small Office/PDF documents and
single-page A4 scans at 200 DPI.

Re-run:

```sh
source .env && export OXIDE_API_API_KEY
python3 scripts/loadtest.py --concurrency 8 --requests 800 --container oxide-api-oxide-api-1 \
  --files tests/fixtures/native.pdf tests/fixtures/sample.docx tests/fixtures/sample.xlsx tests/fixtures/sample.pptx
```

| Scenario | Result |
|---|---|
| Idle memory after startup (2 workers × 4 ONNX sessions loaded) | **224 MiB** |
| `/readyz` after start (model load + 3 golden tests) | ≈ 0.3 s |
| Native documents (PDF, DOCX, XLSX, PPTX), concurrency 8, 800 requests | p50 **1.9 ms**, p90 3.3 ms, p99 **7.6 ms**; 3,070 req/s; peak 262 MiB |
| One scanned A4 page, sequential, `en` | p50 **394 ms** (p99 419 ms) |
| One scanned A4 page, sequential, `hi` | p50 **270 ms** (p99 291 ms) |
| One scanned A4 page, sequential, `ar` | p50 **259 ms** (p99 290 ms) |
| Mixed scans + hybrid page, concurrency 8, 48 requests | p50 1.04 s, p99 **1.38 s**; 7.5 pages/s; peak **739 MiB** |
| 80 concurrent 31.9 MiB uploads | 67 × `429` after the 1 s wait, 13 admitted, 0 connection errors; peak **908 MiB** |
| Memory after the runs above (ONNX Runtime arenas warm) | 676 MiB |

Text-layer pages never reach inference, so native documents stay in the millisecond range even
while OCR is running. English is slower per page than Hindi or Arabic because its PP-OCRv5 recognizer
is larger and its lines are longer. The `hi` and `ar` timings include word segmentation, which
recognizes each word separately.

The upload burst shows the admission bound working. At most 8 bodies are buffered at once (8 × 32 MiB
= 256 MiB on top of the warm OCR sessions), and rejected clients receive their `429` because the
service drains their bodies without buffering them.

## Container limits

PLAN.md says to set the limit to measured peak + 25% once the largest real documents have been
measured. These numbers come from small fixtures, so compose keeps 4 CPU / 3 GiB. Before tightening,
re-run the scenarios with your largest real PDFs, scans and spreadsheets. Large scans (600 DPI) and
big XLSX files dominate memory.
