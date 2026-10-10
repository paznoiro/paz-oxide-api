//! Request input: multipart (`file=<binary>`), JSON (`{"file_base64": …}`),
//! or a raw body. Params are top-level multipart parts, JSON keys, or query
//! params — never nested. Body params override query params.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use serde_json::Value;

use crate::error::ApiError;

pub type Params = BTreeMap<String, String>;

pub struct Input {
    pub bytes: Bytes,
    pub filename: Option<String>,
    pub params: Params,
}

const MAX_PARAM_LEN: usize = 4096;

/// Reads the whole input. The caller wraps this in the body timeout.
pub async fn read_input(req: Request, max_body: usize) -> Result<Input, ApiError> {
    let content_length = content_length(req.headers());
    if let Some(len) = content_length
        && len > max_body
    {
        return Err(ApiError::too_large(max_body));
    }

    let mut params = parse_query(req.uri().query())?;
    let ctype = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = ctype.split(';').next().unwrap_or("").trim().to_string();

    let (bytes, filename) = match mime.as_str() {
        "multipart/form-data" => read_multipart(req, max_body, content_length, &mut params).await?,
        "application/json" => read_json(req, max_body, content_length, &mut params).await?,
        _ => {
            let disposition = filename_from_disposition(req.headers());
            let bytes = read_body(req.into_body(), max_body, content_length).await?;
            (bytes, disposition)
        }
    };
    if bytes.is_empty() {
        return Err(ApiError::bad_request(
            "no document received: send multipart `file`, JSON `file_base64`, or the raw bytes as the body",
        ));
    }
    if bytes.len() > max_body {
        return Err(ApiError::too_large(max_body));
    }
    let filename = filename.or_else(|| params.get("filename").cloned());
    Ok(Input { bytes, filename, params })
}

fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// Initial buffer size when the client sends no `Content-Length`.
const UNSIZED_START: usize = 256 * 1024;

/// Reads a body into one buffer, pre-sized from `Content-Length` so a
/// 32 MiB upload costs 32 MiB (collecting chunks and then concatenating
/// them would briefly hold two copies).
async fn read_body(mut body: Body, max_body: usize, size_hint: Option<usize>) -> Result<Bytes, ApiError> {
    let mut buf = BytesMut::with_capacity(size_hint.unwrap_or(UNSIZED_START).min(max_body));
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ApiError::bad_request("the request body could not be read"))?;
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > max_body {
                return Err(ApiError::too_large(max_body));
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(buf.freeze())
}

/// Consumes and discards a rejected request's body in the background, so
/// the early response (401/429/503) reaches a client that is still
/// uploading instead of being lost to a connection reset. Nothing is
/// buffered; draining stops after `max_body` bytes or `timeout`.
pub fn drain_in_background(mut body: Body, max_body: usize, timeout: Duration) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(timeout, async move {
            let mut seen = 0usize;
            while let Some(Ok(frame)) = body.frame().await {
                if let Some(d) = frame.data_ref() {
                    seen += d.len();
                    if seen > max_body {
                        break;
                    }
                }
            }
        })
        .await;
    });
}

fn parse_query(q: Option<&str>) -> Result<Params, ApiError> {
    let mut out = Params::new();
    let Some(q) = q else { return Ok(out) };
    let pairs: Vec<(String, String)> = form_pairs(q)?;
    for (k, v) in pairs {
        insert_param(&mut out, k, v)?;
    }
    Ok(out)
}

/// Minimal `application/x-www-form-urlencoded` decoding for query strings.
fn form_pairs(q: &str) -> Result<Vec<(String, String)>, ApiError> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((percent_decode(k)?, percent_decode(v)?))
        })
        .collect()
}

fn percent_decode(s: &str) -> Result<String, ApiError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| ApiError::bad_request("malformed percent-encoding in query string"))?;
                out.push(hex);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| ApiError::bad_request("query string is not valid UTF-8"))
}

fn insert_param(params: &mut Params, key: String, value: String) -> Result<(), ApiError> {
    if key.len() > 64 || value.len() > MAX_PARAM_LEN {
        return Err(ApiError::bad_request("parameter name or value too long"));
    }
    params.insert(key, value);
    Ok(())
}

