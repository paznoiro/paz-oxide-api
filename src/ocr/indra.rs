//! Indra's pinned CLI, invoked without a shell. Selected pages are bounded
//! raster inputs; each invocation uses one core and at most BATCH_PAGES images.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use pdf_oxide::geometry::Rect;
use pdf_oxide::rendering::{RenderOptions, render_page};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::pdf::PageSource;
use crate::pdf::document::{Block, PageFrame};

pub const BATCH_PAGES: usize = 4;
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

pub struct IndraService {
    pub slots: Arc<Semaphore>,
    cli: PathBuf,
    models: PathBuf,
    math: bool,
}

pub struct PreparedPage {
    pub index: usize,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frame: Option<PageFrame>,
}

pub struct IndraPage {
    pub text: String,
    pub markdown: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    Unavailable,
    Failed,
    Timeout,
}

impl IndraService {
    pub fn new(cfg: &Config) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(cfg.indra_max_concurrent)),
            cli: cfg.indra_cli.clone(),
            models: cfg.indra_model_dir.clone(),
            math: cfg.indra_math,
        }
    }

    pub fn available(&self) -> bool {
        let executable = if self.cli.components().count() > 1 {
            self.cli.is_file()
        } else {
            std::env::var_os("PATH").is_some_and(|paths| {
                std::env::split_paths(&paths).any(|p| p.join(&self.cli).is_file())
            })
        };
        executable
            && ["det.onnx", "rec.onnx", "order.onnx", "meta.json"]
                .iter()
                .all(|f| self.models.join(f).is_file())
            && (!self.math
                || [
                    "mathcls.onnx",
                    "math_enc.onnx",
                    "math_dec_init.onnx",
                    "math_dec_step.onnx",
                    "math_meta.json",
                ]
                .iter()
                .all(|f| self.models.join(f).is_file()))
    }

    pub async fn run(
        &self,
        pages: &[PreparedPage],
        deadline: Instant,
    ) -> Result<Vec<Value>, CliError> {
        let mut cmd = Command::new(&self.cli);
        cmd.args([
            "--format",
            "json",
            "--mode",
            "ocr",
            "--cores",
            "1",
            "--keep-furniture",
            "--model-dir",
        ])
        .arg(&self.models)
        .args(pages.iter().map(|p| &p.path))
        .env("OMP_NUM_THREADS", "1")
        .env("OPENBLAS_NUM_THREADS", "1")
        .env("MKL_NUM_THREADS", "1")
        .env("HF_HUB_OFFLINE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Hub/native diagnostics may contain paths or secrets. Return only
        // validated stdout and static error messages, never stderr.
        .stderr(Stdio::null())
        .kill_on_drop(true);
        if self.math {
            cmd.arg("--math");
        }
        let mut child = cmd.spawn().map_err(|_| CliError::Unavailable)?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or(CliError::Failed)?
            .take(MAX_OUTPUT_BYTES + 1);
        let result = tokio::time::timeout_at(deadline.into(), async {
            let mut bytes = Vec::new();
            stdout
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| CliError::Failed)?;
            if bytes.len() as u64 > MAX_OUTPUT_BYTES {
                return Err(CliError::Failed);
            }
            let status = child.wait().await.map_err(|_| CliError::Failed)?;
            if !status.success() {
                return Err(CliError::Failed);
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| CliError::Failed)?;
            let values: Result<Vec<Value>, _> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect();
            let values = values.map_err(|_| CliError::Failed)?;
            if values.len() != pages.len() {
                return Err(CliError::Failed);
            }
            Ok(values)
        })
        .await
        .unwrap_or(Err(CliError::Timeout));
        if result.is_err() {
            // --cores=1 takes Indra's serial path, so there is no worker-process
            // tree. Cancellation also kills the child through kill_on_drop.
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        result
    }
}

/// Runs on the shared CPU pool, one decoded page at a time. Holding the
/// directory here keeps cleanup safe if the HTTP request is cancelled.
pub fn prepare(
    source: &PageSource,
    index: usize,
    max_pixels: u64,
    dir: &Arc<TempDir>,
) -> Result<PreparedPage, String> {
    let (img, frame) = match source {
        PageSource::Image(doc) => (doc.decode(index)?, None),
        PageSource::Pdf(doc) => {
            let frame = PageFrame::read(doc, index).map_err(|_| "invalid page geometry")?;
            let pixels_at_dpi = |dpi: u32| {
                (f64::from(frame.width) * f64::from(dpi) / 72.0).ceil()
                    * (f64::from(frame.height) * f64::from(dpi) / 72.0).ceil()
            };
            let mut dpi = 300;
            while dpi > 1 && pixels_at_dpi(dpi) > max_pixels as f64 {
                dpi -= 1;
            }
            if pixels_at_dpi(dpi) > max_pixels as f64 {
                return Err("page exceeds rendering pixel limit".into());
            }
            let rendered = render_page(doc, index, &RenderOptions::with_dpi(dpi).as_raw())
                .map_err(|_| "page render failed")?;
            let img = image::RgbaImage::from_raw(rendered.width, rendered.height, rendered.data)
                .ok_or("invalid rendered image")?;
            (image::DynamicImage::ImageRgba8(img), Some(frame))
        }
    };
    let (width, height) = (img.width(), img.height());
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err("image exceeds pixel limit".into());
    }
    let path = dir.path().join(format!("page-{}.png", index + 1));
    img.to_luma8()
        .save(&path)
        .map_err(|_| "could not prepare OCR image")?;
    Ok(PreparedPage {
        index,
        path,
        width,
        height,
        frame,
    })
}

