//! Exercise CLI routing and process lifecycle without downloading OCR models.
#![cfg(all(feature = "ocr", unix))]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use oxide_api::config::Config;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const KEY: &str = "test-key-0123456789";

struct Harness {
    app: Router,
    state: Arc<oxide_api::AppState>,
    dir: TempDir,
}

impl Harness {
    fn new(script: &str, math: bool, debug_limit: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("indra-test");
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        for file in [
            "det.onnx",
            "rec.onnx",
            "order.onnx",
            "meta.json",
            "mathcls.onnx",
            "math_enc.onnx",
            "math_dec_init.onnx",
            "math_dec_step.onnx",
            "math_meta.json",
        ] {
            std::fs::write(dir.path().join(file), []).unwrap();
        }
        let cfg = Config::from_lookup(|key| match key {
            "OXIDE_API_API_KEY" => Some(KEY.into()),
            "OXIDE_API_CPU_THREADS" | "OXIDE_API_OCR_WORKERS" => Some("1".into()),
            "OXIDE_API_OCR_MODEL_DIR" => Some("/nonexistent/paddle-models".into()),
            "OXIDE_API_INDRA_CLI" => Some(cli.to_string_lossy().into_owned()),
            "OXIDE_API_INDRA_MODEL_DIR" => Some(dir.path().to_string_lossy().into_owned()),
            "OXIDE_API_INDRA_MATH" => Some(math.to_string()),
            "OXIDE_API_MAX_DEBUG_BYTES" => Some(debug_limit.to_string()),
            "OXIDE_API_OCR_TIMEOUT_SECS" => Some("2".into()),
            _ => None,
        })
        .unwrap();
        let state = oxide_api::build_state(cfg);
        Self {
            app: oxide_api::build_router(state.clone()),
            state,
            dir,
        }
    }
}

const CLI: &str = r##"#!/bin/sh
math=false
for arg in "$@"; do if [ "$arg" = --math ]; then math=true; fi; done
for arg in "$@"; do
  case "$arg" in
    */page-*.png)
      printf '{"file":"%s","page":0,"source":"ocr","rotation":0,"lines":[{"text":"math %s","box":[10,20,100,40],"conf":0.87}],"markdown":"# math %s","blocks":[],"timings":{"total_ms":7}}\n' "$arg" "$math" "$math"
      ;;
  esac
done
"##;

