//! OpenDocument reader (ODT, ODS, ODP and their templates). It builds
//! `office_oxide`'s `DocumentIR`, so text and markdown come out of the same
//! renderer as DOCX/XLSX/PPTX.
//!
//! `content.xml` is parsed as it inflates out of the ZIP: nothing is
//! decompressed whole, the XML read is capped, spreadsheet repeats expand
//! only for cells that hold text, and the text budget ends the parse early.
//! Headers, footers and master pages (`styles.xml`) aren't read.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Cursor, Read};

use bytes::Bytes;
use office_oxide::ir::{
    DocumentIR, Element, FootnoteRef, Heading, InlineContent, List, ListItem, Metadata, Note, Paragraph, Section, Table,
    TableCell, TableRow, TextSpan,
};
use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::QName;
use zip::ZipArchive;

use crate::error::ApiError;

/// Decompressed `content.xml` read at most; past it the text is truncated.
const MAX_XML_BYTES: u64 = 256 * 1024 * 1024;
/// `manifest.xml`, `meta.xml`.
const MAX_SMALL_PART: u64 = 4 * 1024 * 1024;
/// Spreadsheet and table cells materialized per document, empty ones included.
const MAX_CELLS: usize = 2_000_000;
/// Columns kept per row (ODF's own sheet limit is 16384).
const MAX_COLS: usize = 16_384;
/// Nested lists, tables, notes and frames.
const MAX_DEPTH: usize = 64;
/// One `<text:s text:c="…"/>` expands to at most this many spaces.
const MAX_SPACES: usize = 256;

pub fn read(bytes: Bytes, max_chars: usize) -> Result<DocumentIR, ApiError> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| corrupt(format!("the OpenDocument package could not be opened: {e}")))?;
    if read_small(&mut zip, "META-INF/manifest.xml").is_some_and(|m| m.contains("encryption-data")) {
        return Err(ApiError::unprocessable(
            "encrypted-document",
            "Encrypted document",
            "the OpenDocument file is password-protected; decryption is not supported",
        ));
    }
    let mut metadata = read_small(&mut zip, "meta.xml").map(|m| parse_meta(&m)).unwrap_or_default();
    let content = zip
        .by_name("content.xml")
        .map_err(|_| corrupt("the OpenDocument package has no content.xml".into()))?;
    let capped = content.size() > MAX_XML_BYTES;
    let mut p = Parser::new(BufReader::new(content.take(MAX_XML_BYTES)), max_chars, capped);
    p.document()?;
    metadata.text_truncated = p.truncated;
    Ok(DocumentIR { metadata, sections: p.sections, ..Default::default() })
}

fn corrupt(detail: String) -> ApiError {
    ApiError::unprocessable("corrupt-document", "Unreadable document", detail)
}

fn read_small<R: Read + std::io::Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<String> {
    let mut s = String::new();
    zip.by_name(name).ok()?.take(MAX_SMALL_PART).read_to_string(&mut s).ok()?;
    Some(s)
}

/// `&amp;`, `&#x41;` and friends.
fn resolve_ref(r: &BytesRef) -> String {
    let raw = format!("&{};", r.xml10_content());
    quick_xml::escape::unescape(&raw).map(|s| s.into_owned()).unwrap_or_default()
}

