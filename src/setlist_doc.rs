//! The printed setlist of a playlist as a Word document (.docx, Office Open
//! XML; Word and LibreOffice open it).
//!
//! One A4 page: the heading ("Rainbow - 07.11.2026") with a thick line under
//! it and the band logo on the right, then the songs of the concert as a
//! numbered list in large letters, a thick line before the encores. A song
//! shows `Title (Artist)` with the title bold. The font size is chosen so
//! the songs fill the page; a line that would be too wide gets a smaller
//! size of its own. The document stays editable: real paragraphs, a Word
//! list for the numbers, no text boxes.

use std::io::{Cursor, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// A4 in twips (1/20 pt).
const PAGE_W: u32 = 11906;
const PAGE_H: u32 = 16838;
/// 1 cm on every side
const MARGIN: u32 = 567;
const TEXT_W: u32 = PAGE_W - 2 * MARGIN;
/// height of the heading row (the logo is scaled to it): 2.8 cm
const HEAD_H: u32 = 1588;
/// space between the heading and the first song
const HEAD_GAP: u32 = 120;
/// left free at the bottom of the page, so Word never breaks the page
const SAFETY: u32 = 500;
/// largest and smallest font size of the songs (pt)
const MAX_PT: f64 = 40.0;
const MIN_PT: f64 = 10.0;
/// heading font size (pt)
const HEAD_PT: f64 = 28.0;
/// thickness of the lines in 1/8 pt
const LINE_SZ: u32 = 28;
const FONT: &str = "Arial";

/// The band logo (PNG or JPEG).
#[derive(Clone)]
pub struct Logo {
    bytes: Vec<u8>,
    ext: &'static str,
    width: u32,
    height: u32,
}

impl Logo {
    pub fn read(path: &Path) -> Result<Logo, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        Logo::from_bytes(bytes).ok_or_else(|| crate::tr!("print.bad_logo", path = path.display()))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Option<Logo> {
        let (ext, width, height) = image_size(&bytes)?;
        Some(Logo { bytes, ext, width, height })
    }
}

/// File type and pixel size of a PNG or JPEG.
fn image_size(b: &[u8]) -> Option<(&'static str, u32, u32)> {
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
    let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32) };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        let (w, h) = (be32(16)?, be32(20)?);
        return (w > 0 && h > 0).then_some(("png", w, h));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xFF {
                return None;
            }
            let marker = b[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            let len = be16(i + 2)? as usize;
            // SOF0..SOF15 without DHT (C4), JPG (C8), DAC (CC)
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                let (h, w) = (be16(i + 5)?, be16(i + 7)?);
                return (w > 0 && h > 0).then_some(("jpeg", w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// What goes on the page.
pub struct SetlistDoc {
    /// the heading, e.g. "Rainbow - 07.11.2026"
    pub heading: String,
    /// the songs, as printed: "Title (Artist)"
    pub songs: Vec<String>,
    /// index into `songs` of the first encore (the line goes above it)
    pub encore_at: Option<usize>,
    pub logo: Option<Logo>,
}

/// The heading proposed for a playlist: "2026-11-07_Rainbow" ->
/// "Rainbow - 07.11.2026"; a name without a leading date stays as it is
/// (with spaces for `_`).
pub fn default_heading(playlist: &str) -> String {
    let spaced = |s: &str| s.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    let b = playlist.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).map(|d| d.iter().all(u8::is_ascii_digit)).unwrap_or(false);
    if digits(0..4) && b.get(4) == Some(&b'-') && digits(5..7) && b.get(7) == Some(&b'-') && digits(8..10) {
        let date = format!("{}.{}.{}", &playlist[8..10], &playlist[5..7], &playlist[0..4]);
        let rest = spaced(playlist[10..].trim_start_matches(['_', '-', ' ']));
        return if rest.is_empty() { date } else { format!("{} - {}", rest, date) };
    }
    spaced(playlist)
}

/// "I love it (Icona Pop)" -> ("I love it", " (Icona Pop)"): the part in
/// brackets at the end is printed normal, the rest bold.
pub fn split_title(text: &str) -> (&str, &str) {
    let t = text.trim();
    if t.ends_with(')') {
        if let Some(i) = t.rfind('(') {
            if i > 0 {
                let bold = t[..i].trim_end();
                return (bold, &t[bold.len()..]);
            }
        }
    }
    (t, "")
}

// ---------------------------------------------------------------------------
// text width (Arial = Helvetica metrics, 1/1000 em)
// ---------------------------------------------------------------------------

const REGULAR: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556,
    556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334,
    260, 334, 584,
];
const BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611,
    556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389,
    280, 389, 584,
];

