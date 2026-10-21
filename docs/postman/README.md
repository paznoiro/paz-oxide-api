# Postman

Import both files into Postman:

- `oxide-api.postman_collection.json`
- `oxide-api.postman_environment.json`

Then in the environment set:

- `baseUrl` — e.g. `http://localhost:8080` or `https://oxide-api.zaporion.com`
- `apiKey` — the value of `OXIDE_API_API_KEY` (see `.env`)

All `POST /v1/*` requests send `Authorization: Bearer {{apiKey}}`
(inherited from the collection). Ops requests (`GET /healthz`,
`/readyz`, `/version`, `/metrics`, `/openapi.json`, `/openapi.yaml`)
use `noauth`.

## Folders

| Folder | Requests |
|---|---|
| `extract` | every endpoint (`text`, `markdown`, `fields`, `info`) in each input style: multipart, JSON base64, raw body with `Accept: text/plain` / `text/markdown`. Any supported file works: the type is detected from the content |
| `documents (structured)` | digital, scanned and mixed PDF pages to structured JSON; fixture requests run as-is |
| `images (OCR)` | an image to text, a sideways phone photo (EXIF rotation), page 2 of a multi-page TIFF, raw markdown from an Arabic WebP, image info |
| `opendocument` | ODT / ODS / ODP to markdown, spreadsheet info |
| `ops` | health, readiness, version, metrics, OpenAPI |

Supported files: PDF; DOCX, XLSX, PPTX, DOC, XLS, PPT; ODT, ODS, ODP;
PNG, JPEG, TIFF, WebP, GIF, BMP, JPEG 2000, PNM, TGA. Images go straight
to OCR. HEIC/HEIF, AVIF and JPEG XL come back as a `422` that names the
format; convert them to JPEG or PNG first.

## Files

For multipart requests, pick a file for the `file` key; for binary
requests (Body → Binary), pick the file there. The `extract` requests
start empty.

The `documents (structured)`, `images (OCR)` and `opendocument` requests point at the repo's test
fixtures (`tests/fixtures/scan_en.png`, `sample.ods`, ...). Set Postman's
working directory to the repo root (Settings → General → Working
directory) and they run as-is.

From the command line, with the service running:

```sh
docker run --rm --add-host=host.docker.internal:host-gateway -v "$PWD:/etc/newman" -w /etc/newman \
  postman/newman run docs/postman/oxide-api.postman_collection.json \
  --working-dir /etc/newman --folder "images (OCR)" --folder opendocument --folder ops \
  --env-var baseUrl=http://host.docker.internal:8080 --env-var apiKey="$OXIDE_API_API_KEY"
```
