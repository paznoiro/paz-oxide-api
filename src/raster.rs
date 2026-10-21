//! Images (PNG, JPEG, TIFF, WebP, GIF, BMP, JPEG 2000, PNM, TGA): format
//! sniffing, a header-only probe (dimensions, TIFF pages) on the CPU pool,
//! and page decoding for OCR. Pixels are decoded inside the OCR worker, so
//! at most one decoded image per worker is alive.

use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use serde::Serialize;

use crate::detect::FormatSource;
use crate::error::ApiError;
use crate::pdf::route::{OcrMode, OcrReason};
use crate::pdf::{NativePage, PageSource, Phase1};
use crate::text::ScriptCounts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Tiff,
    Webp,
    Gif,
    Bmp,
    Jp2,
    Pnm,
    Tga,
}

impl ImageFormat {
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Tiff => "tiff",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Jp2 => "jp2",
            Self::Pnm => "pnm",
            Self::Tga => "tga",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "png" => Self::Png,
            "jpeg" | "jpg" | "jpe" | "jfif" => Self::Jpeg,
            "tiff" | "tif" => Self::Tiff,
            "webp" => Self::Webp,
            "gif" => Self::Gif,
            "bmp" | "dib" => Self::Bmp,
            "jp2" | "j2k" | "j2c" | "jpc" | "jpx" | "jpf" => Self::Jp2,
            "pnm" | "pbm" | "pgm" | "ppm" | "pam" => Self::Pnm,
            "tga" => Self::Tga,
            _ => return None,
        })
    }

    /// The `image` codec; JPEG 2000 has its own decoder.
    fn codec(self) -> Option<image::ImageFormat> {
        Some(match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Tiff => image::ImageFormat::Tiff,
            Self::Webp => image::ImageFormat::WebP,
            Self::Gif => image::ImageFormat::Gif,
            Self::Bmp => image::ImageFormat::Bmp,
            Self::Pnm => image::ImageFormat::Pnm,
            Self::Tga => image::ImageFormat::Tga,
            Self::Jp2 => return None,
        })
    }
}

