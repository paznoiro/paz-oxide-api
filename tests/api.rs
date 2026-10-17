//! In-process API tests against the real router and engines.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use http_body_util::BodyExt;
use oxide_api::config::Config;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: &str = "test-key-0123456789";

fn config(overrides: &[(&str, &str)]) -> Config {
    let mut m: HashMap<String, String> = HashMap::from([
        ("OXIDE_API_API_KEY".into(), KEY.into()),
        ("OXIDE_API_CPU_THREADS".into(), "2".into()),
        ("OXIDE_API_OCR_WORKERS".into(), "1".into()),
    ]);
    for (k, v) in overrides {
        m.insert(k.to_string(), v.to_string());
    }
    Config::from_lookup(|k| m.get(k).cloned()).unwrap()
}

fn app_with(overrides: &[(&str, &str)]) -> (Router, Arc<oxide_api::AppState>) {
    let st = oxide_api::build_state(config(overrides));
    // `:ocr` turns ready only after model loading and the golden tests.
    let start = std::time::Instant::now();
    while !st.health.is_ready() && start.elapsed() < std::time::Duration::from_secs(120) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    (oxide_api::build_router(st.clone()), st)
}

fn app() -> Router {
    app_with(&[]).0
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(app: &Router, req: Request<Body>) -> Resp {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    Resp { status, headers, body }
}

fn authed(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri).header(header::AUTHORIZATION, format!("Bearer {KEY}"))
}

fn raw_req(uri: &str, bytes: Vec<u8>) -> Request<Body> {
    authed("POST", uri).header(header::CONTENT_TYPE, "application/octet-stream").body(Body::from(bytes)).unwrap()
}

fn multipart_req(uri: &str, bytes: &[u8], filename: Option<&str>, fields: &[(&str, &str)]) -> Request<Body> {
    let boundary = "oxideBoundary7MA4YWxkTrZu0gW";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    let fname = filename.map(|f| format!("; filename=\"{f}\"")).unwrap_or_default();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"{fname}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    authed("POST", uri)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(body))
        .unwrap()
}

fn json_req(uri: &str, bytes: &[u8], extra: Value) -> Request<Body> {
    let mut v = json!({ "file_base64": base64::engine::general_purpose::STANDARD.encode(bytes) });
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    authed("POST", uri).header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap()
}

// --------------------------------------------------------------------------
// Extraction across formats, encodings and output modes
// --------------------------------------------------------------------------

const OFFICE: [(&str, &str, &str); 9] = [
    ("sample.docx", "docx", "docx-alpha"),
    ("sample.xlsx", "xlsx", "xlsx-alpha"),
    ("sample.pptx", "pptx", "pptx-alpha"),
    ("sample.doc", "doc", "docx-alpha"),
    ("sample.xls", "xls", "xlsx-alpha"),
    ("sample.ppt", "ppt", "pptx-alpha"),
    // OpenDocument, saved by LibreOffice from the OOXML samples.
    ("sample.odt", "odt", "docx-alpha"),
    ("sample.ods", "ods", "xlsx-alpha"),
    ("sample.odp", "odp", "pptx-alpha"),
];

#[tokio::test]
async fn office_formats_all_encodings_and_outputs() {
    let app = app();
    for (file, format, marker) in OFFICE {
        let bytes = fixture(file);
        let engine = if format.starts_with("od") {
            format!("oxide-api/{}", env!("CARGO_PKG_VERSION"))
        } else {
            format!("office_oxide/{}", office_oxide::VERSION)
        };
        for endpoint in ["text", "markdown"] {
            let uri = format!("/v1/extract/{endpoint}");
            let reqs = [
                ("multipart", multipart_req(&uri, &bytes, Some(file), &[])),
                ("json", json_req(&uri, &bytes, json!({}))),
                ("raw", raw_req(&uri, bytes.clone())),
            ];
            for (enc, req) in reqs {
                let r = send(&app, req).await;
                assert_eq!(r.status, StatusCode::OK, "{file} {endpoint} {enc}: {}", r.text());
                let j = r.json();
                assert_eq!(j["kind"], "office");
                assert_eq!(j["format"], format, "{file}");
                assert_eq!(j["format_source"], "sniff");
                assert!(j["text"].as_str().unwrap().contains(marker), "{file} {endpoint} {enc}: {}", j["text"]);
                assert_eq!(j["char_count"].as_u64().unwrap() as usize, j["text"].as_str().unwrap().chars().count());
                assert_eq!(r.header("x-engine").unwrap(), engine, "{file}");
                assert!(r.header("server-timing").unwrap().contains("queue;dur="));
                assert_eq!(r.header("cache-control"), Some("no-store"));
            }
            // Raw output: text body + metadata headers.
            let accept = if endpoint == "text" { "text/plain" } else { "text/markdown" };
            let mut req = raw_req(&uri, bytes.clone());
            req.headers_mut().insert(header::ACCEPT, accept.parse().unwrap());
            let r = send(&app, req).await;
            assert_eq!(r.status, StatusCode::OK);
            assert!(r.header("content-type").unwrap().starts_with(accept));
            assert_eq!(r.header("x-oxide-kind"), Some("office"));
            assert_eq!(r.header("x-oxide-format"), Some(format));
            assert!(r.text().contains(marker), "{file} raw {endpoint}");
        }
    }
}

