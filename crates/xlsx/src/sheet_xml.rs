//! Zero-copy streaming reader for worksheet `<row>` elements — the hot path of sheet reading.
//!
//! A big sheet is millions of `<c>` elements, and building the generic [`El`] tree for each costs
//! around ten small heap allocations per cell (element name, attribute names and values, text).
//! [`parse_sheet`] instead streams rows straight off the part's bytes: attribute names and values
//! borrow the input, cell text stays a borrowed [`Cow`] until an entity forces decoding, and only
//! the small non-row part of the sheet (views, column widths, merges…) is built as an [`El`] tree.
//!
//! What the row callback and the returned root observe matches `xml::parse_streaming` with
//! `split = ["row"]`, with one deliberate exception for invalid input: a `<row>` nested inside
//! another row's subtree is skipped here, where the generic reader would report it as its own row.

use std::borrow::Cow;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::IoError;
use crate::xml::{self, El, MAX_DEPTH};

/// A `<row>` element: attributes and cells, borrowing from the sheet XML. Reused across rows.
#[derive(Default)]
pub struct RowXml<'a> {
    pub attrs: Vec<(Cow<'a, str>, Cow<'a, str>)>,
    pub cells: Vec<CellXml<'a>>,
}

impl<'a> RowXml<'a> {
    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_ref())
    }
    pub fn attr_u32(&self, k: &str) -> Option<u32> {
        self.attr(k).and_then(|v| v.trim().parse::<u32>().ok())
    }
    pub fn attr_f64(&self, k: &str) -> Option<f64> {
        self.attr(k).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())
    }
    /// xsd:boolean (`1`/`true`/`0`/`false`).
    pub fn attr_bool(&self, k: &str) -> Option<bool> {
        match self.attr(k)?.trim() {
            "1" | "true" | "on" | "t" => Some(true),
            "0" | "false" | "off" | "f" => Some(false),
            _ => None,
        }
    }
    pub fn flag(&self, k: &str, default: bool) -> bool {
        self.attr_bool(k).unwrap_or(default)
    }
}

/// A `<c>` element, borrowing from the sheet XML. Only the attributes and children the reader
/// uses are kept; the first occurrence of each wins (as `El::child`/`El::attr` do).
#[derive(Default)]
pub struct CellXml<'a> {
    /// `r` (cell reference) attribute.
    pub r: Option<Cow<'a, str>>,
    /// `s` (style index) attribute.
    pub s: Option<Cow<'a, str>>,
    /// `t` (value type) attribute.
    pub t: Option<Cow<'a, str>>,
    /// `cm` (cell metadata) attribute: its presence marks a dynamic-array result.
    pub cm: Option<Cow<'a, str>>,
    /// `vm` (value metadata) attribute: index into the cell-picture blocks.
    pub vm: Option<Cow<'a, str>>,
    /// Text of the `<v>` child (`Some("")` for `<v/>`).
    pub v: Option<Cow<'a, str>>,
    /// The `<f>` child.
    pub f: Option<FormulaXml<'a>>,
    /// Concatenated text of the `<is>` rich-text child, entity-decoded but not yet
    /// `_xHHHH_`-decoded (apply `xml::decode_xstring`, as `xml::rich_text` does).
    pub is: Option<String>,
}

/// An `<f>` element, borrowing from the sheet XML.
#[derive(Default)]
pub struct FormulaXml<'a> {
    /// `t` attribute (`shared`, `array`, `dataTable`…).
    pub t: Option<Cow<'a, str>>,
    /// `si` (shared formula index) attribute.
    pub si: Option<Cow<'a, str>>,
    /// `ref` (array range) attribute.
    pub ref_: Option<Cow<'a, str>>,
    pub text: Cow<'a, str>,
}

/// A numeric attribute of a [`CellXml`]/[`FormulaXml`] (parsed like `El::attr_u32`).
pub fn u32_attr(v: &Option<Cow<'_, str>>) -> Option<u32> {
    v.as_deref().and_then(|v| v.trim().parse::<u32>().ok())
}

/// Local name (namespace prefix dropped) as bytes.
fn local(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => name.get(i + 1..).unwrap_or(name),
        None => name,
    }
}