/// Magic bytes. TGA has none; it is recognized by `format` or the filename.
pub fn sniff(b: &[u8]) -> Option<ImageFormat> {
    let at = |off: usize, magic: &[u8]| b.get(off..off + magic.len()) == Some(magic);
    Some(if at(0, b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if at(0, &[0xFF, 0xD8, 0xFF]) {
        ImageFormat::Jpeg
    } else if at(0, b"II*\0") || at(0, b"MM\0*") || at(0, b"II+\0") || at(0, b"MM\0+") {
        ImageFormat::Tiff
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        ImageFormat::Webp
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        ImageFormat::Gif
    } else if at(0, b"\0\0\0\x0cjP  \r\n\x87\n") || at(0, &[0xFF, 0x4F, 0xFF, 0x51]) {
        ImageFormat::Jp2
    } else if at(0, b"BM")
        && b.get(14..18).is_some_and(|h| matches!(u32::from_le_bytes([h[0], h[1], h[2], h[3]]), 12 | 40 | 52 | 56 | 64 | 108 | 124))
    {
        ImageFormat::Bmp
    } else if b.len() > 2 && b[0] == b'P' && (b'1'..=b'7').contains(&b[1]) && b[2].is_ascii_whitespace() {
        ImageFormat::Pnm
    } else {
        return None;
    })
}

/// Image formats recognized only to name them in the error: there is no
/// pure-Rust decoder for them in the image.
pub fn unsupported(b: &[u8]) -> Option<&'static str> {
    let brand = (b.get(4..8) == Some(b"ftyp")).then(|| b.get(8..12)).flatten();
    match brand {
        Some(b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1" | b"msf1") => Some("HEIC/HEIF"),
        Some(b"avif" | b"avis") => Some("AVIF"),
        _ if b.starts_with(&[0xFF, 0x0A]) || b.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") => Some("JPEG XL"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ImagePage {
    pub width: u32,
    pub height: u32,
    /// The TIFF directory holding this page.
    #[serde(skip)]
    ifd: Option<u64>,
}

/// A probed image: validated dimensions, not yet decoded.
pub struct ImageDoc {
    bytes: Bytes,
    pub format: ImageFormat,
    pub pages: Vec<ImagePage>,
    max_pixels: u64,
}

fn corrupt(e: impl std::fmt::Display) -> ApiError {
    ApiError::unprocessable("corrupt-document", "Unreadable document", format!("the image could not be read: {e}"))
}

/// Reads headers only: dimensions, and every page of a TIFF. Rejects
/// images over `max_pixels` before anything is decoded.
pub fn probe(bytes: Bytes, format: ImageFormat, max_pixels: u64, max_pages: usize) -> Result<ImageDoc, ApiError> {
    let pages = match format.codec() {
        None => {
            let img = hayro_jpeg2000::Image::new(&bytes, &hayro_jpeg2000::DecodeSettings::default()).map_err(corrupt)?;
            vec![ImagePage { width: img.width(), height: img.height(), ifd: None }]
        }
        Some(_) if format == ImageFormat::Tiff => tiff_pages(&bytes, max_pages)?,
        Some(codec) => {
            let (width, height) = ImageReader::with_format(Cursor::new(&bytes[..]), codec).into_dimensions().map_err(corrupt)?;
            vec![ImagePage { width, height, ifd: None }]
        }
    };
    for (i, p) in pages.iter().enumerate() {
        let px = u64::from(p.width) * u64::from(p.height);
        if px == 0 {
            return Err(corrupt(format!("page {} has no pixels", i + 1)));
        }
        if px > max_pixels {
            return Err(ApiError::unprocessable(
                "image-too-large",
                "Image too large",
                format!("page {} is {}x{} pixels; the limit is {max_pixels} pixels", i + 1, p.width, p.height),
            )
            .with("limit_pixels", max_pixels));
        }
    }
    Ok(ImageDoc { bytes, format, pages, max_pixels })
}

/// Phase 1 for an image (CPU pool): probe it and route every selected page
/// to OCR. There is no native text.
pub fn phase1(
    bytes: Bytes,
    format: ImageFormat,
    pages: Option<&[usize]>,
    ocr: OcrMode,
    max_pixels: u64,
    max_pages: usize,
) -> Result<Phase1, ApiError> {
    let t = Instant::now();
    let doc = probe(bytes, format, max_pixels, max_pages)?;
    let page_count = doc.page_count();
    let selected = crate::pdf::select_pages(pages, page_count)?;
    let reason = (ocr != OcrMode::Off).then_some(OcrReason::Image);
    Ok(Phase1 {
        source: PageSource::Image(Arc::new(doc)),
        page_count,
        pages: selected.into_iter().map(|index| NativePage { index, text: String::new(), ocr: reason, failed: false, document: None }).collect(),
        incomplete: false,
        truncated: false,
        legacy_font: false,
        script_counts: ScriptCounts::default(),
        parse_time: t.elapsed(),
        extract_time: Duration::ZERO,
    })
}

/// Every full-resolution page of a TIFF (reduced-resolution copies and
/// transparency masks skipped), with the directory it lives in.
fn tiff_pages(bytes: &[u8], max_pages: usize) -> Result<Vec<ImagePage>, ApiError> {
    let mut d = tiff::decoder::Decoder::new(Cursor::new(bytes)).map_err(corrupt)?;
    let mut pages = Vec::new();
    loop {
        let subfile = d.find_tag_unsigned::<u32>(tiff::tags::Tag::NewSubfileType).ok().flatten().unwrap_or(0);
        if subfile & 0b101 == 0 {
            let (width, height) = d.dimensions().map_err(corrupt)?;
            pages.push(ImagePage { width, height, ifd: d.ifd_pointer().map(|p| p.0) });
            if pages.len() > max_pages {
                return Err(ApiError::unprocessable(
                    "too-many-pages",
                    "Too many pages",
                    format!("the TIFF has more than {max_pages} pages; the limit is {max_pages}"),
                )
                .with("limit", max_pages));
            }
        }
        // A directory that doesn't parse ends the page list.
        if !d.more_images() || d.next_image().is_err() {
            break;
        }
    }
    if pages.is_empty() {
        return Err(corrupt("the TIFF holds no full-resolution image"));
    }
    Ok(pages)
}

impl ImageDoc {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Decodes one page for OCR, upright (EXIF/TIFF orientation applied)
    /// and with any transparency composited onto white. Errors describe the
    /// failure, never the content.
    pub fn decode(&self, page: usize) -> Result<DynamicImage, String> {
        let p = self.pages.get(page).ok_or("page out of range")?;
        let img = match self.format.codec() {
            None => decode_jp2(&self.bytes)?,
            Some(codec) => match p.ifd.filter(|&ifd| Some(ifd) != first_ifd(&self.bytes)) {
                // image's TIFF decoder reads the first directory only; point
                // a copy's header at this page's directory instead.
                Some(ifd) => {
                    let copy = retarget_tiff(&self.bytes, ifd).ok_or("TIFF page could not be addressed")?;
                    decode_with(&copy, codec, self.max_pixels)?
                }
                None => decode_with(&self.bytes, codec, self.max_pixels)?,
            },
        };
        Ok(flatten_alpha(img))
    }
}

fn decode_with(bytes: &[u8], codec: image::ImageFormat, max_pixels: u64) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), codec);
    let mut limits = Limits::default();
    // 16-bit RGBA is 8 bytes per pixel; leave room for decoder scratch.
    limits.max_alloc = Some(max_pixels.saturating_mul(12).max(64 << 20));
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| format!("image decode failed: {e}"))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| format!("image decode failed: {e}"))?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn decode_jp2(bytes: &[u8]) -> Result<DynamicImage, String> {
    let fail = |e: hayro_jpeg2000::DecodeError| format!("image decode failed: {e}");
    let img = hayro_jpeg2000::Image::new(bytes, &hayro_jpeg2000::DecodeSettings::default()).map_err(fail)?;
    let (w, h, alpha) = (img.width(), img.height(), img.has_alpha());
    let channels = img.color_space().num_channels();
    let mut ctx = hayro_jpeg2000::DecoderContext::default();
    let data = img.decode(&mut ctx).map_err(fail)?.data_u8();
    let malformed = || "JPEG 2000 decoder returned a malformed buffer".to_string();
    Ok(match (channels, alpha) {
        (1, false) => DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, data).ok_or_else(malformed)?),
        (1, true) => DynamicImage::ImageLumaA8(image::GrayAlphaImage::from_raw(w, h, data).ok_or_else(malformed)?),
        (3, false) => DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, data).ok_or_else(malformed)?),
        (3, true) => DynamicImage::ImageRgba8(image::RgbaImage::from_raw(w, h, data).ok_or_else(malformed)?),
        (4, _) => {
            // CMYK(+alpha) → RGB, naive (no ICC): enough for text.
            let step = 4 + usize::from(alpha);
            if data.len() != w as usize * h as usize * step {
                return Err(malformed());
            }
            let rgb = data
                .chunks_exact(step)
                .flat_map(|p| {
                    let k = 255 - u16::from(p[3]);
                    [0, 1, 2].map(|i| ((255 - u16::from(p[i])) * k / 255) as u8)
                })
                .collect();
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, rgb).ok_or_else(malformed)?)
        }
        _ => return Err(format!("unsupported JPEG 2000 layout ({channels} channels)")),
    })
}