#[tokio::test]
async fn native_pdf_text_markdown_and_pages() {
    let app = app();
    let pdf = fixture("native.pdf");
    let r = send(&app, multipart_req("/v1/extract/text", &pdf, Some("native.pdf"), &[])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["kind"], "pdf");
    assert_eq!(j["page_count"], 3);
    assert_eq!(j["pages_extracted"], json!([1, 2, 3]));
    assert_eq!(j["incomplete"], false);
    assert_eq!(j["truncated"], false);
    assert_eq!(j["ocr"]["status"], "native");
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([]));
    for m in ["page-one-alpha", "page-two-beta", "page-three-gamma"] {
        assert!(j["text"].as_str().unwrap().contains(m), "{m}");
    }
    assert!(r.header("x-engine").unwrap().starts_with("pdf_oxide/"));

    // pages= selection through each param channel.
    let r = send(&app, json_req("/v1/extract/text", &pdf, json!({"pages": "2"}))).await;
    let j = r.json();
    assert_eq!(j["pages_extracted"], json!([2]));
    assert!(j["text"].as_str().unwrap().contains("page-two-beta"));
    assert!(!j["text"].as_str().unwrap().contains("page-one-alpha"));
    let r = send(&app, multipart_req("/v1/extract/text", &pdf, None, &[("pages", "1,3")])).await;
    assert_eq!(r.json()["pages_extracted"], json!([1, 3]));
    let r = send(&app, raw_req("/v1/extract/text?pages=1-2", pdf.clone())).await;
    assert_eq!(r.json()["pages_extracted"], json!([1, 2]));

    // Markdown detects the heading.
    let r = send(&app, raw_req("/v1/extract/markdown", pdf.clone())).await;
    let md = r.json()["text"].as_str().unwrap().to_string();
    assert!(md.contains("# ") && md.contains("Chapter 1"), "{md}");

    // Raw text output with metadata headers.
    let mut req = raw_req("/v1/extract/text", pdf.clone());
    req.headers_mut().insert(header::ACCEPT, "text/plain".parse().unwrap());
    let r = send(&app, req).await;
    assert!(r.text().contains("page-one-alpha"));
    assert_eq!(r.header("x-oxide-kind"), Some("pdf"));
    assert_eq!(r.header("x-oxide-page-count"), Some("3"));
    assert_eq!(r.header("x-oxide-ocr-status"), Some("native"));
    assert_eq!(r.header("x-oxide-incomplete"), Some("false"));
    assert_eq!(r.header("x-oxide-truncated"), Some("false"));

    // Out-of-range page → 400.
    let r = send(&app, raw_req("/v1/extract/text?pages=9", pdf)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ignored_params_are_listed() {
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text?pages=1&ocr=off&lang=hi&detect_headings=false&foo=bar", fixture("sample.docx"))).await;
    let j = r.json();
    assert_eq!(j["ignored_params"], json!(["detect_headings", "foo", "lang", "ocr", "pages"]));
    let r = send(&app, raw_req("/v1/extract/text?detect_headings=false", fixture("native.pdf"))).await;
    assert_eq!(r.json()["ignored_params"], json!(["detect_headings"]));
    let r = send(&app, raw_req("/v1/extract/markdown?detect_headings=false", fixture("native.pdf"))).await;
    assert!(r.json().get("ignored_params").is_none());
    // Images take the paged params, but have no headings to detect.
    let r = send(&app, raw_req("/v1/extract/markdown?ocr=off&pages=1&lang=en&detect_headings=false", fixture("scan_en.png"))).await;
    assert_eq!(r.json()["ignored_params"], json!(["detect_headings"]));
}

#[tokio::test]
async fn clean_param_collapses_whitespace() {
    let app = app();
    let pdf = fixture("native.pdf");
    // `clean` applies to every extraction, so it is never ignored there.
    let r = send(&app, raw_req("/v1/extract/text?clean=true", pdf.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert!(r.json().get("ignored_params").is_none());
    let r_office = send(&app, raw_req("/v1/extract/text?clean=true", fixture("sample.docx"))).await;
    assert!(r_office.json().get("ignored_params").is_none());
    // Already-clean output is unchanged by `clean`.
    let plain = send(&app, raw_req("/v1/extract/text", pdf.clone())).await.json();
    let cleaned = send(&app, raw_req("/v1/extract/text?clean=true", pdf.clone())).await.json();
    assert_eq!(plain["text"], cleaned["text"]);
    // Markdown and the JSON body channel accept it too.
    let r = send(&app, raw_req("/v1/extract/markdown?clean=true", pdf.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let r = send(&app, json_req("/v1/extract/text", &pdf, json!({"clean": true}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    // ... but `/v1/info` has no text to clean, so it is ignored there.
    let r = send(&app, raw_req("/v1/info?clean=true", pdf.clone())).await;
    assert_eq!(r.json()["ignored_params"], json!(["clean"]));
}

#[tokio::test]
async fn opendocument_structure() {
    let app = app();
    let r = send(&app, raw_req("/v1/extract/markdown", fixture("sample.odt"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let md = r.json()["text"].as_str().unwrap().to_string();
    assert!(md.contains("# Quarterly Report"), "{md}");
    assert!(md.contains("| Region | Revenue |") && md.contains("| North | 1200 |"), "{md}");

    let r = send(&app, raw_req("/v1/extract/markdown", fixture("sample.ods"))).await;
    let md = r.json()["text"].as_str().unwrap().to_string();
    assert!(md.contains("## Q1") && md.contains("## Q2") && md.contains("| North | 1200 | xlsx-alpha |"), "{md}");

    let r = send(&app, raw_req("/v1/extract/markdown", fixture("sample.odp"))).await;
    let md = r.json()["text"].as_str().unwrap().to_string();
    assert!(md.contains("## Roadmap 2026") && md.contains("## Next Steps"), "{md}");
    assert_eq!(md.matches("Roadmap 2026").count(), 1, "{md}");
}

#[tokio::test]
async fn sniffing_beats_wrong_extension_and_missing_filename() {
    let app = app();
    let r = send(&app, multipart_req("/v1/extract/text", &fixture("sample.xlsx"), Some("report.pdf"), &[])).await;
    let j = r.json();
    assert_eq!((j["format"].as_str(), j["format_source"].as_str()), (Some("xlsx"), Some("sniff")));
    let r = send(&app, multipart_req("/v1/extract/text", &fixture("sample.doc"), None, &[("format", "xls")])).await;
    let j = r.json();
    assert_eq!((j["format"].as_str(), j["format_source"].as_str()), (Some("doc"), Some("sniff")));
    let r = send(&app, raw_req("/v1/extract/text", fixture("native.pdf"))).await;
    assert_eq!(r.json()["format_source"], "sniff");
    let r = send(&app, multipart_req("/v1/extract/text?ocr=off", &fixture("scan_en.png"), Some("scan.pdf"), &[])).await;
    let j = r.json();
    assert_eq!((j["kind"].as_str(), j["format"].as_str(), j["format_source"].as_str()), (Some("image"), Some("png"), Some("sniff")));
    let r = send(&app, multipart_req("/v1/extract/text", &fixture("sample.ods"), Some("sheet.xlsx"), &[])).await;
    assert_eq!(r.json()["format"], "ods");
    // HEIC is recognized, and named in the error.
    let r = send(&app, raw_req("/v1/extract/text", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec())).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json()["detail"].as_str().unwrap().contains("HEIC"), "{}", r.text());
    // Unknown content with no hints → 422.
    let r = send(&app, raw_req("/v1/extract/text", b"just some text".to_vec())).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.header("content-type"), Some("application/problem+json"));
}

#[tokio::test]
async fn encryption_cases() {
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text", fixture("encrypted.docx"))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text());
    assert_eq!(r.json()["type"], "/problems/encrypted-document");

    let r = send(&app, raw_req("/v1/extract/text", fixture("locked.pdf"))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "locked PDF must never be an empty 200: {}", r.text());
    assert_eq!(r.json()["type"], "/problems/encrypted-document");

    for f in ["owner_only.pdf", "owner_only_rc4.pdf"] {
        let r = send(&app, raw_req("/v1/extract/text", fixture(f))).await;
        assert_eq!(r.status, StatusCode::OK, "{f}: {}", r.text());
        assert!(r.json()["text"].as_str().unwrap().contains("page-one-alpha"), "{f}");
    }
}

#[tokio::test]
async fn arabic_presentation_forms_are_normalized() {
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text", fixture("arabic_forms.pdf"))).await;
    assert_eq!(r.status, StatusCode::OK);
    let text = r.json()["text"].as_str().unwrap().to_string();
    assert!(!text.chars().any(|c| ('\u{FB50}'..='\u{FDFF}').contains(&c) || ('\u{FE70}'..='\u{FEFE}').contains(&c)), "{text:?}");
    assert!(text.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)), "{text:?}");
}

#[tokio::test]
async fn complex_script_native_pdfs() {
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text", fixture("native_hi.pdf"))).await;
    let j = r.json();
    assert!(j["text"].as_str().unwrap().contains("भारत"), "{}", j["text"]);
    assert_eq!(j["ocr"]["status"], "native");
    let r = send(&app, raw_req("/v1/extract/text", fixture("native_ar.pdf"))).await;
    let j = r.json();
    assert!(j["text"].as_str().unwrap().contains("مرحبا"), "{}", j["text"]);
}

/// `:slim` still classifies pages and reports what needs OCR.
#[cfg(not(feature = "ocr"))]
#[tokio::test]
async fn slim_reports_pages_needing_ocr() {
    let app = app();
    for f in ["scan_en.pdf", "scan_hi.pdf", "scan_ar.pdf"] {
        let r = send(&app, raw_req("/v1/extract/text", fixture(f))).await;
        assert_eq!(r.status, StatusCode::OK);
        let j = r.json();
        assert_eq!(j["ocr"]["status"], "unavailable", "{f}");
        assert_eq!(j["ocr"]["pages_needing_ocr"], json!([1]), "{f}");
        assert_eq!(j["ocr"]["reasons"][0]["reason"], "scanned", "{f}");
    }
    let r = send(&app, raw_req("/v1/extract/text", fixture("legacy_krutidev.pdf"))).await;
    let j = r.json();
    assert_eq!(j["ocr"]["status"], "unavailable");
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([1]));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "legacy_font");
    assert_eq!(j["ocr"]["lang"], "hi");
    assert_eq!(j["ocr"]["lang_source"], "font");

    let r = send(&app, raw_req("/v1/extract/text?ocr=off", fixture("scan_en.pdf"))).await;
    let j = r.json();
    assert_eq!(j["ocr"]["status"], "off");
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([]));

    // A scanner's junk OCR layer is dropped, not passed off as native text;
    // a good one stays native.
    let r = send(&app, raw_req("/v1/extract/text", fixture("scan_en_junk_layer.pdf"))).await;
    let j = r.json();
    assert_eq!(j["ocr"]["status"], "unavailable");
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([1]));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "scanned");
    assert_eq!(j["text"], "");
    let r = send(&app, raw_req("/v1/extract/text", fixture("scan_en_ocr_layer.pdf"))).await;
    let j = r.json();
    assert_eq!(j["ocr"]["status"], "native");
    assert!(j["text"].as_str().unwrap().contains("Invoice 4471"), "{}", j["text"]);

    let r = send(&app, raw_req("/v1/extract/text", fixture("scan_pages.tif"))).await;
    let j = r.json();
    assert_eq!((j["kind"].as_str(), j["ocr"]["status"].as_str()), (Some("image"), Some("unavailable")));
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([1, 2]));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "image");

    // Raw output carries the routing hint too.
    let mut req = raw_req("/v1/extract/text", fixture("scan_en.pdf"));
    req.headers_mut().insert(header::ACCEPT, "text/plain".parse().unwrap());
    let r = send(&app, req).await;
    assert_eq!(r.header("x-oxide-ocr-status"), Some("unavailable"));
    assert_eq!(r.header("x-oxide-pages-needing-ocr"), Some("1"));
}