/// Width of `text` in em.
fn text_em(text: &str, bold: bool) -> f64 {
    let table = if bold { &BOLD } else { &REGULAR };
    let w = |c: char| -> u16 {
        let base = match c {
            'ä' | 'á' | 'à' | 'â' => 'a',
            'Ä' | 'Á' | 'À' | 'Â' => 'A',
            'ö' | 'ó' | 'ò' | 'ô' => 'o',
            'Ö' | 'Ó' | 'Ò' | 'Ô' => 'O',
            'ü' | 'ú' | 'ù' | 'û' => 'u',
            'Ü' | 'Ú' | 'Ù' | 'Û' => 'U',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'É' | 'È' | 'Ê' => 'E',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ç' => 'c',
            '’' | '‘' | '´' | '`' => '\'',
            '“' | '”' | '„' => '"',
            '–' => '-',
            c => c,
        };
        match base as u32 {
            32..=126 => table[base as usize - 32],
            _ if base == 'ß' => 611,
            _ if base == '—' => 1000,
            _ => 667,
        }
    };
    text.chars().map(|c| w(c) as f64).sum::<f64>() / 1000.0
}

/// Width of a song line in pt at `pt`: number column + text.
fn line_width(number_em: f64, text: &str, pt: f64) -> f64 {
    let (bold, rest) = split_title(text);
    (number_em + text_em(bold, true) + text_em(rest, false)) * pt
}

/// Width of the number column in em: the widest number + "." + a gap.
fn number_em(count: usize) -> f64 {
    let digits = count.max(1).to_string().len();
    digits as f64 * 0.556 + 0.278 + 0.3
}

/// Font size (pt, in half points) of every song: as large as the page
/// allows, each line smaller if it would not fit the width.
fn sizes(songs: &[String], encore: bool) -> (f64, Vec<f64>) {
    let n = songs.len().max(1) as f64;
    let line_extra = if encore { 160.0 } else { 0.0 };
    let avail = (PAGE_H - 2 * MARGIN - HEAD_H - HEAD_GAP - SAFETY) as f64 - line_extra;
    let base = (avail / n / 20.0).clamp(MIN_PT, MAX_PT);
    let base = (base * 2.0).floor() / 2.0;
    let width_pt = TEXT_W as f64 / 20.0 * 0.97;
    let num = number_em(songs.len());
    let each = songs
        .iter()
        .map(|s| {
            let w = line_width(num, s, base);
            let pt = if w > width_pt { base * width_pt / w } else { base };
            ((pt * 2.0).floor() / 2.0).max(MIN_PT)
        })
        .collect();
    (base, each)
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn half_points(pt: f64) -> u32 {
    (pt * 2.0).round() as u32
}

fn run(text: &str, bold: bool, pt: f64) -> String {
    if text.is_empty() {
        return String::new();
    }
    let sz = half_points(pt);
    format!(
        "<w:r><w:rPr>{}<w:sz w:val=\"{sz}\"/><w:szCs w:val=\"{sz}\"/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>",
        if bold { "<w:b/><w:bCs/>" } else { "" },
        esc(text)
    )
}

const EMU_PER_TWIP: u64 = 635;

fn logo_xml(logo: &Logo) -> (String, u32) {
    // as high as the heading row, at most a third of the width
    let mut h = HEAD_H as u64 - 60;
    let mut w = h * logo.width as u64 / logo.height as u64;
    let max_w = TEXT_W as u64 / 3;
    if w > max_w {
        h = h * max_w / w;
        w = max_w;
    }
    let (cx, cy) = (w * EMU_PER_TWIP, h * EMU_PER_TWIP);
    let xml = format!(
        "<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{cx}\" cy=\"{cy}\"/>\
<wp:docPr id=\"1\" name=\"Logo\"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
<a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic>\
<pic:nvPicPr><pic:cNvPr id=\"1\" name=\"logo.{ext}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"rIdLogo\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>",
        ext = logo.ext
    );
    (xml, w as u32)
}

fn heading_xml(doc: &SetlistDoc) -> String {
    let line = format!("w:val=\"single\" w:sz=\"{}\" w:space=\"1\" w:color=\"000000\"", LINE_SZ);
    let title = format!(
        "<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"40\"/><w:ind w:left=\"200\"/></w:pPr>{}</w:p>",
        run(&doc.heading, false, HEAD_PT)
    );
    let Some(logo) = &doc.logo else {
        // no logo: the heading with the line under it, as high as with a logo
        return format!(
            "<w:p><w:pPr><w:pBdr><w:bottom {line}/></w:pBdr><w:spacing w:before=\"{}\" w:after=\"{HEAD_GAP}\"/><w:ind w:left=\"200\"/></w:pPr>{}</w:p>",
            HEAD_H.saturating_sub((HEAD_PT * 20.0 * 1.2) as u32),
            run(&doc.heading, false, HEAD_PT)
        );
    };
    // a table without borders: heading (line under it) | logo
    let (pic, logo_w) = logo_xml(logo);
    let c2 = logo_w + 280;
    let c1 = TEXT_W - c2;
    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"{TEXT_W}\" w:type=\"dxa\"/><w:tblLayout w:type=\"fixed\"/>\
<w:tblCellMar><w:left w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"0\" w:type=\"dxa\"/></w:tblCellMar><w:tblLook w:val=\"0000\"/></w:tblPr>\
<w:tblGrid><w:gridCol w:w=\"{c1}\"/><w:gridCol w:w=\"{c2}\"/></w:tblGrid>\
<w:tr><w:trPr><w:trHeight w:val=\"{HEAD_H}\"/></w:trPr>\
<w:tc><w:tcPr><w:tcW w:w=\"{c1}\" w:type=\"dxa\"/><w:tcBorders><w:bottom {line}/></w:tcBorders><w:vAlign w:val=\"bottom\"/></w:tcPr>{title}</w:tc>\
<w:tc><w:tcPr><w:tcW w:w=\"{c2}\" w:type=\"dxa\"/><w:vAlign w:val=\"bottom\"/></w:tcPr>\
<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\"/><w:jc w:val=\"right\"/></w:pPr>{pic}</w:p></w:tc></w:tr></w:tbl>\
<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"{HEAD_GAP}\" w:lineRule=\"exact\"/><w:rPr><w:sz w:val=\"2\"/></w:rPr></w:pPr></w:p>"
    )
}

