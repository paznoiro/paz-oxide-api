# Integrations

Oxide API is a plain HTTP service with a bearer token, so any HTTP client works. The patterns below
cover the two common low-code hosts.

## n8n

**Binary from a previous node (e.g. "Read Binary File", Gmail attachment, HTTP download):**

1. Add an **HTTP Request** node.
2. Method `POST`, URL `http://oxide-api:8080/v1/extract/markdown`.
3. Authentication: *Generic Credential Type → Header Auth*, with name `Authorization` and value
   `Bearer <OXIDE_API_API_KEY>`.
4. Send Body: on. Body Content Type: **Form-Data**. Add a parameter of type *n8n Binary File*
   named `file`, with input data field name `data` (or whatever your binary property is called).
5. Optional extra form fields: `pages`, `ocr`, `lang`.
6. The response JSON has `text` (the markdown), `char_count`, `truncated`, and for PDFs `ocr`.

Scanned PDFs need nothing extra: pages without a text layer are OCR'd in the same request, and
`ocr.status` tells you what happened (`native`, `ocr` or `hybrid`).

**Base64 instead of multipart.** Body Content Type: JSON, body
`{"file_base64": "{{$binary.data.data}}", "format": "pdf"}`.

**Retries.** Enable *Retry On Fail* on the node. A `429` carries `Retry-After` (10 s by default).

## Dify

Use an **HTTP Request** node in a workflow (or a custom tool built from `/openapi.json`):

1. Method `POST`, URL `http://oxide-api:8080/v1/extract/text`.
2. Authorization: *API-Key → Bearer*, key `<OXIDE_API_API_KEY>`.
3. Body: **form-data**. Key `file`, type *file*, value = the uploaded file variable
   (e.g. `sys.files` or a Start-node file input).
4. Parse the output with a **Code** node (`json.loads(body)["text"]`), or ask for raw text by adding
   the header `Accept: text/plain`. The body is then the text itself.

**Custom tool.** *Tools → Custom → Create*, then paste the schema from
`http://<host>:8080/openapi.json` and set the auth to Bearer. Dify lists `extractText`,
`extractMarkdown` and `documentInfo` as actions.

## Tips

- Prefer multipart or raw bodies for large files. JSON base64 inflates the upload by a third, and the
  32 MiB limit counts the encoded text.
- `pages=` lets you page through very large PDFs. When `incomplete: true` comes back, request the pages
  after the last entry of `pages_extracted`.
- Errors are RFC 9457 JSON (`application/problem+json`). The `type` slug (e.g.
  `/problems/encrypted-document`) is stable and safe to branch on.