/// Transparent pixels are often stored black, which would hide black text
/// from the detector: composite onto white.
fn flatten_alpha(img: DynamicImage) -> DynamicImage {
    if !img.color().has_alpha() {
        return img;
    }
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let mut rgb = image::RgbImage::new(w, h);
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let a = u32::from(src[3]);
        for i in 0..3 {
            dst[i] = ((u32::from(src[i]) * a + 255 * (255 - a) + 127) / 255) as u8;
        }
    }
    DynamicImage::ImageRgb8(rgb)
}

fn first_ifd(b: &[u8]) -> Option<u64> {
    let le = match b.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) });
    match u16_at(2)? {
        42 => {
            let s: [u8; 4] = b.get(4..8)?.try_into().ok()?;
            Some(u64::from(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) }))
        }
        43 => {
            let s: [u8; 8] = b.get(8..16)?.try_into().ok()?;
            Some(if le { u64::from_le_bytes(s) } else { u64::from_be_bytes(s) })
        }
        _ => None,
    }
}

/// A copy of a TIFF whose header points at directory `ifd`. Every other
/// offset in a TIFF is absolute, so the rest of the file stays valid.
fn retarget_tiff(b: &[u8], ifd: u64) -> Option<Vec<u8>> {
    let le = b.starts_with(b"II");
    let classic = first_ifd(b).is_some() && b.get(2..4).is_some_and(|v| v == [42, 0] || v == [0, 42]);
    let mut v = b.to_vec();
    if classic {
        let o = u32::try_from(ifd).ok()?;
        v[4..8].copy_from_slice(&if le { o.to_le_bytes() } else { o.to_be_bytes() });
    } else {
        first_ifd(b)?;
        v[8..16].copy_from_slice(&if le { ifd.to_le_bytes() } else { ifd.to_be_bytes() });
    }
    Some(v)
}