async fn send(app: &Router, uri: &str, file: &str) -> (StatusCode, Value) {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let request = Request::post(uri)
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(header::ACCEPT, "text/plain")
        .body(Body::from(bytes))
        .unwrap();
    let r = app.clone().oneshot(request).await.unwrap();
    let status = r.status();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn routes_selected_pages_and_returns_full_debug_and_scores() {
    let h = Harness::new(CLI, true, 1 << 20);
    let (status, j) = send(
        &h.app,
        "/v1/extract/document?ocr_engine=indra&ocr=force&pages=2&include_debug=true",
        "native.pdf",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{j}");
    assert_eq!(j["ocr"]["engine"], "indra");
    assert_eq!(j["pages_extracted"], json!([2]));
    assert_eq!(j["pages"][0]["blocks"][0]["confidence"], json!(0.87));
    assert_eq!(j["pages"][0]["text"], "math true");
    let steps = j["debug"]["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|s| s["step"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pdf_oxide_parse", "ocr_routing", "indra_cli", "ocr_parse", "assemble"]
    );
    assert!(
        steps[0]["output"]["pages"][0]["text"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert_eq!(steps[2]["input"]["math"], true);
    assert_eq!(steps[2]["input"]["pages"][0]["page"], 2);
    assert_eq!(
        steps[2]["output"]["pages"][0]["lines"][0]["conf"],
        json!(0.87)
    );
    let encoded = j["debug"].to_string();
    assert!(!encoded.contains(KEY));
    assert!(!encoded.contains(h.dir.path().to_str().unwrap()));
    assert!(!encoded.contains("file_base64"));
    assert_eq!(j["debug"]["truncated"], false);
}

#[tokio::test]
async fn images_markdown_and_disabled_math_use_the_same_cli() {
    let h = Harness::new(CLI, false, 1 << 20);
    let (status, j) = send(
        &h.app,
        "/v1/extract/markdown?ocr_engine=indra&include_debug=true",
        "scan_en.png",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{j}");
    assert_eq!(j["text"], "# math false");
    assert_eq!(j["debug"]["steps"][2]["input"]["math"], false);
    let (status, j) = send(
        &h.app,
        "/v1/extract/text?ocr_engine=indra&pages=2&include_debug=true",
        "scan_pages.tif",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{j}");
    assert_eq!(j["pages_extracted"], json!([2]));
    assert_eq!(j["text"], "math false");
}

#[tokio::test]
async fn body_engine_options_override_query_and_debug_is_opt_in() {
    use base64::Engine as _;
    let h = Harness::new(CLI, true, 1 << 20);
    let image = std::fs::read(format!("{}/tests/fixtures/scan_en.png", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let payload = json!({"file_base64":base64::engine::general_purpose::STANDARD.encode(&image),"ocr_engine":"indra"});
    let mut multipart = b"--boundary\r\nContent-Disposition: form-data; name=\"ocr_engine\"\r\n\r\nindra\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"scan.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    multipart.extend(image);
    multipart.extend(b"\r\n--boundary--\r\n");
    for (content_type, bytes) in [("application/json", payload.to_string().into_bytes()), ("multipart/form-data; boundary=boundary", multipart)] {
        let request = Request::post("/v1/extract/text?ocr_engine=paddle")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(bytes)).unwrap();
        let response = h.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["ocr"]["engine"], "indra");
        assert_eq!(result["text"], "math true");
        assert!(result.get("debug").is_none());
    }
}

#[tokio::test]
async fn honors_native_and_off_routes_and_bounds_debug() {
    let h = Harness::new("#!/bin/sh\nexit 99\n", true, 80);
    for uri in [
        "/v1/extract/document?ocr_engine=indra&include_debug=true",
        "/v1/extract/document?ocr_engine=indra&ocr=off&include_debug=true",
    ] {
        let (status, j) = send(&h.app, uri, "native.pdf").await;
        assert_eq!(status, StatusCode::OK, "{j}");
        assert!(j["ocr"]["pages"].as_array().unwrap().is_empty());
        assert_eq!(j["debug"]["truncated"], true);
        assert!(
            !j["debug"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["step"] == "indra_cli")
        );
    }
    let (status, _) = send(
        &h.app,
        "/v1/extract/document?ocr_engine=wrong",
        "native.pdf",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, j) = send(
        &h.app,
        "/v1/extract/document?ocr_engine=indra&lang=hi",
        "native.pdf",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["ignored_params"], json!(["lang"]));
}

#[tokio::test]
async fn reports_missing_engine_and_page_failures_without_false_success() {
    let h = Harness::new(CLI, true, 1 << 20);
    std::fs::remove_file(h.dir.path().join("math_enc.onnx")).unwrap();
    let (status, j) = send(
        &h.app,
        "/v1/extract/document?ocr_engine=indra&ocr=force&include_debug=true",
        "native.pdf",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{j}");
    assert!(j["debug"]["steps"].is_array());
    let bad_cli = CLI.replace("\"source\":\"ocr\"", "\"source\":\"error\"");
    let h = Harness::new(&bad_cli, true, 1 << 20);
    let (status, j) = send(
        &h.app,
        "/v1/extract/text?ocr_engine=indra&include_debug=true",
        "scan_en.png",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{j}");
    assert_eq!(j["type"], "/problems/ocr-failed");
}

#[tokio::test]
async fn timeout_reaps_the_cli_and_releases_its_slot() {
    let h = Harness::new("#!/bin/sh\nexec sleep 30\n", true, 1 << 20);
    let (status, j) = send(
        &h.app,
        "/v1/extract/text?ocr_engine=indra&include_debug=true",
        "scan_en.png",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{j}");
    assert_eq!(j["type"], "/problems/processing-timeout");
    assert_eq!(h.state.indra.slots.available_permits(), 1);
    assert!(h.state.admission.available_permits() > 0);
}

#[tokio::test]
async fn cancellation_kills_the_cli() {
    let h = Harness::new(CLI, true, 1 << 20);
    let marker = h.dir.path().join("pid");
    std::fs::write(
        h.dir.path().join("indra-test"),
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    let app = h.app.clone();
    let task = tokio::spawn(async move {
        send(
            &app,
            "/v1/extract/text?ocr_engine=indra&include_debug=true",
            "scan_en.png",
        )
        .await
    });
    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(marker).unwrap();
    task.abort();
    let _ = task.await;
    for _ in 0..100 {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled request left Indra running");
}