fn document_xml(doc: &SetlistDoc) -> String {
    let encore = doc.encore_at.filter(|&e| e > 0 && e < doc.songs.len());
    let (base, each) = sizes(&doc.songs, encore.is_some());
    let line_h = (base * 20.0).round() as u32;
    let num_em = number_em(doc.songs.len());
    let mut body = heading_xml(doc);
    for (i, (song, pt)) in doc.songs.iter().zip(&each).enumerate() {
        let (bold, rest) = split_title(song);
        let indent = (num_em * pt * 20.0).round() as u32;
        let border = if encore == Some(i) {
            format!("<w:pBdr><w:top w:val=\"single\" w:sz=\"{}\" w:space=\"1\" w:color=\"000000\"/></w:pBdr>", LINE_SZ)
        } else {
            String::new()
        };
        let sz = half_points(*pt);
        body.push_str(&format!(
            "<w:p><w:pPr><w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"1\"/></w:numPr>{border}\
<w:spacing w:before=\"0\" w:after=\"0\" w:line=\"{line_h}\" w:lineRule=\"exact\"/>\
<w:ind w:left=\"{indent}\" w:hanging=\"{indent}\"/><w:rPr><w:sz w:val=\"{sz}\"/><w:szCs w:val=\"{sz}\"/></w:rPr></w:pPr>{}{}</w:p>",
            run(bold, true, *pt),
            run(rest, false, *pt)
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><w:body>{body}\
<w:sectPr><w:pgSz w:w=\"{PAGE_W}\" w:h=\"{PAGE_H}\"/>\
<w:pgMar w:top=\"{MARGIN}\" w:right=\"{MARGIN}\" w:bottom=\"{MARGIN}\" w:left=\"{MARGIN}\" w:header=\"0\" w:footer=\"0\" w:gutter=\"0\"/>\
</w:sectPr></w:body></w:document>"
    )
}

fn styles_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"{FONT}\" w:hAnsi=\"{FONT}\" w:cs=\"{FONT}\" w:eastAsia=\"{FONT}\"/>\
<w:color w:val=\"000000\"/><w:sz w:val=\"24\"/><w:szCs w:val=\"24\"/><w:lang w:val=\"de-DE\"/></w:rPr></w:rPrDefault>\
<w:pPrDefault><w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
<w:style w:type=\"table\" w:default=\"1\" w:styleId=\"TableNormal\"><w:name w:val=\"Normal Table\"/>\
<w:tblPr><w:tblInd w:w=\"0\" w:type=\"dxa\"/><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"0\" w:type=\"dxa\"/>\
<w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"0\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style>\
</w:styles>"
    )
}

fn numbering_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:abstractNum w:abstractNumId=\"0\"><w:multiLevelType w:val=\"singleLevel\"/>\
<w:lvl w:ilvl=\"0\"><w:start w:val=\"1\"/><w:numFmt w:val=\"decimal\"/><w:lvlText w:val=\"%1.\"/><w:lvlJc w:val=\"left\"/>\
<w:pPr><w:ind w:left=\"720\" w:hanging=\"720\"/></w:pPr></w:lvl></w:abstractNum>\
<w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num></w:numbering>"
}