fn parse_meta(xml: &str) -> Metadata {
    let mut md = Metadata::default();
    let mut reader = Reader::from_str(xml);
    let (mut current, mut text) = (None::<String>, String::new());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = Some(e.name().0.to_string());
                text.clear();
            }
            Ok(Event::Text(t)) => text.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) => text.push_str(&resolve_ref(&r)),
            Ok(Event::End(_)) => {
                let (Some(name), value) = (current.take(), text.trim()) else { continue };
                if value.is_empty() {
                    continue;
                }
                let v = Some(value.to_string());
                match name.as_str() {
                    "dc:title" => md.title = v,
                    "dc:subject" => md.subject = v,
                    "dc:description" => md.description = v,
                    "dc:language" => md.language = v,
                    "meta:keyword" => md.keywords.push(value.to_string()),
                    "meta:initial-creator" => md.author = v,
                    "dc:creator" => md.last_modified_by = v,
                    "meta:creation-date" => md.created = v,
                    "dc:date" => md.modified = v,
                    "meta:editing-cycles" => md.revision = v,
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if md.author.is_none() {
        md.author = md.last_modified_by.clone();
    }
    md
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    TextBody,
    SheetBody,
    SlideBody,
    ListStyle,
    ListLevelNumber,
    P,
    H,
    Span,
    Link,
    Space,
    Tab,
    LineBreak,
    List,
    ListItem,
    Note,
    NoteCitation,
    NoteBody,
    Table,
    HeaderRows,
    Row,
    Cell,
    Page,
    /// Any `draw:*` shape, frame, text box or group.
    Draw,
    Notes,
    /// A subtree that holds no document text; never surfaces as an event.
    Skip,
    Other,
}

fn classify(name: &str) -> Tag {
    match name {
        "office:text" => Tag::TextBody,
        "office:spreadsheet" => Tag::SheetBody,
        "office:presentation" | "office:drawing" => Tag::SlideBody,
        "text:list-style" => Tag::ListStyle,
        "text:list-level-style-number" => Tag::ListLevelNumber,
        "text:p" => Tag::P,
        "text:h" => Tag::H,
        "text:span" => Tag::Span,
        "text:a" => Tag::Link,
        "text:s" => Tag::Space,
        "text:tab" => Tag::Tab,
        "text:line-break" => Tag::LineBreak,
        "text:list" => Tag::List,
        "text:list-item" | "text:list-header" => Tag::ListItem,
        "text:note" => Tag::Note,
        "text:note-citation" => Tag::NoteCitation,
        "text:note-body" => Tag::NoteBody,
        "table:table" => Tag::Table,
        "table:table-header-rows" => Tag::HeaderRows,
        "table:table-row" => Tag::Row,
        "table:table-cell" | "table:covered-table-cell" => Tag::Cell,
        "draw:page" => Tag::Page,
        "presentation:notes" => Tag::Notes,
        // Comments, tracked deletions, forms, declarations, alt text,
        // embedded binaries and index templates.
        "office:annotation"
        | "text:tracked-changes"
        | "table:tracked-changes"
        | "office:forms"
        | "office:scripts"
        | "office:font-face-decls"
        | "office:event-listeners"
        | "office:binary-data"
        | "text:sequence-decls"
        | "text:variable-decls"
        | "text:user-field-decls"
        | "text:dde-connection-decls"
        | "table:table-columns"
        | "table:table-column-group"
        | "table:table-header-columns"
        | "table:shapes"
        | "table:named-expressions"
        | "table:database-ranges"
        | "table:content-validations"
        | "table:calculation-settings"
        | "table:label-ranges"
        | "svg:title"
        | "svg:desc"
        | "draw:image"
        | "draw:page-thumbnail"
        | "draw:enhanced-geometry"
        | "presentation:settings" => Tag::Skip,
        n if n.starts_with("text:") && n.ends_with("-source") => Tag::Skip,
        n if n.starts_with("draw:") => Tag::Draw,
        _ => Tag::Other,
    }
}

#[derive(Default)]
struct Attrs {
    /// `style:name`, `table:name`.
    name: Option<String>,
    /// `text:outline-level`, `text:level`, `text:c`.
    number: Option<usize>,
    href: Option<String>,
    /// `text:style-name` of a list.
    style: Option<String>,
    /// `presentation:class`.
    class: Option<String>,
    repeat: usize,
    /// A cell's typed value, used when it has no paragraph.
    value: Option<String>,
}

struct Node {
    tag: Tag,
    a: Attrs,
}

fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.0 == key)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

impl Node {
    fn new(tag: Tag, e: &BytesStart) -> Self {
        let num = |k| attr(e, k).and_then(|v| v.trim().parse::<usize>().ok());
        let mut a = Attrs::default();
        match tag {
            Tag::H => a.number = num("text:outline-level"),
            Tag::ListLevelNumber => a.number = num("text:level"),
            Tag::Space => a.number = num("text:c"),
            Tag::Link => a.href = attr(e, "xlink:href"),
            Tag::List => a.style = attr(e, "text:style-name"),
            Tag::ListStyle => a.name = attr(e, "style:name"),
            Tag::Table => a.name = attr(e, "table:name"),
            Tag::Draw => a.class = attr(e, "presentation:class"),
            Tag::Row => a.repeat = num("table:number-rows-repeated").unwrap_or(1),
            Tag::Cell => {
                a.repeat = num("table:number-columns-repeated").unwrap_or(1);
                a.value = ["office:string-value", "office:value", "office:date-value", "office:time-value", "office:boolean-value"]
                    .iter()
                    .find_map(|k| attr(e, k))
                    .filter(|v| !v.trim().is_empty());
            }
            _ => {}
        }
        a.repeat = a.repeat.max(1);
        Self { tag, a }
    }
}

enum Ev {
    Open(Node),
    Leaf(Node),
    Close,
    Text(String),
    Eof,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<R: BufRead> {
    xml: Reader<R>,
    buf: Vec<u8>,
    skip_buf: Vec<u8>,
    /// `content.xml` is larger than the read cap: a parse error is the cut.
    capped: bool,
    stopped: bool,
    truncated: bool,
    chars: usize,
    max_chars: usize,
    cells: usize,
    depth: usize,
    ordered_styles: HashSet<String>,
    list_ordered: Vec<bool>,
    notes: Vec<Element>,
    slide_title: Option<String>,
    slide_notes: Vec<Element>,
    sections: Vec<Section>,
}

impl<R: BufRead> Parser<R> {
    fn new(r: R, max_chars: usize, capped: bool) -> Self {
        Self {
            xml: Reader::from_reader(r),
            buf: Vec::new(),
            skip_buf: Vec::new(),
            capped,
            stopped: false,
            truncated: false,
            chars: 0,
            max_chars,
            cells: 0,
            depth: 0,
            ordered_styles: HashSet::new(),
            list_ordered: Vec::new(),
            notes: Vec::new(),
            slide_title: None,
            slide_notes: Vec::new(),
            sections: Vec::new(),
        }
    }