#[tokio::test]
async fn info_endpoint() {
    let app = app();
    let r = send(&app, raw_req("/v1/info", fixture("sample.xlsx"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["kind"], "office");
    assert_eq!(j["format"], "xlsx");
    assert_eq!(j["sheets"][0]["name"], "Q1");
    assert_eq!(j["sheets"][0]["rows"], 3);
    assert_eq!(j["sheets"][0]["cols"], 3);
    assert_eq!(j["sheets"][1]["name"], "Q2");

    let r = send(&app, raw_req("/v1/info", fixture("sample.pptx"))).await;
    assert_eq!(r.json()["slide_count"], 2);

    let r = send(&app, raw_req("/v1/info", fixture("sample.ods"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!((j["kind"].as_str(), j["format"].as_str()), (Some("office"), Some("ods")));
    assert_eq!(j["sheets"][0]["name"], "Q1");
    assert_eq!(j["sheets"][0]["rows"], 3);
    assert_eq!(j["sheets"][0]["cols"], 3);
    assert_eq!(j["sheets"][1]["name"], "Q2");
    assert_eq!(j["metadata"]["title"], "Revenue Workbook");
    let r = send(&app, raw_req("/v1/info", fixture("sample.odp"))).await;
    assert_eq!(r.json()["slide_count"], 2);
    let r = send(&app, raw_req("/v1/info", fixture("sample.odt"))).await;
    assert_eq!(r.json()["metadata"]["title"], "Quarterly Report");

    let r = send(&app, raw_req("/v1/info", fixture("scan_pages.tif"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!((j["kind"].as_str(), j["format"].as_str()), (Some("image"), Some("tiff")));
    assert_eq!(j["page_count"], 2);
    assert_eq!(j["pages"][1], json!({"page": 2, "width": 1654, "height": 900}));
    let r = send(&app, raw_req("/v1/info", fixture("scan_en_exif.jpg"))).await;
    assert_eq!(r.json()["pages"][0]["width"], 900, "stored dimensions, before orientation");

    let r = send(&app, raw_req("/v1/info", fixture("native.pdf"))).await;
    let j = r.json();
    assert_eq!(j["kind"], "pdf");
    assert_eq!(j["page_count"], 3);
    assert_eq!(j["encrypted"], false);
    assert_eq!(j["metadata"]["title"], "Native Fixture");
    let w = j["pages"][0]["width"].as_f64().unwrap();
    assert!((w - 595.27).abs() < 1.0, "A4 width, got {w}");

    let r = send(&app, raw_req("/v1/info", fixture("owner_only.pdf"))).await;
    assert_eq!(r.json()["encrypted"], true);
    let r = send(&app, raw_req("/v1/info", fixture("locked.pdf"))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --------------------------------------------------------------------------
// Robustness
// --------------------------------------------------------------------------

/// Tiny deterministic PRNG so the fuzz corpus is reproducible.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuzz_corrupt_and_truncated_never_5xx() {
    let app = app();
    let seeds = [
        "native.pdf", "hybrid.pdf", "sample.docx", "sample.xlsx", "sample.pptx", "sample.doc", "sample.xls", "sample.ppt",
        "owner_only.pdf", "arabic_forms.pdf", "sample.odt", "sample.ods", "sample.odp", "line.gif", "scan_pages.tif",
        "scan_hi.jp2", "scan_ar.webp",
    ];
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut statuses: HashMap<u16, usize> = HashMap::new();
    for i in 0..170 {
        let mut bytes = fixture(seeds[i % seeds.len()]);
        match i % 4 {
            0 => bytes.truncate((rng.next() as usize) % bytes.len().max(1)),
            1 => {
                for _ in 0..1 + rng.next() % 64 {
                    let at = (rng.next() as usize) % bytes.len();
                    bytes[at] = rng.next() as u8;
                }
            }
            2 => {
                let at = (rng.next() as usize) % bytes.len();
                let len = ((rng.next() as usize) % 4096).min(bytes.len() - at);
                bytes.drain(at..at + len);
            }
            _ => {
                let at = 16 + (rng.next() as usize) % (bytes.len().saturating_sub(16)).max(1);
                bytes.truncate(at);
                bytes.extend((0..512).map(|_| rng.next() as u8));
            }
        }
        let endpoint = ["/v1/extract/text", "/v1/extract/markdown", "/v1/info"][i % 3];
        let r = send(&app, raw_req(endpoint, bytes)).await;
        assert!(!r.status.is_server_error(), "case {i} ({}) → {}: {}", seeds[i % seeds.len()], r.status, r.text());
        *statuses.entry(r.status.as_u16()).or_default() += 1;
    }
    eprintln!("fuzz status histogram: {statuses:?}");
}

#[tokio::test]
async fn deeply_nested_pdf_does_not_crash() {
    let app = app();
    for endpoint in ["/v1/extract/text", "/v1/info"] {
        let r = send(&app, raw_req(endpoint, fixture("nested.pdf"))).await;
        assert!(!r.status.is_server_error(), "{endpoint}: {} {}", r.status, r.text());
    }
}

#[tokio::test]
async fn misfiled_xref_entries_are_recovered() {
    // A stray xref-stream row shifts every later entry onto its neighbour's
    // offset: the ObjStm's entry points at an image XObject.
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text", fixture("misfiled_xref.pdf"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["page_count"], 1);
    assert!(j["text"].as_str().unwrap().contains("Misfiled xref recovered"), "{}", j["text"]);
    let r = send(&app, raw_req("/v1/info", fixture("misfiled_xref.pdf"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());

    // Recovering the misfiled Pages object sweeps the ObjStm, which still
    // holds a superseded copy of the page; the page's own entry is right.
    let r = send(&app, raw_req("/v1/extract/text", fixture("misfiled_xref_superseded.pdf"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let text = r.json()["text"].as_str().unwrap().to_owned();
    assert!(text.contains("Current page text") && !text.contains("Superseded"), "{text}");
}

// --------------------------------------------------------------------------
// Auth, limits, admission, params
// --------------------------------------------------------------------------

#[tokio::test]
async fn auth_is_required_on_v1_only() {
    let app = app();
    let req = Request::post("/v1/extract/text").body(Body::from(fixture("native.pdf"))).unwrap();
    let r = send(&app, req).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let req = Request::post("/v1/info").header(header::AUTHORIZATION, "Bearer wrong-key-0123456789").body(Body::empty()).unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::UNAUTHORIZED);
    for path in ["/healthz", "/readyz", "/version", "/metrics", "/openapi.json", "/openapi.yaml"] {
        let r = send(&app, Request::get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn bad_params_are_400() {
    let app = app();
    let pdf = fixture("native.pdf");
    for (q, param) in [("lang=fr", "lang"), ("ocr=maybe", "ocr"), ("pages=0", "pages"), ("format=rtf", "format"), ("detect_headings=perhaps", "detect_headings"), ("clean=maybe", "clean")] {
        let r = send(&app, raw_req(&format!("/v1/extract/text?{q}"), pdf.clone())).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}");
        let j = r.json();
        assert_eq!(j["parameter"], param);
        assert!(j["accepted"].is_string());
    }
    // Nested JSON params are rejected.
    let r = send(&app, json_req("/v1/extract/text", &pdf, json!({"options": {"lang": "hi"}}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Missing file.
    let r = send(&app, authed("POST", "/v1/extract/text").header(header::CONTENT_TYPE, "application/json").body(Body::from("{}")).unwrap()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = send(&app, raw_req("/v1/extract/text", vec![])).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn body_over_limit_is_413() {
    let (app, _) = app_with(&[("OXIDE_API_MAX_BODY_BYTES", "4096")]);
    let big = vec![b'x'; 10_000];
    // With Content-Length.
    let r = send(&app, raw_req("/v1/extract/text", big.clone())).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Without Content-Length (streamed body).
    let req = authed("POST", "/v1/extract/text").body(Body::new(ChunkBody::new(&big))).unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Multipart.
    let r = send(&app, multipart_req("/v1/extract/text", &big, Some("a.pdf"), &[])).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A body streamed in chunks, with no size hint (no Content-Length).
struct ChunkBody(Vec<axum::body::Bytes>);

impl ChunkBody {
    fn new(v: &[u8]) -> Self {
        Self(v.chunks(1000).map(axum::body::Bytes::copy_from_slice).rev().collect())
    }
}

impl http_body::Body for ChunkBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Ready(self.0.pop().map(|b| Ok(http_body::Frame::data(b))))
    }
}

#[tokio::test]
async fn admission_rejects_with_429_and_releases_permits() {
    let (app, st) = app_with(&[("OXIDE_API_MAX_INFLIGHT", "1"), ("OXIDE_API_QUEUE_WAIT_MS", "50")]);
    let held = st.admission.clone().acquire_owned().await.unwrap();
    let r = send(&app, raw_req("/v1/extract/text", fixture("native.pdf"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.header("retry-after").is_some());
    drop(held);
    let r = send(&app, raw_req("/v1/extract/text", fixture("native.pdf"))).await;
    assert_eq!(r.status, StatusCode::OK);
    // The permit goes back once the job is done.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(st.admission.available_permits(), 1);
}

#[tokio::test]
async fn ops_endpoints() {
    let (app, st) = app_with(&[]);
    let r = send(&app, Request::get("/version").body(Body::empty()).unwrap()).await;
    let j = r.json();
    assert_eq!(j["engines"]["pdf_oxide"], pdf_oxide::VERSION);
    assert_eq!(j["engines"]["office_oxide"], office_oxide::VERSION);
    assert_eq!(j["limits"]["max_inflight"], 8);
    assert!(r.header("x-content-type-options").is_some());
    assert!(r.header("x-request-id").is_some());

    // Draining flips /readyz first and refuses new work.
    st.health.start_draining();
    let r = send(&app, Request::get("/readyz").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    let r = send(&app, raw_req("/v1/extract/text", fixture("native.pdf"))).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);

    let r = send(&app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert!(r.text().contains("oxide_http_requests_total"));
}

#[tokio::test]
async fn openapi_documents_every_route() {
    let spec: Value = serde_json::from_str(oxide_api::OPENAPI_JSON).unwrap();
    for path in ["/v1/extract/text", "/v1/extract/markdown", "/v1/extract/fields", "/v1/info", "/healthz", "/readyz", "/version", "/metrics"] {
        assert!(spec["paths"].get(path).is_some(), "{path} missing from openapi.json");
    }
    // The YAML and JSON contracts are the same document.
    let yaml: Value = serde_yaml_ng::from_str(oxide_api::OPENAPI_YAML).unwrap();
    if let Some(diff) = first_diff(&yaml, &spec, "$") {
        panic!("openapi.yaml and openapi.json differ at {diff} (run scripts/openapi_json.sh)");
    }

    // Every top-level field the API returns for a PDF is in the schema.
    let app = app();
    let r = send(&app, raw_req("/v1/extract/text", fixture("native.pdf"))).await;
    let props = &spec["components"]["schemas"]["PdfExtraction"]["properties"];
    for k in r.json().as_object().unwrap().keys() {
        assert!(props.get(k).is_some(), "PdfExtraction schema lacks `{k}`");
    }
    let ocr_props = &spec["components"]["schemas"]["OcrInfo"]["properties"];
    for k in r.json()["ocr"].as_object().unwrap().keys() {
        assert!(ocr_props.get(k).is_some(), "OcrInfo schema lacks `{k}`");
    }
    let r = send(&app, raw_req("/v1/extract/text", fixture("sample.docx"))).await;
    let props = &spec["components"]["schemas"]["OfficeExtraction"]["properties"];
    for k in r.json().as_object().unwrap().keys() {
        assert!(props.get(k).is_some(), "OfficeExtraction schema lacks `{k}`");
    }
    let r = send(&app, raw_req("/v1/extract/text?ocr=off", fixture("scan_en.png"))).await;
    let props = &spec["components"]["schemas"]["ImageExtraction"]["properties"];
    for k in r.json().as_object().unwrap().keys() {
        assert!(props.get(k).is_some(), "ImageExtraction schema lacks `{k}`");
    }
    let r = send(&app, raw_req("/v1/info", fixture("scan_pages.tif"))).await;
    let props = &spec["components"]["schemas"]["ImageInfo"]["properties"];
    for k in r.json().as_object().unwrap().keys() {
        assert!(props.get(k).is_some(), "ImageInfo schema lacks `{k}`");
    }
    let specs = json!([{"name": "first", "patterns": [r"page-(\w+)-alpha"]}]);
    let r = send(&app, json_req("/v1/extract/fields", &fixture("native.pdf"), json!({"fields": specs, "include_text": true, "foo": 1}))).await;
    let props = &spec["components"]["schemas"]["FieldsExtraction"]["properties"];
    for k in r.json().as_object().unwrap().keys() {
        assert!(props.get(k).is_some(), "FieldsExtraction schema lacks `{k}`");
    }
    // Every key a field spec may carry is documented, and nothing more.
    let mut documented: Vec<&str> = spec["components"]["schemas"]["FieldSpec"]["properties"].as_object().unwrap().keys().map(String::as_str).collect();
    let mut known = oxide_api::fields::FIELD_KEYS.to_vec();
    documented.sort_unstable();
    known.sort_unstable();
    assert_eq!(documented, known, "FieldSpec properties drift from fields::FIELD_KEYS");
    // Every accepted `format` value is in the contract.
    let formats = &spec["components"]["parameters"]["format"]["schema"]["enum"];
    for f in oxide_api::detect::ACCEPTED_FORMATS.split(", ") {
        assert!(formats.as_array().unwrap().iter().any(|v| v == f), "format enum lacks `{f}`");
    }
}

#[tokio::test]
async fn image_limits() {
    let (app, _) = app_with(&[("OXIDE_API_MAX_IMAGE_PIXELS", "1000000")]);
    let r = send(&app, raw_req("/v1/extract/text", fixture("scan_en.png"))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text());
    assert_eq!(r.json()["type"], "/problems/image-too-large");
    assert_eq!(r.json()["limit_pixels"], 1_000_000);
    let r = send(&app, raw_req("/v1/info", fixture("scan_pages.tif"))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let (app, _) = app_with(&[("OXIDE_API_MAX_PDF_PAGES", "1")]);
    let r = send(&app, raw_req("/v1/extract/text", fixture("scan_pages.tif"))).await;
    assert_eq!(r.json()["type"], "/problems/too-many-pages");
}

fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => {
                        if let Some(d) = first_diff(va, vb, &format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                    _ => return Some(format!("{path}.{k} (present on one side only)")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            x.iter().zip(y).enumerate().find_map(|(i, (va, vb))| first_diff(va, vb, &format!("{path}[{i}]")))
        }
        _ if a == b => None,
        _ => Some(format!("{path}: yaml={a} json={b}")),
    }
}

/// A body that sends one chunk and then stalls forever.
struct StalledBody(bool);

impl http_body::Body for StalledBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.0 {
            return std::task::Poll::Pending;
        }
        self.0 = true;
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(axum::body::Bytes::from_static(b"%PDF-1.7\n")))))
    }
}

#[tokio::test]
async fn slow_upload_is_408() {
    let (app, st) = app_with(&[("OXIDE_API_BODY_TIMEOUT_SECS", "1")]);
    let req = authed("POST", "/v1/extract/text").body(Body::new(StalledBody(false))).unwrap();
    let r = send(&app, req).await;
    assert_eq!(r.status, StatusCode::REQUEST_TIMEOUT, "{}", r.text());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(st.admission.available_permits(), 8, "permit released");
}

/// A valid PDF with `n` text pages.
fn many_page_pdf(n: usize) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(), // pages tree, filled below
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = String::new();
    for i in 0..n {
        let content = format!("BT /F1 12 Tf 72 720 Td (Page {} of the long fixture) Tj ET", i + 1);
        objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).into_bytes());
        let content_id = objs.len();
        objs.push(
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>")
                .into_bytes(),
        );
        kids.push_str(&format!("{} 0 R ", objs.len()));
    }
    objs[1] = format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes();
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_returns_partial_pdf_results() {
    let (app, _) = app_with(&[("OXIDE_API_PROCESS_TIMEOUT_SECS", "1"), ("OXIDE_API_MAX_PDF_PAGES", "100000")]);
    let pdf = many_page_pdf(30_000);
    // Markdown + page classification: the heaviest native path per page.
    let r = send(&app, raw_req("/v1/extract/markdown", pdf)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["incomplete"], true, "30k pages should not finish in 1 s");
    let done = j["pages_extracted"].as_array().unwrap();
    assert!(!done.is_empty() && done.len() < 30_000, "{} pages", done.len());
    // A contiguous prefix: the caller can resume with `pages=<next>-`.
    assert!(done.iter().enumerate().all(|(i, p)| p.as_u64() == Some(i as u64 + 1)));
    assert!(j["text"].as_str().unwrap().contains("Page 1 of the long fixture"));
    eprintln!("extracted {} pages before the deadline", done.len());

    // The page-count guard.
    let (app, _) = app_with(&[]);
    let r = send(&app, raw_req("/v1/extract/text", many_page_pdf(2001))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["type"], "/problems/too-many-pages");
}

// --------------------------------------------------------------------------
// /v1/extract/fields
// --------------------------------------------------------------------------

fn fields_json(uri: &str, body: Value) -> Request<Body> {
    authed("POST", uri).header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn fields_from_supplied_text() {
    let app = app();
    let text = "Tax Invoice\nInvoice No:   INV-2026/0042\nPO Number\n  PO-77/A\nTotal amount due: 1,250.00\n\n\n\nRef: R-1 Ref: R-2";
    let r = send(
        &app,
        fields_json(
            "/v1/extract/fields?pages=1",
            json!({
                "text": text,
                "include_text": true,
                "clean": true,
                "fields": [
                    {"name": "invoice_number", "patterns": [r"(?i)invoice\s*no[:.]?\s*([A-Z0-9 _./-]+)"], "required": true},
                    {"name": "po_number", "patterns": [r"PO Number\s+([^\n]+)"]},
                    {"name": "total", "patterns": [r"Total amount due:\s*([\d,]+\.\d{2})"], "max_length": 12},
                    {"name": "refs", "patterns": [r"Ref: (R-\d)"], "multiple": true},
                    {"name": "iban", "patterns": [r"IBAN:\s*(\S+)"], "required": true},
                ],
            }),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["kind"], "text");
    assert!(j.get("format").is_none() && j.get("page_count").is_none());
    assert_eq!(
        j["fields"],
        json!({"invoice_number": "INV-2026/0042", "po_number": "PO-77/A", "total": "1,250.00", "refs": ["R-1", "R-2"], "iban": null})
    );
    // Request order, not alphabetical (read off the wire: a parsed Value sorts its keys).
    let body = r.text();
    let at = |k: &str| body.find(&format!("\"{k}\":")).unwrap();
    assert!(at("invoice_number") < at("po_number") && at("po_number") < at("total") && at("total") < at("refs") && at("refs") < at("iban"));
    assert_eq!(j["missing_required"], json!(["iban"]));
    // `clean` ran before the fields: the blank lines are collapsed in the text they saw.
    assert!(!j["text"].as_str().unwrap().contains("\n\n\n"));
    assert_eq!(j["char_count"].as_u64().unwrap() as usize, j["text"].as_str().unwrap().chars().count());
    // No document, so the document params don't apply.
    assert_eq!(j["ignored_params"], json!(["pages"]));
    assert_eq!(r.header("cache-control"), Some("no-store"));

    // Empty text is text: every field is null, a required one is missing, and the
    // specs are still checked (callers use this to validate fields without a document).
    let r = send(
        &app,
        fields_json(
            "/v1/extract/fields",
            json!({"text": "", "include_text": true, "fields": [
                {"name": "a", "patterns": [r"(\S*)"]},
                {"name": "b", "patterns": ["x"], "required": true},
            ]}),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["kind"], "text");
    assert_eq!(j["fields"], json!({"a": null, "b": null}));
    assert_eq!(j["missing_required"], json!(["b"]));
    assert_eq!(j["char_count"], 0);
    assert_eq!(j["text"], "");
    let r = send(&app, fields_json("/v1/extract/fields", json!({"text": "", "fields": [{"name": "a", "patterns": ["(?<=x)y"]}]}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["parameter"], "fields[0].patterns[0]");

    // Without include_text the text stays home.
    let r = send(&app, fields_json("/v1/extract/fields", json!({"text": text, "fields": [{"name": "a", "patterns": ["Tax"]}]}))).await;
    assert!(r.json().get("text").is_none());
}

#[tokio::test]
async fn typed_fields_check_and_rewrite_their_values() {
    let app = app();
    let text = "Due Date: on receipt\nInvoice Date: 14 Oct 2026\nGSTIN: 27AAPFU0939F1ZW\nGSTIN: 27AAPFU0939F1ZV\nTotal: ₹ 1,25,000.50";
    let r = send(
        &app,
        fields_json(
            "/v1/extract/fields",
            json!({"text": text, "fields": [
                {"name": "invoice_date", "patterns": [r"Date:\s*([^\n]+)"], "type": "date"},
                {"name": "gstin", "patterns": [r"GSTIN:\s*(\S+)"], "type": "gstin"},
                {"name": "total", "patterns": [r"Total:\s*([^\n]+)"], "type": "amount", "format": "point"},
                {"name": "pan", "patterns": [r"PAN:\s*(\S+)"], "type": "pan", "required": true},
                {"name": "note", "patterns": [r"(Tax)"]},
            ]}),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    // "on receipt" is no date and the first GSTIN fails its check: both move on to the next match.
    assert_eq!(
        j["fields"],
        json!({"invoice_date": "2026-10-14", "gstin": "27AAPFU0939F1ZV", "total": "125000.50", "pan": null, "note": null})
    );
    assert_eq!(
        j["matched"],
        json!({"invoice_date": "14 Oct 2026", "gstin": "27AAPFU0939F1ZV", "total": "₹ 1,25,000.50", "pan": null, "note": null})
    );
    assert_eq!(j["missing_required"], json!(["pan"]));

    // A type or format that does not exist is refused before anything is read.
    for (spec, param) in [
        (json!({"name": "t", "patterns": ["x"], "type": "money"}), "fields[0].type"),
        (json!({"name": "t", "patterns": ["x"], "type": "date", "format": "comma"}), "fields[0].format"),
        (json!({"name": "t", "patterns": ["x"], "format": "dmy"}), "fields[0].format"),
    ] {
        let r = send(&app, fields_json("/v1/extract/fields", json!({"text": "", "fields": [spec]}))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
        assert_eq!(r.json()["parameter"], param);
    }
}

#[tokio::test]
async fn po_date_numeric_format_is_checked_after_the_regex_matches() {
    let app = app();
    let text = "ver nem PURCHASE ORDER\nPurchase Order No.: 13693\n1400 N. Providence Rd., Suite 302\nMedia, PA 19063 Date Issued 9/29/22\nPhone: 484-234-6030";
    // The third pattern from a real PO field: the regex captures the date;
    // DMY rejects its month, while MDY reads the same capture successfully.
    let pattern = concat!(
        r"(?i)(?:date\s*issued|purchase\s+order\s+date|p\.?\s*o\.?\s*date|order\s+date)[ \t]*[:.#-]*[ \t]*",
        r"(\d{1,4}[./-]\d{1,2}[./-]\d{1,4}|\d{1,2}(?:st|nd|rd|th)?[ \t./-]+[A-Za-z]{3,9}\.?,?[ \t./-]+\d{2,4}|[A-Za-z]{3,9}\.?[ \t]+\d{1,2}(?:st|nd|rd|th)?,?[ \t]+\d{2,4})",
    );
    let r = send(
        &app,
        fields_json(
            "/v1/extract/fields",
            json!({"text": text, "fields": [
                {"name": "printed", "patterns": [pattern]},
                {"name": "day_first", "patterns": [pattern], "type": "date", "format": "dmy", "required": true},
                {"name": "po_date", "patterns": [pattern], "type": "date", "format": "mdy", "required": true},
            ]}),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["fields"], json!({"printed": "9/29/22", "day_first": null, "po_date": "2022-09-29"}));
    assert_eq!(j["matched"], json!({"printed": "9/29/22", "day_first": null, "po_date": "9/29/22"}));
    assert_eq!(j["missing_required"], json!(["day_first"]));
}

#[tokio::test]
async fn fields_from_a_document_in_every_encoding() {
    let app = app();
    let pdf = fixture("native.pdf");
    let specs = json!([
        {"name": "first", "patterns": [r"page-(\w+)-alpha"]},
        {"name": "pages", "patterns": [r"page-(\w+)-(?:alpha|beta|gamma)"], "multiple": true},
    ]);
    let reqs = [
        ("multipart", multipart_req("/v1/extract/fields", &pdf, Some("native.pdf"), &[("fields", &specs.to_string())])),
        ("json", json_req("/v1/extract/fields", &pdf, json!({"fields": specs}))),
        ("raw", raw_req(&format!("/v1/extract/fields?fields={}", urlencode(&specs.to_string())), pdf.clone())),
    ];
    for (enc, req) in reqs {
        let r = send(&app, req).await;
        assert_eq!(r.status, StatusCode::OK, "{enc}: {}", r.text());
        let j = r.json();
        assert_eq!(j["kind"], "pdf", "{enc}");
        assert_eq!(j["format"], "pdf");
        assert_eq!(j["format_source"], "sniff");
        assert_eq!(j["page_count"], 3);
        assert_eq!(j["ocr"]["status"], "native");
        assert_eq!(j["fields"], json!({"first": "one", "pages": ["one", "two", "three"]}), "{enc}");
        assert_eq!(j["missing_required"], json!([]));
        assert!(j.get("text").is_none());
        assert!(r.header("x-engine").unwrap().starts_with("pdf_oxide/"));
    }

    // Page selection narrows the text the fields see.
    let r = send(&app, json_req("/v1/extract/fields", &pdf, json!({"fields": specs, "pages": "2", "include_text": true}))).await;
    let j = r.json();
    assert_eq!(j["fields"], json!({"first": null, "pages": ["two"]}));
    assert_eq!(j["pages_extracted"], json!([2]));
    assert!(j["text"].as_str().unwrap().contains("page-two-beta"));

    // Office: no page details, and the paged params are listed as ignored.
    let r = send(&app, json_req("/v1/extract/fields", &fixture("sample.docx"), json!({"ocr": "off", "fields": [{"name": "m", "patterns": [r"(docx)-alpha"]}]}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let j = r.json();
    assert_eq!(j["kind"], "office");
    assert_eq!(j["fields"], json!({"m": "docx"}));
    assert!(j.get("page_count").is_none());
    assert_eq!(j["ignored_params"], json!(["ocr"]));

    // Asking for raw text changes nothing: the values only exist as JSON.
    let mut req = json_req("/v1/extract/fields", &pdf, json!({"fields": specs}));
    req.headers_mut().insert(header::ACCEPT, "text/plain".parse().unwrap());
    let r = send(&app, req).await;
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    assert_eq!(r.json()["fields"]["first"], "one");
}

#[tokio::test]
async fn fields_errors_are_400_before_any_reading() {
    let app = app();
    let pdf = fixture("native.pdf");
    // A bad pattern names its place in the request and the engine's reason.
    let r = send(&app, json_req("/v1/extract/fields", &pdf, json!({"fields": [{"name": "t", "patterns": ["ok", r"(?<=Total)\d+"]}]}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let j = r.json();
    assert_eq!(j["type"], "/problems/invalid-parameter");
    assert_eq!(j["parameter"], "fields[0].patterns[1]");
    assert!(j["detail"].as_str().unwrap().contains("look-around"), "{j}");
    assert!(j["accepted"].is_string());
    // ... and so does a capture group the pattern lacks.
    let r = send(&app, json_req("/v1/extract/fields", &pdf, json!({"fields": [{"name": "t", "patterns": ["(a)"], "capture_group": 3}]}))).await;
    assert_eq!(r.json()["parameter"], "fields[0].capture_group");
    // A bad spec is refused even when the document itself is unreadable: specs come first.
    let r = send(&app, json_req("/v1/extract/fields", b"%PDF-1.7 garbage", json!({"fields": [{"name": "t", "patterns": ["("]}]}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    for (body, why) in [
        (json!({"text": "x"}), "no fields"),
        (json!({"fields": [{"name": "t", "patterns": ["x"]}]}), "nothing to read"),
        (json!({"text": null, "fields": [{"name": "t", "patterns": ["x"]}]}), "null text is no text"),
        (json!({"text": 42, "fields": [{"name": "t", "patterns": ["x"]}]}), "text not a string"),
        (json!({"text": "x", "fields": [{"name": "t", "patterns": ["x"]}], "include_text": "maybe"}), "bad include_text"),
        (json!({"text": "x", "fields": {"name": "t"}}), "fields not an array"),
    ] {
        let r = send(&app, fields_json("/v1/extract/fields", body)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{why}: {}", r.text());
    }
    // Nothing to read names what was missing.
    let r = send(&app, fields_json("/v1/extract/fields", json!({"fields": [{"name": "t", "patterns": ["x"]}]}))).await;
    assert_eq!(r.json()["parameter"], "text");
    // Both a document and text.
    let r = send(&app, json_req("/v1/extract/fields", &pdf, json!({"text": "x", "fields": [{"name": "t", "patterns": ["x"]}]}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["parameter"], "text");
    // A raw body has nowhere to carry the specs but the query string.
    let r = send(&app, raw_req("/v1/extract/fields", pdf)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["parameter"], "fields");
    // Authenticated like every other /v1 route.
    let req = Request::post("/v1/extract/fields").header(header::CONTENT_TYPE, "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::UNAUTHORIZED);
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}