// ---------------------------------------------------------------------------
// /v1/info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ImageInfo {
    pub kind: &'static str,
    pub format: &'static str,
    pub format_source: FormatSource,
    pub page_count: usize,
    pub pages: Vec<PageInfo>,
}

#[derive(Serialize)]
pub struct PageInfo {
    pub page: usize,
    pub width: u32,
    pub height: u32,
}

pub fn info(doc: &ImageDoc, source: FormatSource) -> ImageInfo {
    ImageInfo {
        kind: "image",
        format: doc.format.name(),
        format_source: source,
        page_count: doc.page_count(),
        pages: doc.pages.iter().enumerate().map(|(i, p)| PageInfo { page: i + 1, width: p.width, height: p.height }).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(img: &DynamicImage, fmt: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, fmt).unwrap();
        out.into_inner()
    }

    #[test]
    fn sniffing() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(4, 3));
        for (fmt, want) in [
            (image::ImageFormat::Png, ImageFormat::Png),
            (image::ImageFormat::Jpeg, ImageFormat::Jpeg),
            (image::ImageFormat::Tiff, ImageFormat::Tiff),
            (image::ImageFormat::Gif, ImageFormat::Gif),
            (image::ImageFormat::Bmp, ImageFormat::Bmp),
            (image::ImageFormat::Pnm, ImageFormat::Pnm),
            (image::ImageFormat::WebP, ImageFormat::Webp),
        ] {
            assert_eq!(sniff(&encode(&img, fmt)), Some(want), "{fmt:?}");
        }
        assert_eq!(sniff(b"BM-not-a-bitmap-header"), None);
        assert_eq!(sniff(b"Plain text"), None);
        assert_eq!(unsupported(b"\0\0\0\x18ftypheic\0\0\0\0"), Some("HEIC/HEIF"));
        assert_eq!(unsupported(b"\0\0\0\x1cftypavif\0\0\0\0"), Some("AVIF"));
        assert_eq!(unsupported(b"\0\0\0\x18ftypisom"), None);
    }

    #[test]
    fn probe_limits_and_dimensions() {
        let png = encode(&DynamicImage::ImageLuma8(image::GrayImage::new(300, 200)), image::ImageFormat::Png);
        let doc = probe(Bytes::from(png.clone()), ImageFormat::Png, 1_000_000, 10).unwrap();
        assert_eq!((doc.page_count(), doc.pages[0].width, doc.pages[0].height), (1, 300, 200));
        let err = probe(Bytes::from(png), ImageFormat::Png, 50_000, 10).err().unwrap();
        assert_eq!(err.slug, "image-too-large");
        assert_eq!(probe(Bytes::from_static(b"\x89PNG\r\n\x1a\nbroken"), ImageFormat::Png, 1 << 20, 10).err().unwrap().slug, "corrupt-document");
    }

    #[test]
    fn alpha_is_flattened_onto_white() {
        // Black text pixel, and a fully transparent pixel stored as black.
        let mut rgba = image::RgbaImage::new(2, 1);
        rgba.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        rgba.put_pixel(1, 0, image::Rgba([0, 0, 0, 0]));
        let png = encode(&DynamicImage::ImageRgba8(rgba), image::ImageFormat::Png);
        let doc = probe(Bytes::from(png), ImageFormat::Png, 1 << 20, 10).unwrap();
        let rgb = doc.decode(0).unwrap().into_rgb8();
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn tiff_header_retargeting() {
        let tif = encode(&DynamicImage::ImageLuma8(image::GrayImage::new(5, 7)), image::ImageFormat::Tiff);
        let first = first_ifd(&tif).unwrap();
        let moved = retarget_tiff(&tif, first).unwrap();
        assert_eq!(moved, tif, "pointing at the same directory changes nothing");
        assert_eq!(first_ifd(&retarget_tiff(&tif, 1234).unwrap()), Some(1234));
    }
}