/// Current Word layout; without it Word opens the file in compatibility mode.
fn settings_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:compat><w:compatSetting w:name=\"compatibilityMode\" w:uri=\"http://schemas.microsoft.com/office/word\" w:val=\"15\"/></w:compat>\
</w:settings>"
}

/// The .docx file of `doc`.
pub fn docx(doc: &SetlistDoc) -> std::io::Result<Vec<u8>> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let file = |zip: &mut ZipWriter<Cursor<Vec<u8>>>, name: &str, data: &[u8]| -> std::io::Result<()> {
        zip.start_file(name, opt).map_err(std::io::Error::other)?;
        zip.write_all(data)
    };
    let image_type = match doc.logo.as_ref().map(|l| l.ext) {
        Some(ext) => format!("<Default Extension=\"{ext}\" ContentType=\"image/{ext}\"/>"),
        None => String::new(),
    };
    file(
        &mut zip,
        "[Content_Types].xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{image_type}\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>\
<Override PartName=\"/word/settings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>\
</Types>"
        )
        .as_bytes(),
    )?;
    file(
        &mut zip,
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>",
    )?;
    let image_rel = match doc.logo.as_ref().map(|l| l.ext) {
        Some(ext) => format!(
            "<Relationship Id=\"rIdLogo\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/logo.{ext}\"/>"
        ),
        None => String::new(),
    };
    file(
        &mut zip,
        "word/_rels/document.xml.rels",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>\
{image_rel}</Relationships>"
        )
        .as_bytes(),
    )?;
    file(&mut zip, "word/document.xml", document_xml(doc).as_bytes())?;
    file(&mut zip, "word/styles.xml", styles_xml().as_bytes())?;
    file(&mut zip, "word/numbering.xml", numbering_xml().as_bytes())?;
    file(&mut zip, "word/settings.xml", settings_xml().as_bytes())?;
    if let Some(logo) = &doc.logo {
        file(&mut zip, &format!("word/media/logo.{}", logo.ext), &logo.bytes)?;
    }
    Ok(zip.finish().map_err(std::io::Error::other)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings() {
        assert_eq!(default_heading("2026-11-07_Rainbow"), "Rainbow - 07.11.2026");
        assert_eq!(default_heading("2026-11-07"), "07.11.2026");
        assert_eq!(default_heading("2026-11_Nerds_alle"), "2026-11 Nerds alle");
        assert_eq!(default_heading("Probe"), "Probe");
    }

    #[test]
    fn bold_title_normal_artist() {
        assert_eq!(split_title("I love it  (Icona Pop)"), ("I love it", "  (Icona Pop)"));
        assert_eq!(split_title("Kids"), ("Kids", ""));
        assert_eq!(split_title("(intro)"), ("(intro)", ""));
        assert_eq!(split_title("Song (a) b"), ("Song (a) b", ""));
    }

    #[test]
    fn font_fills_the_page_and_long_lines_shrink() {
        let songs: Vec<String> = (0..18).map(|i| format!("Song {} (Band)", i)).collect();
        let (base, each) = sizes(&songs, true);
        assert!(base > 34.0 && base <= MAX_PT, "{}", base);
        assert!(each.iter().all(|&p| p == base));
        let few = vec!["A".to_string(); 5];
        assert_eq!(sizes(&few, false).0, MAX_PT);
        let long = vec!["A very very long title of a song (With A Long Band Name)".to_string()];
        let (base, each) = sizes(&long, false);
        assert!(each[0] < base);
        assert!(line_width(number_em(1), &long[0], each[0]) <= TEXT_W as f64 / 20.0);
    }

    #[test]
    fn image_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&275u32.to_be_bytes());
        png.extend_from_slice(&346u32.to_be_bytes());
        assert_eq!(image_size(&png), Some(("png", 275, 346)));
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0, 50, 0, 100, 3];
        assert_eq!(image_size(&jpg), Some(("jpeg", 100, 50)));
        assert_eq!(image_size(b"GIF89a"), None);
    }

    #[test]
    fn document_is_a_zip_with_the_songs() {
        let doc = SetlistDoc {
            heading: "Rainbow - 07.11.2026".into(),
            songs: vec!["I love it (Icona Pop)".into(), "Rock & Roll <live>".into()],
            encore_at: Some(1),
            logo: None,
        };
        let bytes = docx(&doc).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("word/document.xml").unwrap(), &mut xml).unwrap();
        assert!(xml.contains("I love it</w:t>") && xml.contains(" (Icona Pop)</w:t>"));
        assert!(xml.contains("Rock &amp; Roll &lt;live&gt;"));
        assert_eq!(xml.matches("<w:top ").count(), 1);
        assert!(zip.by_name("word/numbering.xml").is_ok());
    }
}
