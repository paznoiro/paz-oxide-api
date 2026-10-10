//! Content-first format detection: magic bytes → ZIP entries / CFB streams.
//! The `format` param and the filename extension are consulted only when the
//! content is inconclusive.

use std::io::{Cursor, Read};

use serde::Serialize;

use crate::error::ApiError;
use crate::office::OfficeFormat;
use crate::raster::{self, ImageFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Office(OfficeFormat),
    Image(ImageFormat),
}

impl DocKind {
    pub fn kind_str(self) -> &'static str {
        match self {
            DocKind::Pdf => "pdf",
            DocKind::Office(_) => "office",
            DocKind::Image(_) => "image",
        }
    }

    pub fn format_str(self) -> &'static str {
        match self {
            DocKind::Pdf => "pdf",
            DocKind::Office(f) => f.extension(),
            DocKind::Image(f) => f.name(),
        }
    }

    /// The `format` param: canonical names plus common aliases (`jpg`, `tif`).
    pub fn from_name(s: &str) -> Option<Self> {
        Self::from_ext(s.trim())
    }

    fn from_extension(filename: &str) -> Option<Self> {
        Self::from_ext(filename.rsplit_once('.')?.1)
    }

    fn from_ext(ext: &str) -> Option<Self> {
        if ext.eq_ignore_ascii_case("pdf") {
            return Some(DocKind::Pdf);
        }
        OfficeFormat::from_extension(ext)
            .map(DocKind::Office)
            .or_else(|| ImageFormat::from_extension(ext).map(DocKind::Image))
    }

    /// Paged kinds take `pages`, `ocr` and `lang`.
    pub fn is_paged(self) -> bool {
        matches!(self, DocKind::Pdf | DocKind::Image(_))
    }
}

pub const ACCEPTED_FORMATS: &str =
    "pdf, docx, xlsx, pptx, doc, xls, ppt, odt, ods, odp, png, jpeg, tiff, webp, gif, bmp, jp2, pnm, tga";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatSource {
    Sniff,
    Param,
    Filename,
}

#[derive(Debug, Clone, Copy)]
pub struct Detected {
    pub kind: DocKind,
    pub source: FormatSource,
}