    /// Ends the parse; every loop sees `Eof` from here on.
    fn stop(&mut self) {
        self.stopped = true;
        self.truncated = true;
    }

    fn charge_chars(&mut self, n: usize) {
        self.chars = self.chars.saturating_add(n);
        if self.chars >= self.max_chars {
            self.stop();
        }
    }

    fn charge_cells(&mut self, n: usize) -> bool {
        self.cells = self.cells.saturating_add(n);
        if self.cells > MAX_CELLS {
            self.stop();
        }
        !self.stopped
    }

    fn next(&mut self) -> Result<Ev, ApiError> {
        loop {
            if self.stopped {
                return Ok(Ev::Eof);
            }
            self.buf.clear();
            let ev = match self.xml.read_event_into(&mut self.buf) {
                Ok(ev) => ev,
                Err(_) if self.capped => {
                    self.stop();
                    return Ok(Ev::Eof);
                }
                Err(e) => return Err(corrupt(format!("content.xml is not well-formed: {e}"))),
            };
            return Ok(match ev {
                Event::Start(e) => {
                    let tag = classify(e.name().0);
                    if tag == Tag::Skip {
                        let name = e.name().0.to_string();
                        self.skip_buf.clear();
                        if self.xml.read_to_end_into(QName(&name), &mut self.skip_buf).is_err() {
                            if self.capped {
                                self.stop();
                                return Ok(Ev::Eof);
                            }
                            return Err(corrupt("content.xml is not well-formed".into()));
                        }
                        continue;
                    }
                    Ev::Open(Node::new(tag, &e))
                }
                Event::Empty(e) => match classify(e.name().0) {
                    Tag::Skip => continue,
                    tag => Ev::Leaf(Node::new(tag, &e)),
                },
                Event::End(_) => Ev::Close,
                Event::Text(t) => Ev::Text(t.xml10_content().into_owned()),
                Event::CData(t) => Ev::Text(t.xml10_content().into_owned()),
                Event::GeneralRef(r) => Ev::Text(resolve_ref(&r)),
                Event::Eof => Ev::Eof,
                _ => continue,
            });
        }
    }