async fn read_multipart(
    req: Request,
    max_body: usize,
    size_hint: Option<usize>,
    params: &mut Params,
) -> Result<(Bytes, Option<String>), ApiError> {
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| ApiError::bad_request("malformed multipart body (missing boundary?)"))?;
    let mut file: Option<(Bytes, Option<String>)> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(multipart_error(e.status(), max_body)),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            if file.is_some() {
                return Err(ApiError::bad_request("multipart body has more than one `file` part"));
            }
            let fname = field.file_name().map(str::to_string);
            // The request's Content-Length bounds the part's size: one
            // allocation instead of a growing buffer.
            let mut field = field;
            let mut buf = BytesMut::with_capacity(size_hint.unwrap_or(UNSIZED_START).min(max_body));
            while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(e.status(), max_body))? {
                buf.extend_from_slice(&chunk);
            }
            file = Some((buf.freeze(), fname));
        } else if !name.is_empty() {
            let text = field.text().await.map_err(|e| multipart_error(e.status(), max_body))?;
            insert_param(params, name, text.trim().to_string())?;
        }
    }
    file.ok_or_else(|| ApiError::bad_request("multipart body has no `file` part"))
}

fn multipart_error(status: StatusCode, max_body: usize) -> ApiError {
    match status {
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::too_large(max_body),
        _ => ApiError::bad_request("malformed multipart body"),
    }
}

async fn read_json(
    req: Request,
    max_body: usize,
    size_hint: Option<usize>,
    params: &mut Params,
) -> Result<(Bytes, Option<String>), ApiError> {
    let raw = read_body(req.into_body(), max_body, size_hint).await?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| ApiError::bad_request("request body is not valid JSON"))?;
    drop(raw);
    let Value::Object(map) = value else {
        return Err(ApiError::bad_request("JSON body must be an object"));
    };
    let mut b64: Option<String> = None;
    for (k, v) in map {
        if k == "file_base64" {
            match v {
                Value::String(s) => b64 = Some(s),
                _ => return Err(ApiError::bad_request("`file_base64` must be a string")),
            }
            continue;
        }
        let s = match v {
            Value::String(s) => s,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Null => continue,
            Value::Array(_) | Value::Object(_) => {
                return Err(ApiError::bad_request(format!("parameter `{k}` must be a scalar; params are never nested")));
            }
        };
        insert_param(params, k, s)?;
    }
    let b64 = b64.ok_or_else(|| ApiError::bad_request("JSON body has no `file_base64`"))?;
    let bytes = decode_base64(&b64)?;
    Ok((Bytes::from(bytes), None))
}

fn decode_base64(s: &str) -> Result<Vec<u8>, ApiError> {
    // Accept `data:<mime>;base64,` prefixes and embedded whitespace/newlines.
    let s = match s.split_once(";base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => s,
    };
    let cleaned: String = if s.bytes().any(|b| b.is_ascii_whitespace()) {
        s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
    } else {
        s.to_string()
    };
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    engine
        .decode(cleaned.as_bytes())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cleaned.trim_end_matches('=').as_bytes()))
        .map_err(|_| ApiError::bad_request("`file_base64` is not valid base64"))
}

fn filename_from_disposition(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(CONTENT_DISPOSITION)?.to_str().ok()?;
    v.split(';').find_map(|part| {
        let (k, val) = part.trim().split_once('=')?;
        k.trim().eq_ignore_ascii_case("filename").then(|| val.trim().trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding() {
        let p = parse_query(Some("pages=1-3%2C5&lang=hi&x=a+b")).unwrap();
        assert_eq!(p["pages"], "1-3,5");
        assert_eq!(p["lang"], "hi");
        assert_eq!(p["x"], "a b");
        assert!(parse_query(Some("a=%zz")).is_err());
        assert!(parse_query(Some("a=%2")).is_err());
    }

    #[test]
    fn base64_variants() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("data:application/pdf;base64,aGVsbG8=").unwrap(), b"hello");
        assert!(decode_base64("!!!").is_err());
    }
}