/// What the bytes alone say.
#[derive(Debug, PartialEq, Eq)]
enum Sniff {
    Known(DocKind),
    EncryptedOoxml,
    /// An image format with no decoder here (HEIC, AVIF, JPEG XL).
    UnsupportedImage(&'static str),
    /// A ZIP / CFB container whose contents didn't identify the format.
    Family(Family),
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
enum Family {
    Zip,
    Cfb,
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const CFB_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

fn sniff(bytes: &[u8]) -> Sniff {
    // Image signatures sit at offset 0; check them before the PDF marker,
    // which may appear anywhere in the first KiB.
    if let Some(f) = raster::sniff(bytes) {
        return Sniff::Known(DocKind::Image(f));
    }
    if let Some(name) = raster::unsupported(bytes) {
        return Sniff::UnsupportedImage(name);
    }
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(5).any(|w| w == b"%PDF-") {
        return Sniff::Known(DocKind::Pdf);
    }
    if bytes.starts_with(ZIP_MAGIC) {
        return sniff_zip(bytes);
    }
    if bytes.starts_with(CFB_MAGIC) {
        return sniff_cfb(bytes);
    }
    Sniff::Unknown
}

fn sniff_zip(bytes: &[u8]) -> Sniff {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Sniff::Family(Family::Zip);
    };
    // OpenDocument names itself in a `mimetype` entry (templates included).
    if let Ok(entry) = archive.by_name("mimetype") {
        let mut mime = String::new();
        if entry.take(128).read_to_string(&mut mime).is_ok() {
            let fmt = match mime.trim().strip_prefix("application/vnd.oasis.opendocument.") {
                Some(t) if t.starts_with("text") => Some(OfficeFormat::Odt),
                Some(t) if t.starts_with("spreadsheet") => Some(OfficeFormat::Ods),
                Some(t) if t.starts_with("presentation") => Some(OfficeFormat::Odp),
                _ => None,
            };
            if let Some(f) = fmt {
                return Sniff::Known(DocKind::Office(f));
            }
        }
    }
    let (mut word, mut xl, mut ppt) = (false, false, false);
    for name in archive.file_names() {
        word |= name.starts_with("word/");
        xl |= name.starts_with("xl/");
        ppt |= name.starts_with("ppt/");
    }
    match (word, xl, ppt) {
        (true, false, false) => Sniff::Known(DocKind::Office(OfficeFormat::Docx)),
        (false, true, false) => Sniff::Known(DocKind::Office(OfficeFormat::Xlsx)),
        (false, false, true) => Sniff::Known(DocKind::Office(OfficeFormat::Pptx)),
        _ => Sniff::Family(Family::Zip),
    }
}

fn sniff_cfb(bytes: &[u8]) -> Sniff {
    let Ok(cfb) = office_oxide::cfb::CfbReader::new(Cursor::new(bytes)) else {
        return Sniff::Family(Family::Cfb);
    };
    if cfb.has_stream("EncryptionInfo") && cfb.has_stream("EncryptedPackage") {
        Sniff::EncryptedOoxml
    } else if cfb.has_stream("WordDocument") {
        Sniff::Known(DocKind::Office(OfficeFormat::Doc))
    } else if cfb.has_stream("Workbook") || cfb.has_stream("Book") {
        Sniff::Known(DocKind::Office(OfficeFormat::Xls))
    } else if cfb.has_stream("PowerPoint Document") || cfb.has_stream("PP97_DUALSTORAGE") {
        Sniff::Known(DocKind::Office(OfficeFormat::Ppt))
    } else {
        Sniff::Family(Family::Cfb)
    }
}

fn fits_family(kind: DocKind, family: &Family) -> bool {
    use OfficeFormat as F;
    matches!(
        (kind, family),
        (DocKind::Office(F::Docx | F::Xlsx | F::Pptx | F::Odt | F::Ods | F::Odp), Family::Zip)
            | (DocKind::Office(F::Doc | F::Xls | F::Ppt), Family::Cfb)
    )
}

/// `hint` is the already-validated `format` param.
pub fn detect(bytes: &[u8], hint: Option<DocKind>, filename: Option<&str>) -> Result<Detected, ApiError> {
    let from_name = filename.and_then(DocKind::from_extension);
    let fallback = |pred: &dyn Fn(DocKind) -> bool| {
        hint.filter(|k| pred(*k))
            .map(|kind| Detected { kind, source: FormatSource::Param })
            .or_else(|| from_name.filter(|k| pred(*k)).map(|kind| Detected { kind, source: FormatSource::Filename }))
    };
    match sniff(bytes) {
        Sniff::Known(kind) => Ok(Detected { kind, source: FormatSource::Sniff }),
        Sniff::EncryptedOoxml => Err(ApiError::unprocessable(
            "encrypted-document",
            "Encrypted document",
            "the Office document is password-protected (encrypted OOXML); decryption is not supported",
        )),
        Sniff::UnsupportedImage(name) => Err(ApiError::unsupported(format!(
            "{name} images are not supported; convert to JPEG, PNG or TIFF (supported formats: {ACCEPTED_FORMATS})"
        ))),
        Sniff::Family(family) => fallback(&|k| fits_family(k, &family)).ok_or_else(|| {
            ApiError::unsupported(match family {
                Family::Zip => "the ZIP archive is not a DOCX, XLSX, PPTX, ODT, ODS or ODP package",
                Family::Cfb => "the compound file holds no Word, Excel or PowerPoint document",
            })
        }),
        // Only formats without a signature (TGA) are taken on a hint here;
        // bytes that match no signature aren't a PNG or a JPEG.
        Sniff::Unknown => fallback(&|k| !matches!(k, DocKind::Image(f) if f != ImageFormat::Tga)).ok_or_else(|| {
            ApiError::unsupported(format!(
                "the content is not a recognised document; supported formats: {ACCEPTED_FORMATS}"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_magic_within_first_kib() {
        let mut b = vec![b' '; 500];
        b.extend_from_slice(b"%PDF-1.7\n");
        let d = detect(&b, None, None).unwrap();
        assert_eq!(d.kind, DocKind::Pdf);
        assert_eq!(d.source, FormatSource::Sniff);
    }

    #[test]
    fn sniff_beats_filename_and_param() {
        let d = detect(b"%PDF-1.4", Some(DocKind::Office(OfficeFormat::Docx)), Some("x.xlsx")).unwrap();
        assert_eq!(d.kind, DocKind::Pdf);
        assert_eq!(d.source, FormatSource::Sniff);
    }

    #[test]
    fn unknown_content_uses_param_then_filename() {
        let d = detect(b"hello", Some(DocKind::Pdf), Some("a.docx")).unwrap();
        assert_eq!((d.kind, d.source), (DocKind::Pdf, FormatSource::Param));
        let d = detect(b"hello", None, Some("a.docx")).unwrap();
        assert_eq!((d.kind, d.source), (DocKind::Office(OfficeFormat::Docx), FormatSource::Filename));
        assert!(detect(b"hello", None, None).is_err());
        assert!(detect(b"hello", None, Some("noext")).is_err());
    }

    #[test]
    fn broken_zip_only_accepts_ooxml_hints() {
        let b = b"PK\x03\x04garbage";
        assert!(detect(b, None, None).is_err());
        assert!(detect(b, Some(DocKind::Pdf), None).is_err());
        let d = detect(b, Some(DocKind::Office(OfficeFormat::Xlsx)), None).unwrap();
        assert_eq!(d.source, FormatSource::Param);
        let d = detect(b, Some(DocKind::Office(OfficeFormat::Ods)), None).unwrap();
        assert_eq!(d.kind, DocKind::Office(OfficeFormat::Ods));
    }

    #[test]
    fn images_and_odf() {
        let d = detect(b"\x89PNG\r\n\x1a\n....", Some(DocKind::Pdf), Some("scan.pdf")).unwrap();
        assert_eq!((d.kind, d.source), (DocKind::Image(ImageFormat::Png), FormatSource::Sniff));
        // TGA has no signature: hints decide. Other image hints need the bytes to agree.
        let d = detect(b"\0\0\x02\0\0\0", None, Some("scan.TGA")).unwrap();
        assert_eq!((d.kind, d.source), (DocKind::Image(ImageFormat::Tga), FormatSource::Filename));
        assert!(detect(b"hello", Some(DocKind::Image(ImageFormat::Png)), None).is_err());
        let e = detect(b"\0\0\0\x18ftypheic\0\0\0\0", None, Some("IMG_0001.HEIC")).unwrap_err();
        assert!(e.detail.contains("HEIC"), "{}", e.detail);
        let odt = crate::odf::tests::package("application/vnd.oasis.opendocument.text-template", "", &[]);
        assert_eq!(detect(&odt, None, None).unwrap().kind, DocKind::Office(OfficeFormat::Odt));
    }

    #[test]
    fn names() {
        assert_eq!(DocKind::from_name("DOCX"), Some(DocKind::Office(OfficeFormat::Docx)));
        assert_eq!(DocKind::from_name("ods"), Some(DocKind::Office(OfficeFormat::Ods)));
        assert_eq!(DocKind::from_name("jpg"), Some(DocKind::Image(ImageFormat::Jpeg)));
        assert_eq!(DocKind::from_name("rtf"), None);
        assert_eq!(DocKind::from_extension("report.final.PDF"), Some(DocKind::Pdf));
    }
}