    /// Consumes the rest of the element just opened.
    fn skip_open(&mut self) -> Result<(), ApiError> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Ev::Eof => return Ok(()),
                Ev::Open(_) => depth += 1,
                Ev::Close if depth == 0 => return Ok(()),
                Ev::Close => depth -= 1,
                _ => {}
            }
        }
    }

    /// Runs `f` one nesting level deeper, or skips the element when the
    /// document nests past `MAX_DEPTH`.
    fn nested<T: Default>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, ApiError>) -> Result<T, ApiError> {
        if self.depth >= MAX_DEPTH {
            self.skip_open()?;
            return Ok(T::default());
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn document(&mut self) -> Result<(), ApiError> {
        loop {
            match self.next()? {
                Ev::Eof => return Ok(()),
                Ev::Open(n) => match n.tag {
                    Tag::ListStyle => self.list_style(n.a.name)?,
                    Tag::TextBody => {
                        let mut elements = self.blocks()?;
                        elements.append(&mut self.notes);
                        self.sections.push(Section { elements, ..Default::default() });
                    }
                    Tag::SheetBody => self.sheets()?,
                    Tag::SlideBody => self.slides()?,
                    _ => {} // office:document-content, office:body, styles: walk through
                },
                _ => {}
            }
        }
    }

    /// Records list styles whose first level is numbered.
    fn list_style(&mut self, name: Option<String>) -> Result<(), ApiError> {
        let mut depth = 0usize;
        loop {
            let (n, open) = match self.next()? {
                Ev::Eof => return Ok(()),
                Ev::Close if depth == 0 => return Ok(()),
                Ev::Close => {
                    depth -= 1;
                    continue;
                }
                Ev::Open(n) => (n, true),
                Ev::Leaf(n) => (n, false),
                Ev::Text(_) => continue,
            };
            depth += usize::from(open);
            if n.tag == Tag::ListLevelNumber
                && n.a.number.unwrap_or(1) == 1
                && let Some(name) = &name
            {
                self.ordered_styles.insert(name.clone());
            }
        }
    }

    // --- blocks --------------------------------------------------------

    /// Block content up to the end of the element just opened. Elements
    /// this reader doesn't model are walked through, so their text stays.
    fn blocks(&mut self) -> Result<Vec<Element>, ApiError> {
        self.nested(|p| {
            let mut out = Vec::new();
            let mut depth = 0usize;
            loop {
                match p.next()? {
                    Ev::Eof => break,
                    Ev::Close if depth == 0 => break,
                    Ev::Close => depth -= 1,
                    Ev::Text(_) | Ev::Leaf(_) => {} // whitespace, empty paragraphs, markers
                    Ev::Open(n) => match n.tag {
                        Tag::P | Tag::H => {
                            let (content, extra) = p.inline()?;
                            if has_text(&content) {
                                out.push(match n.tag {
                                    Tag::H => Element::Heading(Heading {
                                        level: n.a.number.unwrap_or(1).clamp(1, 6) as u8,
                                        content,
                                        ..Default::default()
                                    }),
                                    _ => Element::Paragraph(Paragraph { content, ..Default::default() }),
                                });
                            }
                            out.extend(extra);
                        }
                        Tag::List => out.extend(p.list(n.a.style)?),
                        Tag::Table => {
                            let t = p.table(false)?;
                            if !t.rows.is_empty() {
                                out.push(Element::Table(t));
                            }
                        }
                        Tag::Draw if n.a.class.as_deref() == Some("title") => {
                            let inner = p.blocks()?;
                            let title = blocks_text(&inner);
                            if !title.is_empty() {
                                p.slide_title.get_or_insert_with(|| title.clone());
                                out.push(Element::Heading(Heading {
                                    level: 2,
                                    content: vec![InlineContent::Text(TextSpan::plain(title))],
                                    ..Default::default()
                                }));
                            }
                        }
                        Tag::Notes => {
                            let notes = p.blocks()?;
                            p.slide_notes.extend(notes);
                        }
                        _ => depth += 1,
                    },
                }
            }
            Ok(out)
        })
    }

    // --- inline --------------------------------------------------------

    /// A paragraph's content up to its end, plus blocks from frames
    /// anchored inside it (text boxes, captions), which follow it.
    fn inline(&mut self) -> Result<(Vec<InlineContent>, Vec<Element>), ApiError> {
        self.nested(|p| {
            let mut line = Line::default();
            let mut extra = Vec::new();
            // One entry per open element inside the paragraph: the link it starts.
            let mut open: Vec<Option<String>> = Vec::new();
            loop {
                let (n, is_open) = match p.next()? {
                    Ev::Eof => break,
                    Ev::Close => match open.pop() {
                        None => break,
                        Some(_) => {
                            let href = open.iter().rev().find_map(|h| h.clone());
                            if href != line.href {
                                line.flush();
                                line.href = href;
                            }
                            continue;
                        }
                    },
                    Ev::Text(t) => {
                        let n = line.text(&t);
                        p.charge_chars(n);
                        continue;
                    }
                    Ev::Open(n) => (n, true),
                    Ev::Leaf(n) => (n, false),
                };
                match n.tag {
                    Tag::Space => {
                        let k = n.a.number.unwrap_or(1).clamp(1, MAX_SPACES);
                        line.literal(&" ".repeat(k));
                        p.charge_chars(k);
                    }
                    Tag::Tab => {
                        line.literal("\t");
                        p.charge_chars(1);
                    }
                    Tag::LineBreak => line.line_break(),
                    _ => {}
                }
                if !is_open {
                    continue;
                }
                match n.tag {
                    Tag::Draw => extra.extend(p.blocks()?),
                    Tag::Note => {
                        let (id, marker) = p.note()?;
                        line.flush();
                        line.content.push(InlineContent::FootnoteRef(FootnoteRef { note_id: id, marker }));
                    }
                    _ => {
                        let href = n.a.href.clone();
                        if href.is_some() {
                            line.flush();
                            line.href = href.clone();
                        }
                        open.push(href);
                    }
                }
            }
            Ok((line.finish(), extra))
        })
    }

    /// A footnote or endnote; its body goes to the end of the document.
    fn note(&mut self) -> Result<(u32, Option<String>), ApiError> {
        let (mut marker, mut content) = (String::new(), Vec::new());
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Ev::Eof => break,
                Ev::Close if depth == 0 => break,
                Ev::Close => depth -= 1,
                Ev::Open(n) if n.tag == Tag::NoteCitation => marker = self.plain_text()?,
                Ev::Open(n) if n.tag == Tag::NoteBody => content = self.blocks()?,
                Ev::Open(_) => depth += 1,
                _ => {}
            }
        }
        let id = self.notes.len() as u32 + 1;
        let marker = Some(marker.trim().to_string()).filter(|m| !m.is_empty());
        self.notes.push(Element::Footnote(Note { id, content, marker: marker.clone(), author: None }));
        Ok((id, marker))
    }

    /// All text up to the end of the element just opened.
    fn plain_text(&mut self) -> Result<String, ApiError> {
        let (mut s, mut depth) = (String::new(), 0usize);
        loop {
            match self.next()? {
                Ev::Eof => break,
                Ev::Close if depth == 0 => break,
                Ev::Close => depth -= 1,
                Ev::Open(_) => depth += 1,
                Ev::Text(t) => s.push_str(&t),
                Ev::Leaf(_) => {}
            }
        }
        Ok(s)
    }

    // --- lists ---------------------------------------------------------

    fn list(&mut self, style: Option<String>) -> Result<Vec<Element>, ApiError> {
        let inherited = self.list_ordered.last().copied().unwrap_or(false);
        let ordered = style.map_or(inherited, |s| self.ordered_styles.contains(&s));
        self.list_ordered.push(ordered);
        let items = self.nested(|p| {
            let mut items: Vec<ListItem> = Vec::new();
            let mut depth = 0usize;
            loop {
                match p.next()? {
                    Ev::Eof => break,
                    Ev::Close if depth == 0 => break,
                    Ev::Close => depth -= 1,
                    Ev::Open(n) if n.tag == Tag::ListItem => {
                        let mut content = p.blocks()?;
                        let nested = content
                            .iter()
                            .position(|e| matches!(e, Element::List(_)))
                            .and_then(|i| match &mut content[i] {
                                Element::List(l) => {
                                    let l = std::mem::take(l);
                                    content.remove(i);
                                    Some(l)
                                }
                                _ => None,
                            });
                        // A level-2 item with no level-1 text of its own
                        // belongs under the item before it.
                        if content.is_empty()
                            && let Some(prev) = items.last_mut().filter(|i| i.nested.is_none())
                        {
                            prev.nested = nested;
                            continue;
                        }
                        if !content.is_empty() || nested.is_some() {
                            items.push(ListItem { content, nested });
                        }
                    }
                    Ev::Open(_) => depth += 1,
                    _ => {}
                }
            }
            Ok(items)
        });
        self.list_ordered.pop();
        let list = List { ordered, items: items?, ..Default::default() };
        if list.items.is_empty() {
            return Ok(Vec::new());
        }
        // Outline numbering wraps headings in lists; they stay headings.
        if let Some(headings) = heading_list(&list) {
            return Ok(headings);
        }
        Ok(vec![Element::List(list)])
    }

    // --- tables --------------------------------------------------------

    /// A table up to its end. `sheet` drops empty rows and trailing empty
    /// cells, and expands repeats only for cells that hold text.
    fn table(&mut self, sheet: bool) -> Result<Table, ApiError> {
        self.nested(|p| {
            let mut rows = Vec::new();
            let mut depth = 0usize;
            let mut header_at: Option<usize> = None;
            loop {
                match p.next()? {
                    Ev::Eof => break,
                    Ev::Close if depth == 0 => break,
                    Ev::Close => {
                        if header_at == Some(depth) {
                            header_at = None;
                        }
                        depth -= 1;
                    }
                    Ev::Open(n) => match n.tag {
                        Tag::Row => p.row(n.a.repeat, sheet, header_at.is_some(), &mut rows)?,
                        Tag::HeaderRows => {
                            depth += 1;
                            header_at = Some(depth);
                        }
                        _ => depth += 1,
                    },
                    _ => {} // an empty `<table:table-row/>` has no cells to keep
                }
            }
            Ok(Table { rows, ..Default::default() })
        })
    }

    fn row(&mut self, repeat: usize, sheet: bool, is_header: bool, rows: &mut Vec<TableRow>) -> Result<(), ApiError> {
        let chars_before = self.chars;
        let mut cells: Vec<TableCell> = Vec::new();
        let mut pending_empty = 0usize;
        let mut depth = 0usize;
        loop {
            let (n, open) = match self.next()? {
                Ev::Eof => break,
                Ev::Close if depth == 0 => break,
                Ev::Close => {
                    depth -= 1;
                    continue;
                }
                Ev::Open(n) => (n, true),
                Ev::Leaf(n) => (n, false),
                Ev::Text(_) => continue,
            };
            if n.tag != Tag::Cell {
                depth += usize::from(open);
                continue;
            }
            let before = self.chars;
            let mut content = if open { self.blocks()? } else { Vec::new() };
            if content.is_empty()
                && let Some(v) = n.a.value
            {
                self.charge_chars(v.chars().count());
                content.push(Element::Paragraph(Paragraph {
                    content: vec![InlineContent::Text(TextSpan::plain(v))],
                    ..Default::default()
                }));
            }
            let cell_chars = self.chars - before;
            if content.is_empty() {
                pending_empty = pending_empty.saturating_add(n.a.repeat);
                if sheet {
                    continue; // materialized only if text follows in this row
                }
            }
            let empties = pending_empty.min(MAX_COLS.saturating_sub(cells.len()));
            pending_empty = 0;
            if !self.charge_cells(empties) {
                break;
            }
            cells.extend((0..empties).map(|_| cell(Vec::new())));
            if content.is_empty() {
                continue;
            }
            let copies = n.a.repeat.min(MAX_COLS.saturating_sub(cells.len()));
            if copies == 0 || !self.charge_cells(copies) {
                break;
            }
            self.charge_chars(cell_chars.saturating_mul(copies - 1));
            for _ in 1..copies {
                cells.push(cell(content.clone()));
            }
            cells.push(cell(content));
        }
        if cells.is_empty() || (sheet && cells.iter().all(|c| c.content.is_empty())) {
            return Ok(());
        }
        let row_chars = self.chars - chars_before;
        let row = TableRow { cells, is_header, ..Default::default() };
        for _ in 1..repeat {
            if !self.charge_cells(row.cells.len()) {
                break;
            }
            self.charge_chars(row_chars);
            rows.push(row.clone());
        }
        rows.push(row);
        Ok(())
    }

    // --- spreadsheets and presentations --------------------------------

    fn sheets(&mut self) -> Result<(), ApiError> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Ev::Eof => return Ok(()),
                Ev::Close if depth == 0 => return Ok(()),
                Ev::Close => depth -= 1,
                Ev::Open(n) if n.tag == Tag::Table => {
                    let table = self.table(true)?;
                    let elements = if table.rows.is_empty() { Vec::new() } else { vec![Element::Table(table)] };
                    self.sections.push(Section { title: n.a.name, elements, ..Default::default() });
                }
                Ev::Leaf(n) if n.tag == Tag::Table => {
                    self.sections.push(Section { title: n.a.name, ..Default::default() });
                }
                Ev::Open(_) => depth += 1,
                _ => {}
            }
        }
    }

    fn slides(&mut self) -> Result<(), ApiError> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Ev::Eof => return Ok(()),
                Ev::Close if depth == 0 => return Ok(()),
                Ev::Close => depth -= 1,
                Ev::Open(n) if n.tag == Tag::Page => {
                    self.slide_title = None;
                    let elements = self.blocks()?;
                    let notes = std::mem::take(&mut self.slide_notes);
                    self.sections.push(Section {
                        title: self.slide_title.take(),
                        elements,
                        speaker_notes: (!notes.is_empty()).then_some(notes),
                        ..Default::default()
                    });
                }
                Ev::Open(_) => depth += 1,
                _ => {}
            }
        }
    }
}