#[derive(Deserialize)]
struct CliPage {
    file: PathBuf,
    page: usize,
    source: String,
    rotation: i32,
    lines: Vec<CliLine>,
    markdown: Option<String>,
    #[serde(default)]
    blocks: Vec<CliBlock>,
}

#[derive(Deserialize)]
struct CliLine {
    text: String,
    #[serde(rename = "box")]
    bbox: [f32; 4],
    conf: Option<f32>,
}

#[derive(Deserialize)]
struct CliBlock {
    label: String,
    bbox: [f32; 4],
}

pub fn interpret(value: &Value, prepared: &PreparedPage) -> Result<IndraPage, CliError> {
    let page: CliPage = serde_json::from_value(value.clone()).map_err(|_| CliError::Failed)?;
    if page.file != prepared.path
        || page.page != 0
        || page.source != "ocr"
        || !matches!(page.rotation, 0 | 90 | 180 | 270)
    {
        return Err(CliError::Failed);
    }
    let text = page
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut blocks = Vec::with_capacity(page.lines.len());
    for line in page.lines {
        if !line.bbox.iter().all(|n| n.is_finite())
            || line.bbox[2] < line.bbox[0]
            || line.bbox[3] < line.bbox[1]
        {
            return Err(CliError::Failed);
        }
        let bbox = unrotate_box(line.bbox, page.rotation, prepared.width, prepared.height);
        let mut block = Block::ocr(line.text, bbox, line.conf.unwrap_or(f32::NAN));
        if let Some(frame) = &prepared.frame {
            block.bbox = frame.ocr_box(bbox, prepared.width, prepared.height);
        }
        let (cx, cy) = (
            (line.bbox[0] + line.bbox[2]) / 2.0,
            (line.bbox[1] + line.bbox[3]) / 2.0,
        );
        if let Some(layout) = page
            .blocks
            .iter()
            .find(|b| b.bbox[0] <= cx && cx <= b.bbox[2] && b.bbox[1] <= cy && cy <= b.bbox[3])
        {
            block.kind = match layout.label.as_str() {
                "Section-Header" => "heading",
                "Page-Header" => "header",
                "Page-Footer" => "footer",
                "Table" => "table",
                "Caption" => "caption",
                "Footnote" => "footnote",
                "List-Group" => "list",
                _ => "text",
            };
        }
        blocks.push(block);
    }
    Ok(IndraPage {
        markdown: page.markdown.unwrap_or_else(|| text.clone()),
        text,
        blocks,
    })
}

// In 0.3.2, upright() applies np.rot90(gray, -k): positive rotation is
// clockwise. Deskew is already reversed by Indra; undo only its quarter turn.
fn unrotate_box(b: [f32; 4], rotation: i32, width: u32, height: u32) -> Rect {
    let (w, h) = (width as f32, height as f32);
    let points =
        [(b[0], b[1]), (b[2], b[1]), (b[2], b[3]), (b[0], b[3])].map(|(x, y)| match rotation {
            90 => (y, h - x),
            180 => (w - x, h - y),
            270 => (w - y, x),
            _ => (x, y),
        });
    let x0 = points
        .iter()
        .map(|p| p.0)
        .fold(f32::INFINITY, f32::min)
        .clamp(0.0, w);
    let y0 = points
        .iter()
        .map(|p| p.1)
        .fold(f32::INFINITY, f32::min)
        .clamp(0.0, h);
    let x1 = points
        .iter()
        .map(|p| p.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .clamp(0.0, w);
    let y1 = points
        .iter()
        .map(|p| p.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .clamp(0.0, h);
    Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

/// File paths are implementation details, not document content.
pub fn debug_output(values: &[Value]) -> Vec<Value> {
    values
        .iter()
        .map(|v| {
            let mut v = v.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("file");
                obj.remove("note");
            }
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_scores_and_reverses_orientation() {
        let p = PreparedPage {
            index: 4,
            path: PathBuf::from("/tmp/page-5.png"),
            width: 100,
            height: 200,
            frame: None,
        };
        let v = json!({"file":p.path,"page":0,"source":"ocr","rotation":90,"lines":[{"text":"x = 2", "box":[10,20,30,40],"conf":0.87}],"markdown":"$x = 2$"});
        let result = interpret(&v, &p).unwrap();
        assert_eq!(result.markdown, "$x = 2$");
        let b = &result.blocks[0];
        assert_eq!(b.confidence, Some(0.87));
        let rect = b.bbox.unwrap();
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (20.0, 170.0, 20.0, 20.0)
        );
        for rotation in [0, 90, 180, 270] {
            let rect = unrotate_box([10.0, 20.0, 30.0, 40.0], rotation, 100, 200);
            assert!(
                rect.x >= 0.0
                    && rect.y >= 0.0
                    && rect.x + rect.width <= 100.0
                    && rect.y + rect.height <= 200.0
            );
        }
        let mut unknown = v.clone();
        unknown["lines"][0]["conf"] = Value::Null;
        assert_eq!(interpret(&unknown, &p).unwrap().blocks[0].confidence, None);
        for source in ["text", "hybrid", "error"] {
            let mut bad = v.clone();
            bad["source"] = json!(source);
            assert!(interpret(&bad, &p).is_err());
        }
    }
}