/// Re-slices `s` — a subslice of `base`, as quick-xml's borrowed events point into the input —
/// with `base`'s lifetime. Copies when it isn't one (a slice reader always borrows, but stay
/// correct anyway).
fn reborrow<'a>(base: &'a [u8], s: &[u8]) -> Cow<'a, [u8]> {
    let sliced = (s.as_ptr() as usize)
        .checked_sub(base.as_ptr() as usize)
        .and_then(|o| o.checked_add(s.len()).map(|end| (o, end)))
        .and_then(|(o, end)| base.get(o..end));
    match sliced {
        Some(b) => Cow::Borrowed(b),
        None => Cow::Owned(s.to_vec()),
    }
}

fn reborrow_bytes<'a>(base: &'a [u8], s: &[u8]) -> Cow<'a, str> {
    match reborrow(base, s) {
        Cow::Borrowed(b) => String::from_utf8_lossy(b),
        Cow::Owned(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
    }
}

fn reborrow_str<'a>(base: &'a [u8], v: Cow<'_, str>) -> Cow<'a, str> {
    match v {
        Cow::Borrowed(s) => reborrow_bytes(base, s.as_bytes()),
        Cow::Owned(s) => Cow::Owned(s),
    }
}

/// The non-`xmlns` attributes of an element as (local name, normalized value), both borrowed
/// from the input when no decoding is needed.
fn each_attr<'a>(base: &'a [u8], e: &BytesStart<'_>, f: &mut dyn FnMut(Cow<'a, str>, Cow<'a, str>)) {
    for a in e.attributes().with_checks(false).flatten() {
        let key = a.key.as_ref();
        // Namespace declarations carry no data for us.
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue;
        }
        let v = match a.normalized_value(XmlVersion::Explicit1_0) {
            Ok(v) => reborrow_str(base, v),
            Err(_) => reborrow_bytes(base, &a.value),
        };
        f(reborrow_bytes(base, local(key)), v);
    }
}

fn cell_from<'a>(base: &'a [u8], e: &BytesStart<'_>) -> CellXml<'a> {
    let mut c = CellXml::default();
    each_attr(base, e, &mut |k, v| {
        let slot = match k.as_bytes() {
            b"r" => &mut c.r,
            b"s" => &mut c.s,
            b"t" => &mut c.t,
            b"cm" => &mut c.cm,
            b"vm" => &mut c.vm,
            _ => return,
        };
        if slot.is_none() {
            *slot = Some(v);
        }
    });
    c
}

fn formula_from<'a>(base: &'a [u8], e: &BytesStart<'_>) -> FormulaXml<'a> {
    let mut f = FormulaXml::default();
    each_attr(base, e, &mut |k, v| {
        let slot = match k.as_bytes() {
            b"t" => &mut f.t,
            b"si" => &mut f.si,
            b"ref" => &mut f.ref_,
            _ => return,
        };
        if slot.is_none() {
            *slot = Some(v);
        }
    });
    f
}

/// Appends a text chunk, keeping the slot borrowed when it is still empty.
fn append<'a>(slot: &mut Cow<'a, str>, chunk: Cow<'a, str>) {
    if slot.is_empty() {
        *slot = chunk;
    } else {
        slot.to_mut().push_str(&chunk);
    }
}

/// Which depth-3 child of a `<c>` is collecting text.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    None,
    V,
    F,
    Is,
}

/// The element seen at depth 4 inside `<is>` (a direct child: `<t>`, `<r>` or something else).
#[derive(Clone, Copy, PartialEq)]
enum IsChild {
    None,
    T,
    R,
    Other,
}

