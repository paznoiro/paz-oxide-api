//! OCR tests (`--features ocr`). They need ONNX Runtime and the models:
//! `ORT_DYLIB_PATH` and `OXIDE_API_OCR_MODEL_DIR` (see scripts/test_ocr.sh).
#![cfg(feature = "ocr")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use oxide_api::config::Config;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: &str = "test-key-0123456789";

fn model_dir() -> PathBuf {
    PathBuf::from(std::env::var("OXIDE_API_OCR_MODEL_DIR").expect("set OXIDE_API_OCR_MODEL_DIR (see scripts/test_ocr.sh)"))
}

async fn app_with(overrides: &[(&str, &str)]) -> (Router, Arc<oxide_api::AppState>) {
    let dir = model_dir();
    let mut m: HashMap<String, String> = HashMap::from([
        ("OXIDE_API_API_KEY".into(), KEY.into()),
        ("OXIDE_API_CPU_THREADS".into(), "4".into()),
        ("OXIDE_API_OCR_WORKERS".into(), "2".into()),
        ("OXIDE_API_OCR_MODEL_DIR".into(), dir.to_string_lossy().into_owned()),
    ]);
    for (k, v) in overrides {
        m.insert(k.to_string(), v.to_string());
    }
    let st = oxide_api::build_state(Config::from_lookup(|k| m.get(k).cloned()).unwrap());
    let start = Instant::now();
    while !st.health.is_ready() {
        assert!(start.elapsed() < Duration::from_secs(120), "OCR never became ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (oxide_api::build_router(st.clone()), st)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

async fn extract(app: &Router, uri: &str, file: &str) -> (StatusCode, Value) {
    let req = Request::post(uri)
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .body(Body::from(fixture(file)))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn text(j: &Value) -> &str {
    j["text"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scans_per_language_with_probe() {
    let (app, _) = app_with(&[]).await;
    for (file, lang, must_contain) in [
        ("scan_en.pdf", "en", &["Invoice 4471", "Payment terms"][..]),
        // The PP-OCRv3 Devanagari model drops some vowel signs and conjuncts
        // (≈0.8 character accuracy); assert on words it reads reliably.
        ("scan_hi.pdf", "hi", &["भारत", "हम भारत", "लोग", "समता"][..]),
        ("scan_ar.pdf", "ar", &["مرحبا بالعالم", "شكرا"][..]),
        ("scan_ta.pdf", "ta", &["தமிழ்நாடு அரசு", "வணக்கம் உலகம்", "நன்றி"][..]),
    ] {
        let (status, j) = extract(&app, "/v1/extract/text", file).await;
        assert_eq!(status, StatusCode::OK, "{file}: {j}");
        eprintln!("--- {file}: lang={} source={}\n{}", j["ocr"]["lang"], j["ocr"]["lang_source"], text(&j));
        assert_eq!(j["ocr"]["status"], "ocr", "{file}");
        assert_eq!(j["ocr"]["pages"], json!([1]));
        assert_eq!(j["ocr"]["lang"], lang, "{file}");
        assert_eq!(j["ocr"]["lang_source"], "probe", "{file}");
        for m in must_contain {
            assert!(text(&j).contains(m), "{file}: expected {m:?} in {:?}", text(&j));
        }
        // Materially more text than with OCR off.
        let (_, off) = extract(&app, "/v1/extract/text?ocr=off", file).await;
        assert!(text(&j).chars().count() > text(&off).chars().count() + 20, "{file}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanned_page_overlay_text_does_not_pick_language() {
    // A browser print header/footer is a Latin text layer over a Hindi scan.
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "scan_hi_overlay.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- overlay: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "scanned");
    assert_eq!(j["ocr"]["lang"], "hi");
    assert_eq!(j["ocr"]["lang_source"], "probe");
    assert!(text(&j).contains("भारत"), "{:?}", text(&j));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanner_ocr_layer_is_kept_unless_junk() {
    // A scanner's invisible OCR layer over the scan: kept when it's right…
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "scan_en_ocr_layer.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    assert_eq!(j["ocr"]["status"], "native", "{j}");
    assert!(text(&j).contains("Invoice 4471"), "{:?}", text(&j));

    // …OCR'd again when the scanner wrote nonsense, junk dropped.
    let (status, j) = extract(&app, "/v1/extract/text", "scan_en_junk_layer.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- junk layer: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "scanned");
    assert_eq!(j["ocr"]["lang_source"], "probe");
    assert!(text(&j).contains("Invoice 4471"), "{:?}", text(&j));
    assert!(!text(&j).contains("cxAtl"), "{:?}", text(&j));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_lang_param_wins() {
    let (app, _) = app_with(&[]).await;
    let (_, j) = extract(&app, "/v1/extract/text?lang=ar", "scan_ar.pdf").await;
    assert_eq!(j["ocr"]["lang"], "ar");
    assert_eq!(j["ocr"]["lang_source"], "param");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_page_is_not_duplicated() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "hybrid.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- hybrid: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["ocr"]["status"], "hybrid");
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "hybrid");
    let t = text(&j);
    assert!(t.contains("hybrid-native"), "native text kept");
    assert!(t.contains("APPROVED BY FINANCE"), "stamp OCR'd: {t:?}");
    assert_eq!(t.matches("Purchase order 5521").count(), 1, "native line not duplicated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_pages_never_hit_inference() {
    let (app, _) = app_with(&[]).await;
    let (_, j) = extract(&app, "/v1/extract/text", "native.pdf").await;
    assert_eq!(j["ocr"]["status"], "native");
    assert_eq!(j["ocr"]["pages"], json!([]));
    assert!(j["ocr"].get("reasons").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_font_routes_to_hindi_ocr() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "legacy_krutidev.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- krutidev: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "legacy_font");
    assert_eq!(j["ocr"]["lang"], "hi");
    assert_eq!(j["ocr"]["lang_source"], "font");
    assert_eq!(j["ocr"]["pages"], json!([1]), "rendered page went through OCR");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_ocrs_native_pages_by_rendering() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text?ocr=force&pages=1", "native.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- force: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["ocr"]["status"], "ocr");
    assert_eq!(j["ocr"]["reasons"][0]["reason"], "forced");
    assert_eq!(j["ocr"]["lang_source"], "text");
    assert!(text(&j).contains("quick brown fox"), "{:?}", text(&j));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_endpoint_with_ocr() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/markdown", "scan_en.pdf").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["ocr"]["status"], "ocr");
    assert!(text(&j).contains("Invoice 4471"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn images_go_straight_to_ocr() {
    let (app, _) = app_with(&[]).await;
    for (file, format, lang, must_contain) in [
        ("scan_en.png", "png", "en", &["Invoice 4471", "Payment terms"][..]),
        // Stored sideways with EXIF orientation 6: only readable once rotated.
        ("scan_en_exif.jpg", "jpeg", "en", &["Invoice 4471", "Payment terms"][..]),
        ("scan_ar.webp", "webp", "ar", &["مرحبا بالعالم", "شكرا"][..]),
        ("scan_hi.jp2", "jp2", "hi", &["भारत", "लोग"][..]),
        // Black text on transparent pixels that are stored black.
        ("transparent.png", "png", "en", &["Transparent label 8812"][..]),
        ("line.gif", "gif", "en", &["Format check 3141"][..]),
        ("line.bmp", "bmp", "en", &["Format check 3141"][..]),
        // TGA has no signature: the filename names it.
        ("line.tga", "tga", "en", &["Format check 3141"][..]),
    ] {
        let uri = format!("/v1/extract/text?filename={file}");
        let (status, j) = extract(&app, &uri, file).await;
        assert_eq!(status, StatusCode::OK, "{file}: {j}");
        eprintln!("--- {file}: lang={} source={}\n{}", j["ocr"]["lang"], j["ocr"]["lang_source"], text(&j));
        assert_eq!(j["kind"], "image", "{file}");
        assert_eq!(j["format"], format, "{file}");
        assert_eq!(j["ocr"]["status"], "ocr", "{file}");
        assert_eq!(j["ocr"]["reasons"][0]["reason"], "image", "{file}");
        assert_eq!(j["ocr"]["lang"], lang, "{file}");
        for m in must_contain {
            assert!(text(&j).contains(m), "{file}: expected {m:?} in {:?}", text(&j));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_page_tiff() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "scan_pages.tif").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    eprintln!("--- tiff: {}\n{}", j["ocr"], text(&j));
    assert_eq!(j["page_count"], 2);
    assert_eq!(j["ocr"]["pages"], json!([1, 2]));
    assert!(text(&j).contains("Invoice 4471") && text(&j).contains("tiff-beta"), "{:?}", text(&j));

    let (_, j) = extract(&app, "/v1/extract/text?pages=2", "scan_pages.tif").await;
    assert_eq!(j["pages_extracted"], json!([2]));
    assert!(text(&j).contains("tiff-beta") && !text(&j).contains("Invoice 4471"), "{:?}", text(&j));

    let (status, _) = extract(&app, "/v1/extract/text?pages=3", "scan_pages.tif").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_ocr_off_and_unreadable_pixels() {
    let (app, _) = app_with(&[]).await;
    let (status, j) = extract(&app, "/v1/extract/text?ocr=off", "scan_en.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["ocr"]["status"], "off");
    assert_eq!(text(&j), "");

    // A valid header with the pixel data cut off: the probe passes, the
    // decode in the OCR worker fails, and an image has nothing to fall back on.
    let mut png = fixture("scan_en.png");
    png.truncate(200);
    let req = Request::post("/v1/extract/text")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .body(Body::from(png))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["type"], "/problems/corrupt-document", "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_scans_all_progress() {
    let (app, _) = app_with(&[]).await;
    let files = ["scan_en.pdf", "scan_hi.pdf", "scan_ar.pdf", "hybrid.pdf"];
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..8 {
        let app = app.clone();
        handles.push(tokio::spawn(async move { extract(&app, "/v1/extract/text", files[i % files.len()]).await }));
    }
    for h in handles {
        let (status, j) = h.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{j}");
        assert_eq!(j["incomplete"], false);
    }
    eprintln!("8 concurrent OCR requests: {:?}", start.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_models_mean_unavailable_but_healthy() {
    let (app, st) = app_with(&[("OXIDE_API_OCR_MODEL_DIR", "/nonexistent")]).await;
    let (status, j) = extract(&app, "/v1/extract/text", "scan_en.pdf").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["ocr"]["status"], "unavailable");
    assert_eq!(j["ocr"]["pages_needing_ocr"], json!([1]));
    let (status, j) = extract(&app, "/v1/extract/text", "native.pdf").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&j).contains("page-one-alpha"));
    assert!(st.health.is_ready());
    assert_eq!(st.ocr.as_ref().unwrap().status_json()["status"], "unavailable");
}

/// A deliberately broken dictionary marks that language (and only that
/// language) unavailable, by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_dict_marks_language_unavailable() {
    let src = model_dir();
    let tmp = std::env::temp_dir().join(format!("oxide-models-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for f in std::fs::read_dir(&src).unwrap() {
        let f = f.unwrap().path();
        let name = f.file_name().unwrap();
        std::fs::copy(&f, tmp.join(name)).unwrap();
    }
    // Same length, wrong characters: the model's class indices now map to
    // the wrong glyphs, so the golden text can't match.
    let dict = std::fs::read_to_string(src.join("devanagari_dict.txt")).unwrap();
    let broken: String = dict.lines().map(|_| "x\n").collect();
    std::fs::write(tmp.join("devanagari_dict.txt"), broken).unwrap();

    let dir = tmp.to_string_lossy().into_owned();
    let (app, st) = app_with(&[("OXIDE_API_OCR_MODEL_DIR", &dir)]).await;
    let status = st.ocr.as_ref().unwrap().status_json();
    eprintln!("{status}");
    assert_eq!(status["languages"]["en"], "available");
    assert_eq!(status["languages"]["ar"], "available");
    assert_eq!(status["languages"]["ta"], "available");
    assert_eq!(status["languages"]["hi"]["status"], "unavailable");
    let (_, j) = extract(&app, "/v1/extract/text?lang=hi", "scan_hi.pdf").await;
    assert_eq!(j["ocr"]["status"], "unavailable");
    let _ = std::fs::remove_dir_all(Path::new(&tmp));
}

/// Debug aid: `cargo test --features ocr --test ocr dump_crops -- --ignored`
/// writes the detected line crops and word segments to target/ocr-crops.
#[test]
#[ignore]
fn dump_crops() {
    use oxide_api::ocr::{page, stack};
    use oxide_api::pdf::lang::Lang;
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/ocr-crops");
    std::fs::create_dir_all(&out).unwrap();
    let st = stack::build(&model_dir(), &[Lang::Hi], 2).stack.unwrap();
    let doc = pdf_oxide::PdfDocument::from_bytes(fixture("scan_hi.pdf")).unwrap();
    let img = page::page_image(&doc, 0, false, false).unwrap();
    for (i, b) in page::detect(&st, &img).unwrap().iter().enumerate() {
        let crop = pdf_oxide::ocr::crop_text_region(&img, &b.polygon).unwrap();
        crop.save(out.join(format!("line{i}.png"))).unwrap();
        for (j, (x0, x1)) in page::word_segments(&crop, 0.11).into_iter().enumerate() {
            let w = crop.crop_imm(x0, 0, x1 - x0, crop.height());
            let r = st.recognizers[&Lang::Hi].recognize(&w).unwrap();
            eprintln!("line {i} word {j}: {}x{} → {:?} ({:.2})", w.width(), w.height(), r.text, r.confidence);
            w.save(out.join(format!("line{i}_w{j}.png"))).unwrap();
        }
    }
}

/// Evaluation aid: compares crop/segmentation strategies on labelled line
/// images (`OCR_EVAL_DIR` with `<lang>_<n>.png` + `.txt`).
/// `cargo test --features ocr --test ocr eval_strategies -- --ignored --nocapture`
#[test]
#[ignore]
fn eval_strategies() {
    use oxide_api::ocr::smoke::similarity;
    use oxide_api::ocr::{page, stack};
    use oxide_api::pdf::lang::Lang;
    let dir = PathBuf::from(std::env::var("OCR_EVAL_DIR").expect("OCR_EVAL_DIR"));
    let st = stack::build(&model_dir(), &[Lang::Hi, Lang::Ar], 4).stack.unwrap();
    // Space-aware similarity: like `similarity`, but spaces count.
    let spaced = |a: &str, b: &str| similarity(&a.replace(' ', "_"), &b.replace(' ', "_"));
    for (lang, gap) in [(Lang::Hi, 0.11f32), (Lang::Ar, 0.25)] {
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "png") && p.file_name().unwrap().to_string_lossy().starts_with(lang.code()))
            .collect();
        files.sort();
        // split? → (char sim sum, spaced sim sum, exact)
        let variants = [false, true];
        let mut score = [(0.0f32, 0.0f32, 0usize); 2];
        for f in &files {
            let truth = std::fs::read_to_string(f.with_extension("txt")).unwrap();
            let img = image::open(f).unwrap();
            let boxes = page::detect(&st, &img).unwrap();
            for (vi, &split) in variants.iter().enumerate() {
                let mut lines = Vec::new();
                for b in &boxes {
                    let crop = pdf_oxide::ocr::crop_text_region(&img, &b.polygon).unwrap();
                    let segs = if split { page::word_segments(&crop, gap) } else { vec![] };
                    let pieces: Vec<_> = if segs.len() > 1 {
                        segs.iter().map(|&(a, z)| crop.crop_imm(a, 0, z - a, crop.height())).collect()
                    } else {
                        vec![crop]
                    };
                    let mut words: Vec<String> = pieces
                        .iter()
                        .filter_map(|p| st.recognizers[&lang].recognize(p).ok())
                        .filter(|r| !r.text.trim().is_empty())
                        .map(|r| page::postprocess(lang, r.text.trim()))
                        .collect();
                    if lang == Lang::Ar {
                        words.reverse();
                    }
                    lines.push(words.join(" "));
                }
                let got = lines.join(" ");
                score[vi].0 += similarity(&got, &truth);
                score[vi].1 += spaced(&got, &truth);
                score[vi].2 += usize::from(got == truth);
            }
        }
        let n = files.len() as f32;
        for (vi, &split) in variants.iter().enumerate() {
            eprintln!(
                "{} split={split:<5}  char={:.3}  spaced={:.3}  exact={}/{}",
                lang.code(),
                score[vi].0 / n,
                score[vi].1 / n,
                score[vi].2,
                files.len()
            );
        }
    }
}

/// Fields from a scanned PDF. That the page is OCR'd once for all four fields
/// is by construction (`read_document` runs once per request, then every field
/// runs over its text), not counted here: the OCR metrics are process-wide, and
/// these tests run in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fields_read_from_a_scanned_pdf() {
    let (app, _) = app_with(&[]).await;
    let fields = json!([
        {"name": "invoice_number", "patterns": [r"(?i)invoice\s+(\d+)"], "required": true},
        {"name": "total", "patterns": [r"(?i)total amount due:?\s*([\d,]+\.\d{2})"]},
        {"name": "terms", "patterns": [r"(?i)payment terms:?\s*([^\n]+)"]},
        {"name": "iban", "patterns": [r"IBAN:\s*(\S+)"], "required": true},
    ]);
    let body = json!({
        "file_base64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, fixture("scan_en.pdf")),
        "fields": fields,
        "include_text": true,
    });
    let req = Request::post("/v1/extract/fields")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    eprintln!("--- fields: {}\n{}", j["fields"], text(&j));
    assert_eq!(j["kind"], "pdf");
    assert_eq!(j["ocr"]["status"], "ocr");
    assert_eq!(j["ocr"]["pages"], json!([1]));
    assert_eq!(j["fields"]["invoice_number"], "4471");
    assert_eq!(j["fields"]["total"], "1,250.00");
    assert!(j["fields"]["terms"].as_str().unwrap().to_lowercase().contains("thirty"), "{}", j["fields"]);
    assert_eq!(j["fields"]["iban"], Value::Null);
    assert_eq!(j["missing_required"], json!(["iban"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_scans_have_page_boxes_and_real_recognizer_confidence() {
    let (app, _) = app_with(&[]).await;
    for (file, lang, marker) in [
        ("scan_en.pdf", "en", "Invoice 4471"),
        ("scan_hi.pdf", "hi", "भारत"),
        ("scan_ar.pdf", "ar", "مرحبا"),
        ("scan_ta.pdf", "ta", "தமிழ்நாடு"),
    ] {
        let (status, j) = extract(&app, &format!("/v1/extract/document?lang={lang}"), file).await;
        assert_eq!(status, StatusCode::OK, "{j}");
        assert_eq!(j["ocr"]["status"], "ocr", "{j}");
        let page = &j["pages"][0];
        assert!(page["text"].as_str().unwrap().contains(marker), "{page}");
        let blocks = page["blocks"].as_array().unwrap();
        assert!(!blocks.is_empty());
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(block["source"], "ocr");
            assert_eq!(block["reading_order"], i);
            let confidence = block["confidence"].as_f64().expect("PaddleOCR provides recognition confidence");
            assert!((0.0..=1.0).contains(&confidence));
            let bbox = &block["bbox"];
            assert!(bbox["x"].as_f64().unwrap() >= 0.0);
            assert!(bbox["y"].as_f64().unwrap() >= 0.0);
            assert!(bbox["x"].as_f64().unwrap() + bbox["width"].as_f64().unwrap() <= page["width"].as_f64().unwrap() + 0.1);
            assert!(bbox["y"].as_f64().unwrap() + bbox["height"].as_f64().unwrap() <= page["height"].as_f64().unwrap() + 0.1);
        }
        assert_eq!(page["text"], blocks.iter().map(|b| b["text"].as_str().unwrap()).collect::<Vec<_>>().join("\n"));
    }
    let (status, j) = extract(&app, "/v1/extract/document?pages=1&ocr=force&lang=en", "native.pdf").await;
    assert_eq!(status, StatusCode::OK, "{j}");
    assert_eq!(j["ocr"]["status"], "ocr");
    assert!(j["pages"][0]["text"].as_str().unwrap().contains("page-one-alpha"), "{j}");
    assert!(j["pages"][0]["blocks"].as_array().unwrap().iter().all(|b| b["source"] == "ocr"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_mixed_pdf_reuses_native_text_and_merges_stamp_without_duplicates() {
    let (app, _) = app_with(&[]).await;
    let mut editor = pdf_oxide::editor::DocumentEditor::from_bytes(fixture("native.pdf")).unwrap();
    editor.merge_from_bytes(&fixture("scan_en.pdf")).unwrap();
    editor.merge_from_bytes(&fixture("hybrid.pdf")).unwrap();
    let req = Request::post("/v1/extract/document?lang=en")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .body(Body::from(editor.save_to_bytes().unwrap())).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(j["page_count"], 5);
    assert_eq!(j["pages_extracted"], json!([1, 2, 3, 4, 5]));
    assert_eq!(j["ocr"]["status"], "hybrid");
    assert_eq!(j["ocr"]["pages"], json!([4, 5]));
    for page in j["pages"].as_array().unwrap().iter().take(3) {
        assert!(page["blocks"].as_array().unwrap().iter().all(|b| b["source"] == "native" && b["confidence"].is_null()));
    }
    assert!(j["pages"][3]["text"].as_str().unwrap().contains("Invoice 4471"));
    let hybrid = &j["pages"][4];
    let text = hybrid["text"].as_str().unwrap();
    assert!(text.contains("hybrid-native"), "{text}");
    assert!(text.contains("APPROVED BY FINANCE"), "{text}");
    assert_eq!(text.matches("Purchase order 5521").count(), 1, "{text}");
    let blocks = hybrid["blocks"].as_array().unwrap();
    assert!(blocks.iter().any(|b| b["source"] == "native"));
    assert!(blocks.iter().any(|b| b["source"] == "ocr"));
}