fn cell(content: Vec<Element>) -> TableCell {
    TableCell { content, col_span: 1, row_span: 1, ..Default::default() }
}

fn has_text(content: &[InlineContent]) -> bool {
    content.iter().any(|c| matches!(c, InlineContent::Text(t) if !t.text.trim().is_empty()))
}

/// The text of paragraphs and headings, joined by spaces.
fn blocks_text(blocks: &[Element]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|e| match e {
            Element::Paragraph(p) => Some(office_oxide::ir::inline_to_text(&p.content)),
            Element::Heading(h) => Some(office_oxide::ir::inline_to_text(&h.content)),
            _ => None,
        })
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty())
        .collect();
    parts.join(" ")
}

/// The headings of a list that holds nothing but headings (and nested
/// lists of headings), in order.
fn heading_list(list: &List) -> Option<Vec<Element>> {
    let mut out = Vec::new();
    for item in &list.items {
        for e in &item.content {
            match e {
                Element::Heading(h) => out.push(Element::Heading(h.clone())),
                _ => return None,
            }
        }
        if let Some(nested) = &item.nested {
            out.extend(heading_list(nested)?);
        }
    }
    Some(out)
}

/// Paragraph text with ODF whitespace rules: runs of spaces, tabs and
/// newlines in the XML collapse to one space and are dropped at the
/// paragraph's edges; `<text:s/>`, `<text:tab/>` and `<text:line-break/>`
/// are literal.
#[derive(Default)]
struct Line {
    content: Vec<InlineContent>,
    cur: String,
    href: Option<String>,
    started: bool,
    pending_space: bool,
}