/// Streams a worksheet part: every `<row>` below the root goes to `cb` as a borrowed view,
/// everything else is returned as the root [`El`] tree. `row` buffers are reused across calls.
pub fn parse_sheet(part: &[u8], cb: &mut dyn FnMut(&RowXml<'_>) -> Result<(), IoError>) -> Result<El, IoError> {
    let data = xml::to_utf8(part);
    let base: &[u8] = data.as_ref();
    let mut reader = Reader::from_reader(base);
    {
        let cfg = reader.config_mut();
        cfg.check_end_names = false;
        cfg.allow_unmatched_ends = true;
        cfg.allow_dangling_amp = true;
        cfg.trim_text_start = false;
        cfg.trim_text_end = false;
    }
    let mut stack: Vec<El> = Vec::new();
    let mut root: Option<El> = None;
    let mut row = RowXml::default();
    // Attach a finished non-row element to its parent (or make it the root).
    fn finish(stack: &mut [El], root: &mut Option<El>, el: El) {
        match stack.last_mut() {
            Some(parent) => parent.children.push(el),
            None => {
                if root.is_none() {
                    *root = Some(el);
                }
            }
        }
    }
    loop {
        let ev = reader.read_event().map_err(|e| IoError::Xml(format!("{e} at byte {}", reader.buffer_position())))?;
        match ev {
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(IoError::Xml("XML nested too deeply".into()));
                }
                if !stack.is_empty() && local(e.name().as_ref()) == b"row" {
                    let closed = stream_row(&mut reader, base, &e, &mut row)?;
                    cb(&row)?;
                    if !closed {
                        // EOF inside the row: the tree reader would close the rest too.
                        break;
                    }
                } else {
                    stack.push(xml::start_el(&e));
                }
            }
            Event::Empty(e) => {
                if !stack.is_empty() && local(e.name().as_ref()) == b"row" {
                    row.attrs.clear();
                    row.cells.clear();
                    each_attr(base, &e, &mut |k, v| row.attrs.push((k, v)));
                    cb(&row)?;
                } else {
                    let el = xml::start_el(&e);
                    finish(&mut stack, &mut root, el);
                }
            }
            Event::End(_) => {
                if let Some(el) = stack.pop() {
                    finish(&mut stack, &mut root, el);
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let s = t.xml_content(XmlVersion::Explicit1_0).map_err(|e| IoError::Xml(e.to_string()))?;
                    top.text.push_str(&s);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    xml::push_ref(&mut top.text, &r);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // Unclosed elements (truncated part): close them so we keep what we have.
    while let Some(el) = stack.pop() {
        finish(&mut stack, &mut root, el);
    }
    root.ok_or_else(|| IoError::Xml("no root element".into()))
}

/// Reads one `<row>` subtree into `row`. Returns `false` when EOF cut the row short (the partial
/// row is kept, as the tree reader keeps unclosed elements).
fn stream_row<'a>(reader: &mut Reader<&'a [u8]>, base: &'a [u8], start: &BytesStart<'_>, row: &mut RowXml<'a>) -> Result<bool, IoError> {
    row.attrs.clear();
    row.cells.clear();
    each_attr(base, start, &mut |k, v| row.attrs.push((k, v)));
    let mut depth = 1usize;
    let mut cell: Option<CellXml<'a>> = None;
    let mut mode = Mode::None;
    let mut is4 = IsChild::None;
    let mut is5t = false;
    // Where the text of the current event belongs (nothing collects the direct text of `<row>`,
    // `<c>` or unknown elements: the reader never looks at it).
    macro_rules! text {
        ($chunk:expr) => {{
            let chunk = $chunk;
            match mode {
                Mode::V if depth == 3 => {
                    if let Some(v) = cell.as_mut().and_then(|c| c.v.as_mut()) {
                        append(v, chunk);
                    }
                }
                Mode::F if depth == 3 => {
                    if let Some(f) = cell.as_mut().and_then(|c| c.f.as_mut()) {
                        append(&mut f.text, chunk);
                    }
                }
                Mode::Is if (depth == 4 && is4 == IsChild::T) || (depth == 5 && is4 == IsChild::R && is5t) => {
                    if let Some(s) = cell.as_mut().and_then(|c| c.is.as_mut()) {
                        s.push_str(&chunk);
                    }
                }
                _ => {}
            }
        }};
    }
    loop {
        let ev = reader.read_event().map_err(|e| IoError::Xml(format!("{e} at byte {}", reader.buffer_position())))?;
        match ev {
            Event::Start(e) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(IoError::Xml("XML nested too deeply".into()));
                }
                match depth {
                    2 if local(e.name().as_ref()) == b"c" => cell = Some(cell_from(base, &e)),
                    3 => {
                        if let Some(c) = cell.as_mut() {
                            mode = match local(e.name().as_ref()) {
                                b"v" if c.v.is_none() => {
                                    c.v = Some(Cow::Borrowed(""));
                                    Mode::V
                                }
                                b"f" if c.f.is_none() => {
                                    c.f = Some(formula_from(base, &e));
                                    Mode::F
                                }
                                b"is" if c.is.is_none() => {
                                    c.is = Some(String::new());
                                    Mode::Is
                                }
                                _ => Mode::None,
                            };
                        }
                    }
                    4 if mode == Mode::Is => {
                        is4 = match local(e.name().as_ref()) {
                            b"t" => IsChild::T,
                            b"r" => IsChild::R,
                            _ => IsChild::Other,
                        };
                    }
                    5 if mode == Mode::Is && is4 == IsChild::R => is5t = local(e.name().as_ref()) == b"t",
                    _ => {}
                }
            }
            Event::Empty(e) => match depth {
                1 if local(e.name().as_ref()) == b"c" => row.cells.push(cell_from(base, &e)),
                2 => {
                    if let Some(c) = cell.as_mut() {
                        match local(e.name().as_ref()) {
                            b"v" if c.v.is_none() => c.v = Some(Cow::Borrowed("")),
                            b"f" if c.f.is_none() => c.f = Some(formula_from(base, &e)),
                            b"is" if c.is.is_none() => c.is = Some(String::new()),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Event::End(_) => {
                match depth {
                    2 => {
                        if let Some(c) = cell.take() {
                            row.cells.push(c);
                        }
                    }
                    3 => mode = Mode::None,
                    4 => {
                        is4 = IsChild::None;
                        is5t = false;
                    }
                    5 => is5t = false,
                    _ => {}
                }
                depth -= 1;
                if depth == 0 {
                    return Ok(true);
                }
            }
            Event::Text(t) => {
                // Decoded even where the reader ignores it, so invalid text fails like the tree
                // reader's.
                let chunk = t.xml_content(XmlVersion::Explicit1_0).map_err(|e| IoError::Xml(e.to_string()))?;
                text!(chunk);
            }
            Event::CData(t) => {
                let chunk = reborrow_bytes(base, &t);
                text!(chunk);
            }
            Event::GeneralRef(r) => match mode {
                Mode::V if depth == 3 => {
                    if let Some(v) = cell.as_mut().and_then(|c| c.v.as_mut()) {
                        xml::push_ref(v.to_mut(), &r);
                    }
                }
                Mode::F if depth == 3 => {
                    if let Some(f) = cell.as_mut().and_then(|c| c.f.as_mut()) {
                        xml::push_ref(f.text.to_mut(), &r);
                    }
                }
                Mode::Is if (depth == 4 && is4 == IsChild::T) || (depth == 5 && is4 == IsChild::R && is5t) => {
                    if let Some(s) = cell.as_mut().and_then(|c| c.is.as_mut()) {
                        xml::push_ref(s, &r);
                    }
                }
                _ => {}
            },
            Event::Eof => {
                if let Some(c) = cell.take() {
                    row.cells.push(c);
                }
                return Ok(false);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a sheet fragment and returns (rows seen, root child names).
    fn run(xml: &str) -> (Vec<(Vec<(String, String)>, Vec<CellXml<'static>>)>, El) {
        let mut rows = vec![];
        let root = parse_sheet(xml.as_bytes(), &mut |row| {
            let attrs = row.attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let cells = row
                .cells
                .iter()
                .map(|c| CellXml {
                    r: c.r.as_ref().map(|v| Cow::Owned(v.to_string())),
                    s: c.s.as_ref().map(|v| Cow::Owned(v.to_string())),
                    t: c.t.as_ref().map(|v| Cow::Owned(v.to_string())),
                    cm: c.cm.as_ref().map(|v| Cow::Owned(v.to_string())),
                    vm: c.vm.as_ref().map(|v| Cow::Owned(v.to_string())),
                    v: c.v.as_ref().map(|v| Cow::Owned(v.to_string())),
                    f: c.f.as_ref().map(|f| FormulaXml {
                        t: f.t.as_ref().map(|v| Cow::Owned(v.to_string())),
                        si: f.si.as_ref().map(|v| Cow::Owned(v.to_string())),
                        ref_: f.ref_.as_ref().map(|v| Cow::Owned(v.to_string())),
                        text: Cow::Owned(f.text.to_string()),
                    }),
                    is: c.is.clone(),
                })
                .collect();
            rows.push((attrs, cells));
            Ok(())
        });
        (rows, root.unwrap_or_default())
    }

    #[test]
    fn rows_and_cells() {
        let x = r#"<worksheet><sheetData><row r="1" ht="20"><c r="A1" s="2" t="s"><v>3</v></c><c r="B1"><v>1.5</v></c><c r="C1" t="str"><f>SUM(A1:B1)</f><v>4.5</v></c></row><row r="2"/></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#;
        let (rows, root) = run(x);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, [("r".to_string(), "1".to_string()), ("ht".to_string(), "20".to_string())]);
        let cells = &rows[0].1;
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].r.as_deref(), Some("A1"));
        assert_eq!(cells[0].s.as_deref(), Some("2"));
        assert_eq!(cells[0].t.as_deref(), Some("s"));
        assert_eq!(cells[0].v.as_deref(), Some("3"));
        assert_eq!(cells[1].v.as_deref(), Some("1.5"));
        assert_eq!(cells[2].f.as_ref().map(|f| f.text.as_ref()), Some("SUM(A1:B1)"));
        assert_eq!(cells[2].v.as_deref(), Some("4.5"));
        assert!(rows[1].1.is_empty());
        assert_eq!(root.name, "worksheet");
        let mc = root.child("mergeCells").and_then(|m| m.child("mergeCell")).expect("mergeCell");
        assert_eq!(mc.attr("ref"), Some("A1:B1"));
        assert!(root.child("sheetData").is_some_and(|s| s.children.is_empty()));
    }

    #[test]
    fn entities_and_rich_text() {
        let x = r#"<worksheet><sheetData><row><c r="A1" t="inlineStr"><is><t>a &lt; b</t><r><rPr><b/></rPr><t>cd</t></r><rPh><t>ignored</t></rPh></is></c><c r="B1" t="str"><v>x &gt; y &#65;</v></c><c r="C1" t="str"><v/></c><c r="D1"><f t="shared" si="4" ref="D1:D9">A1&amp;B1</f></c></row></sheetData></worksheet>"#;
        let (rows, _) = run(x);
        let cells = &rows[0].1;
        assert_eq!(cells[0].is.as_deref(), Some("a < bcd"));
        assert_eq!(cells[1].v.as_deref(), Some("x > y A"));
        assert_eq!(cells[2].v.as_deref(), Some(""));
        let f = cells[3].f.as_ref().expect("f");
        assert_eq!(f.t.as_deref(), Some("shared"));
        assert_eq!(f.si.as_deref(), Some("4"));
        assert_eq!(f.ref_.as_deref(), Some("D1:D9"));
        assert_eq!(f.text, "A1&B1");
    }

    #[test]
    fn truncated_row_keeps_cells() {
        let x = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v>"#;
        let (rows, root) = run(x);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.len(), 2);
        assert_eq!(rows[0].1[1].v.as_deref(), Some("2"));
        assert_eq!(root.name, "worksheet");
    }

    #[test]
    fn deep_nesting_in_row_is_an_error() {
        let mut s = String::from("<worksheet><sheetData><row>");
        for _ in 0..MAX_DEPTH {
            s.push_str("<a>");
        }
        let mut n = 0;
        let res = parse_sheet(s.as_bytes(), &mut |_| {
            n += 1;
            Ok(())
        });
        assert!(res.is_err());
        assert_eq!(n, 0);
    }

    #[test]
    fn cdata_and_unknown_refs() {
        let x = "<worksheet><sheetData><row><c r=\"A1\" t=\"str\"><v><![CDATA[a<b>]]>&bogus;</v></c></row></sheetData></worksheet>";
        let (rows, _) = run(x);
        assert_eq!(rows[0].1[0].v.as_deref(), Some("a<b>&bogus;"));
    }
}