impl Line {
    /// Appends XML text; returns the characters it added.
    fn text(&mut self, t: &str) -> usize {
        let mut n = 0;
        for c in t.chars() {
            if matches!(c, ' ' | '\t' | '\n' | '\r') {
                self.pending_space = self.started;
                continue;
            }
            if std::mem::take(&mut self.pending_space) {
                self.cur.push(' ');
                n += 1;
            }
            self.cur.push(c);
            self.started = true;
            n += 1;
        }
        n
    }

    fn literal(&mut self, s: &str) {
        if std::mem::take(&mut self.pending_space) {
            self.cur.push(' ');
        }
        self.cur.push_str(s);
        self.started = true;
    }

    fn line_break(&mut self) {
        self.pending_space = false;
        self.flush();
        self.content.push(InlineContent::LineBreak);
    }

    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut span = TextSpan::plain(std::mem::take(&mut self.cur));
        span.hyperlink = self.href.clone();
        self.content.push(InlineContent::Text(span));
    }

    fn finish(mut self) -> Vec<InlineContent> {
        self.flush();
        while matches!(self.content.last(), Some(InlineContent::LineBreak)) {
            self.content.pop();
        }
        self.content
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0""#;

    pub(crate) fn package(mimetype: &str, body: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("mimetype", stored).unwrap();
        w.write_all(mimetype.as_bytes()).unwrap();
        let content = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}>{body}</office:document-content>"#);
        w.start_file("content.xml", stored).unwrap();
        w.write_all(content.as_bytes()).unwrap();
        for (name, data) in extra {
            w.start_file(*name, stored).unwrap();
            w.write_all(data.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn ir(body: &str) -> DocumentIR {
        read(Bytes::from(package("application/vnd.oasis.opendocument.text", body, &[])), 1_000_000).unwrap()
    }

    #[test]
    fn text_document_structure() {
        let d = ir(r#"<office:automatic-styles><text:list-style style:name="L1"><text:list-level-style-number text:level="1"/></text:list-style></office:automatic-styles>
            <office:body><office:text>
              <text:sequence-decls><text:sequence-decl text:name="Figure"/></text:sequence-decls>
              <text:h text:outline-level="2">Intro   title</text:h>
              <text:p>Hello <text:span>wide</text:span><text:s text:c="2"/>world &amp; more<text:note><text:note-citation>1</text:note-citation><text:note-body><text:p>A footnote.</text:p></text:note-body></text:note></text:p>
              <text:p>See <text:a xlink:href="https://example.com">the site</text:a>.</text:p>
              <text:list text:style-name="L1"><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p><text:list><text:list-item><text:p>two-a</text:p></text:list-item></text:list></text:list-item></text:list>
              <table:table table:name="T"><table:table-column/><table:table-row><table:table-cell><text:p>a</text:p></table:table-cell><table:table-cell><text:p>b</text:p></table:table-cell></table:table-row></table:table>
              <text:p><office:annotation><text:p>a comment</text:p></office:annotation>after comment</text:p>
              <text:p><draw:frame><draw:text-box><text:p>boxed text</text:p></draw:text-box></draw:frame></text:p>
              <text:tracked-changes><text:changed-region><text:deletion><text:p>deleted text</text:p></text:deletion></text:changed-region></text:tracked-changes>
            </office:text></office:body>"#);
        let md = d.to_markdown();
        assert!(md.contains("## Intro title"), "{md}");
        assert!(md.contains("Hello wide  world & more"), "{md}");
        assert!(md.contains("[the site](https://example.com)"), "{md}");
        assert!(md.contains("1. one") && md.contains("two-a"), "{md}");
        assert!(md.contains("| a | b |"), "{md}");
        assert!(md.contains("after comment") && !md.contains("a comment"), "{md}");
        assert!(md.contains("boxed text"), "{md}");
        assert!(md.contains("A footnote."), "{md}");
        assert!(!md.contains("deleted text") && !md.contains("Figure"), "{md}");
        let plain = d.plain_text();
        assert!(plain.contains("Hello wide  world & more"), "{plain}");
    }

    #[test]
    fn spreadsheet_repeats_are_bounded() {
        let body = r#"<office:body><office:spreadsheet>
            <table:table table:name="Data">
              <table:table-column table:number-columns-repeated="1024"/>
              <table:table-row table:number-rows-repeated="3"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>
              <table:table-row><table:table-cell table:number-columns-repeated="2"/><table:table-cell office:value-type="float" office:value="7"><text:p>7.00</text:p></table:table-cell><table:table-cell table:number-columns-repeated="16000"/></table:table-row>
              <table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="2"><text:p>x</text:p></table:table-cell></table:table-row>
              <table:table-row table:number-rows-repeated="1048000"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>
            </table:table>
            <table:table table:name="Empty"/>
          </office:spreadsheet></office:body>"#;
        let d = read(Bytes::from(package("application/vnd.oasis.opendocument.spreadsheet", body, &[])), 1_000_000).unwrap();
        assert_eq!(d.sections.len(), 2);
        assert_eq!(d.sections[0].title.as_deref(), Some("Data"));
        let Element::Table(t) = &d.sections[0].elements[0] else { panic!("no table") };
        assert_eq!(t.rows.len(), 3, "empty rows dropped, the repeated text row expanded");
        assert_eq!(t.rows[0].cells.len(), 3, "leading empties kept, trailing dropped");
        assert_eq!(t.rows[1].cells.len(), 2);
        assert!(d.sections[1].elements.is_empty());
        assert!(!d.metadata.text_truncated);
        let md = d.to_markdown();
        assert!(md.contains("## Data") && md.contains("7.00"), "{md}");
    }

    #[test]
    fn cell_bomb_stops_at_the_budget() {
        let body = r#"<office:body><office:spreadsheet><table:table table:name="Bomb">
            <table:table-row table:number-rows-repeated="1000000"><table:table-cell table:number-columns-repeated="16384"><text:p>x</text:p></table:table-cell></table:table-row>
          </table:table></office:spreadsheet></office:body>"#;
        let d = read(Bytes::from(package("application/vnd.oasis.opendocument.spreadsheet", body, &[])), 4_000_000).unwrap();
        assert!(d.metadata.text_truncated);
        let Element::Table(t) = &d.sections[0].elements[0] else { panic!("no table") };
        assert!(t.rows.iter().map(|r| r.cells.len()).sum::<usize>() <= MAX_CELLS);
    }

    #[test]
    fn presentation_slides_titles_and_notes() {
        let body = r#"<office:body><office:presentation>
            <draw:page draw:name="page1">
              <draw:frame presentation:class="title"><draw:text-box><text:p>Roadmap</text:p></draw:text-box></draw:frame>
              <draw:frame presentation:class="outline"><draw:text-box><text:list><text:list-item><text:p>First point</text:p></text:list-item></text:list></draw:text-box></draw:frame>
              <draw:custom-shape><text:p>Shape text</text:p><draw:enhanced-geometry/></draw:custom-shape>
              <presentation:notes><draw:page-thumbnail/><draw:frame presentation:class="notes"><draw:text-box><text:p>Speaker note</text:p></draw:text-box></draw:frame></presentation:notes>
            </draw:page>
            <draw:page draw:name="page2"><draw:frame><table:table><table:table-row><table:table-cell><text:p>cell</text:p></table:table-cell></table:table-row></table:table></draw:frame></draw:page>
          </office:presentation></office:body>"#;
        let d = read(Bytes::from(package("application/vnd.oasis.opendocument.presentation", body, &[])), 1_000_000).unwrap();
        assert_eq!(d.sections.len(), 2);
        assert_eq!(d.sections[0].title.as_deref(), Some("Roadmap"));
        let md = d.to_markdown();
        assert_eq!(md.matches("Roadmap").count(), 1, "{md}");
        assert!(md.contains("- First point") && md.contains("Shape text") && md.contains("Speaker note"), "{md}");
        assert!(md.contains("| cell |"), "{md}");
    }

    #[test]
    fn metadata_encryption_and_errors() {
        let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"><office:meta><dc:title>Report &amp; Notes</dc:title><meta:initial-creator>Ada</meta:initial-creator><meta:keyword>alpha</meta:keyword><meta:keyword>beta</meta:keyword><meta:document-statistic meta:page-count="1"/></office:meta></office:document-meta>"#;
        let body = "<office:body><office:text><text:p>x</text:p></office:text></office:body>";
        let d = read(Bytes::from(package("application/vnd.oasis.opendocument.text", body, &[("meta.xml", meta)])), 100).unwrap();
        assert_eq!(d.metadata.title.as_deref(), Some("Report & Notes"));
        assert_eq!(d.metadata.author.as_deref(), Some("Ada"));
        assert_eq!(d.metadata.keywords, vec!["alpha", "beta"]);

        let manifest = r#"<manifest:manifest><manifest:file-entry manifest:full-path="content.xml"><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
        let enc = package("application/vnd.oasis.opendocument.text", body, &[("META-INF/manifest.xml", manifest)]);
        assert_eq!(read(Bytes::from(enc), 100).unwrap_err().slug, "encrypted-document");

        let broken = package("application/vnd.oasis.opendocument.text", "<office:body><office:text><text:p>x</office:text>", &[]);
        assert_eq!(read(Bytes::from(broken), 100).unwrap_err().slug, "corrupt-document");
        assert_eq!(read(Bytes::from_static(b"PK\x03\x04junk"), 100).unwrap_err().slug, "corrupt-document");
    }

    #[test]
    fn text_budget_truncates() {
        let para = "<text:p>abcdefghij</text:p>".repeat(100);
        let body = format!("<office:body><office:text>{para}</office:text></office:body>");
        let d = read(Bytes::from(package("application/vnd.oasis.opendocument.text", &body, &[])), 55).unwrap();
        assert!(d.metadata.text_truncated);
        assert!(d.plain_text().chars().filter(|c| c.is_alphabetic()).count() <= 60);
    }

    #[test]
    fn deep_nesting_is_cut_not_overflowed() {
        let depth = 10_000;
        let body = format!(
            "<office:body><office:text>{}<text:p>deep</text:p>{}<text:p>after</text:p></office:text></office:body>",
            "<text:list><text:list-item>".repeat(depth),
            "</text:list-item></text:list>".repeat(depth)
        );
        assert!(ir(&body).plain_text().contains("after"));
    }
}
