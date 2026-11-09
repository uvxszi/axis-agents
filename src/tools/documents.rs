//! Real document files: PDF and DOCX built from the model's markdown-ish
//! text, with images (from its searches) embedded. The model writes the
//! content once with write_file; `![alt](url)` lines become embedded
//! pictures; `#`/`##`/`###` become headings.

use serde_json::Value;

use super::ToolCtx;

/// A parsed block of the document.
#[derive(Debug, Clone)]
enum Block {
    Heading(u8, String),
    Para(String),
    Image(String),
}

fn parse(content: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("### ") {
            blocks.push(Block::Heading(3, rest.to_owned()));
        } else if let Some(rest) = trimmed.strip_prefix("## ") {
            blocks.push(Block::Heading(2, rest.to_owned()));
        } else if let Some(rest) = trimmed.strip_prefix("# ") {
            blocks.push(Block::Heading(1, rest.to_owned()));
        } else if trimmed.starts_with("![") {
            if let Some(start) = trimmed.find("](") {
                let url = trimmed[start + 2..].trim_end_matches(')').to_owned();
                if url.starts_with("http") {
                    blocks.push(Block::Image(url));
                }
            }
        } else {
            blocks.push(Block::Para(trimmed.to_owned()));
        }
    }
    blocks
}

/// One embedded image: JPEG bytes + pixel dimensions.
struct Picture {
    jpeg: Vec<u8>,
    width: u32,
    height: u32,
}

async fn load_picture(url: &str) -> Result<Picture, String> {
    let bytes = super::web::fetch_bytes(url, false, 8_000_000).await?;
    let image = super::images::decode(&bytes)?;
    let (width, height) = (image.width(), image.height());
    // Everything is embedded as JPEG (PDF passthrough; DOCX simplicity).
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image
        .to_rgb8()
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| format!("could not encode image: {e}"))?;
    Ok(Picture {
        jpeg: jpeg.into_inner(),
        width,
        height,
    })
}

/// Build the document named by `name` from markdown-ish `content`.
pub async fn build(ctx: &ToolCtx, name: &str, content: &str) -> Result<Vec<u8>, String> {
    let blocks = parse(content);
    // Fetch the images up front (they take both formats).
    let mut pictures: Vec<Picture> = Vec::new();
    for block in &blocks {
        if let Block::Image(url) = block {
            match load_picture(url).await {
                Ok(picture) => pictures.push(picture),
                Err(error) => tracing::warn!(url = %url, error = %error, "document image skipped"),
            }
        }
    }
    if name.to_lowercase().ends_with(".pdf") {
        Ok(make_pdf(&blocks, &pictures))
    } else {
        make_docx(&blocks, &pictures)
    }
}

// --- PDF (hand-rolled: catalog, pages, Helvetica text, JPEG XObjects) ------

fn pdf_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            _ => out.push(c),
        }
    }
    out
}

fn make_pdf(blocks: &[Block], pictures: &[Picture]) -> Vec<u8> {
    const W: f64 = 595.0;
    const H: f64 = 842.0;
    const MARGIN: f64 = 50.0;
    const BODY: f64 = 11.0;
    const LINE: f64 = 15.0;

    // Layout into page content streams.
    let mut pages: Vec<String> = Vec::new();
    let mut page = String::new();
    let mut y = H - MARGIN;
    let mut pic_index = 0usize;
    let mut new_page = |page: &mut String, pages: &mut Vec<String>, y: &mut f64| {
        pages.push(std::mem::take(page));
        *y = H - MARGIN;
    };
    for block in blocks {
        match block {
            Block::Heading(level, text) => {
                let size = match level {
                    1 => 18.0,
                    2 => 15.0,
                    _ => 13.0,
                };
                if y < MARGIN + size {
                    new_page(&mut page, &mut pages, &mut y);
                }
                y -= size + 2.0;
                page.push_str(&format!(
                    "BT /F2 {size} Tf {} {} Td ({}) Tj ET\n",
                    MARGIN,
                    y,
                    pdf_escape(text)
                ));
            }
            Block::Para(text) => {
                // crude but honest wrap at ~90 chars per line
                let mut line = String::new();
                for word in text.split_whitespace() {
                    if line.chars().count() + word.chars().count() > 90 {
                        if y < MARGIN + BODY {
                            new_page(&mut page, &mut pages, &mut y);
                        }
                        y -= LINE;
                        page.push_str(&format!(
                            "BT /F1 {BODY} Tf {} {} Td ({}) Tj ET\n",
                            MARGIN,
                            y,
                            pdf_escape(&line)
                        ));
                        line.clear();
                    }
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(word);
                }
                if !line.is_empty() {
                    if y < MARGIN + BODY {
                        new_page(&mut page, &mut pages, &mut y);
                    }
                    y -= LINE;
                    page.push_str(&format!(
                        "BT /F1 {BODY} Tf {} {} Td ({}) Tj ET\n",
                        MARGIN,
                        y,
                        pdf_escape(&line)
                    ));
                }
            }
            Block::Image(_) => {
                let Some(picture) = pictures.get(pic_index) else {
                    continue;
                };
                pic_index += 1;
                let display_w = W - 2.0 * MARGIN;
                let display_h = display_w * picture.height as f64 / picture.width.max(1) as f64;
                if y < MARGIN + display_h {
                    new_page(&mut page, &mut pages, &mut y);
                }
                y -= display_h + 8.0;
                page.push_str(&format!(
                    "q {display_w} 0 0 {display_h} {} {} cm /Im{pic_index} Do Q\n",
                    MARGIN,
                    y + 8.0
                ));
            }
        }
    }
    pages.push(page);

    // Assemble the objects.
    let mut objects: Vec<Vec<u8>> = Vec::new();
    // 1: catalog, 2: pages (filled later), 3/4: fonts.
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(Vec::new()); // pages tree, patched below
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_vec());
    // images
    let mut image_ids = Vec::new();
    for picture in pictures {
        let id = objects.len() + 1;
        image_ids.push(id);
        objects.push(format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>stream\n",
            picture.width,
            picture.height,
            picture.jpeg.len()
        )
        .into_bytes());
        let mut with_data = objects.pop().unwrap();
        with_data.extend_from_slice(&picture.jpeg);
        with_data.extend_from_slice(b"\nendstream");
        objects.push(with_data);
    }
    // page objects + content streams
    let first_page_id = objects.len() + 1;
    let mut page_ids = Vec::new();
    for content in &pages {
        let page_id = objects.len() + 1;
        page_ids.push(page_id);
        objects.push(Vec::new()); // page, patched after
        let content_id = objects.len() + 1;
        objects.push(
            format!(
                "<< /Length {} >>stream\n{content}\nendstream",
                content.len()
            )
            .into_bytes(),
        );
    }
    // patch pages tree and page dicts
    let kids: Vec<String> = page_ids.iter().map(|id| format!("{id} 0 R")).collect();
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        page_ids.len()
    )
    .into_bytes();
    let mut xrefs = vec![objects.len() as u64 + 1];
    for (n, page_id) in page_ids.iter().enumerate() {
        let content_id = first_page_id + page_ids.len() + n * 1; // page then content pairs? keep simple below
        let _ = content_id;
        let content_id = first_page_id + page_ids.len() + n;
        let resources = if image_ids.is_empty() {
            "<< /Font << /F1 3 0 R /F2 4 0 R >> >>".to_owned()
        } else {
            let xobjects: Vec<String> = image_ids
                .iter()
                .enumerate()
                .map(|(n, id)| format!("/Im{} {} 0 R", n + 1, id))
                .collect();
            format!(
                "<< /Font << /F1 3 0 R /F2 4 0 R >> /XObject << {} >> >>",
                xobjects.join(" ")
            )
        };
        objects[page_id - 1] = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {W} {H}] /Resources {resources} /Contents {} 0 R >>",
            content_id
        )
        .into_bytes();
    }

    // serialize
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (n, object) in objects.iter().enumerate() {
        offsets.push(out.len() as u64);
        out.extend_from_slice(format!("{} 0 obj\n", n + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let startxref = out.len() as u64;
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

// --- DOCX (a ZIP of OOXML parts) -------------------------------------------

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn make_docx(blocks: &[Block], pictures: &[Picture]) -> Result<Vec<u8>, String> {
    // media + relationships
    let mut media: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rels = String::new();
    for (n, picture) in pictures.iter().enumerate() {
        let name = format!("image{}.jpeg", n + 1);
        rels.push_str(&format!(
            "<Relationship Id=\"rIdIm{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{name}\"/>",
            n + 1
        ));
        media.push((name, picture.jpeg.clone()));
    }
    let mut body = String::new();
    let mut pic_index = 0usize;
    for block in blocks {
        match block {
            Block::Heading(level, text) => {
                let size = [32u32, 26, 24][(*level as usize).min(2)];
                body.push_str(&format!(
                    "<w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr><w:r><w:rPr><w:b/><w:sz w:val=\"{size}\"/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                    xml_escape(text)
                ));
            }
            Block::Para(text) => {
                body.push_str(&format!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                    xml_escape(text)
                ));
            }
            Block::Image(_) => {
                let Some(picture) = pictures.get(pic_index) else {
                    continue;
                };
                pic_index += 1;
                // display width 6 inches (9144000 EMU), height by aspect
                let emu_w: u64 = 5_486_400; // ~6in
                let emu_h =
                    (emu_w as f64 * picture.height as f64 / picture.width.max(1) as f64) as u64;
                let px_w = picture.width;
                let px_h = picture.height;
                body.push_str(&format!(
                    "<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{emu_w}\" cy=\"{emu_h}\"/><wp:docPr id=\"{}\" name=\"Picture {}\"/><a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:nvPicPr><pic:cNvPr id=\"{}\" name=\"image{pic_index}\"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"rIdIm{pic_index}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{emu_w}\" cy=\"{emu_h}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
                    pic_index, pic_index, pic_index
                ));
                let _ = (px_w, px_h);
            }
        }
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\"><w:body>{body}<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr></w:body></w:document>"
    );
    let rels_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{}<Relationship Id=\"rIdDoc\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>",
        rels
    );
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>";
    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>";

    // Zip it (stored entries — Word accepts them).
    let mut out: Vec<u8> = Vec::new();
    let mut crc_entries: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".to_owned(),
            content_types.as_bytes().to_vec(),
        ),
        ("_rels/.rels".to_owned(), root_rels.as_bytes().to_vec()),
        ("word/document.xml".to_owned(), document.into_bytes()),
        (
            "word/_rels/document.xml.rels".to_owned(),
            rels_xml.into_bytes(),
        ),
    ];
    for (name, bytes) in media {
        crc_entries.push((format!("word/media/{name}"), bytes));
    }
    let mut offsets: Vec<u64> = Vec::new();
    let mut sizes: Vec<u32> = Vec::new();
    let mut crcs: Vec<u32> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (name, bytes) in &crc_entries {
        offsets.push(out.len() as u64);
        sizes.push(bytes.len() as u32);
        crcs.push(crc32(bytes));
        names.push(name.clone());
        let header = local_header(name, bytes.len() as u32, crc32(bytes));
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
    }
    let cd_start = out.len() as u64;
    for (n, name) in names.iter().enumerate() {
        out.extend_from_slice(&central_header(name, sizes[n], crcs[n], offsets[n]));
    }
    let cd_size = out.len() as u64 - cd_start;
    out.extend_from_slice(&end_record(names.len() as u16, cd_size, cd_start));
    Ok(out)
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        table
    })
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = crc_table()[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn dos_time() -> (u16, u16) {
    (0, 0) // fixed timestamp keeps builds deterministic
}

fn local_header(name: &str, size: u32, crc: u32) -> Vec<u8> {
    let (date, time) = dos_time();
    let mut out = Vec::new();
    out.extend_from_slice(&0x04034b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // stored
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(&date.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

fn central_header(name: &str, size: u32, crc: u32, offset: u64) -> Vec<u8> {
    let (date, time) = dos_time();
    let mut out = Vec::new();
    out.extend_from_slice(&0x02014b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(&date.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

fn end_record(count: u16, cd_size: u64, cd_start: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(cd_size as u32).to_le_bytes());
    out.extend_from_slice(&(cd_start as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// write_file hook: does this name want a real document?
pub fn is_document(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".pdf") || lower.ends_with(".docx")
}

/// Build and hand back the bytes for write_file.
pub async fn write_document(
    ctx: &ToolCtx,
    name: &str,
    content: &str,
    args: &Value,
) -> Result<Vec<u8>, String> {
    let _ = (ctx, args);
    build(ctx, name, content).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_has_structures() {
        let pdf = make_pdf(
            &[
                Block::Heading(1, "Hi".into()),
                Block::Para("one two".into()),
            ],
            &[],
        );
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
    }

    #[test]
    fn docx_zip_is_well_formed() {
        let docx = make_docx(&[Block::Para("hello".into())], &[]).unwrap();
        assert!(docx.windows(4).any(|w| w == b"PK\x03\x04"));
        assert!(docx.windows(4).any(|w| w == b"PK\x05\x06"));
        assert!(docx.windows(5).any(|w| w == b"hello"));
    }

    #[test]
    fn markdownish_parses() {
        let blocks = parse("# Title\n\ntext line\n![pic](https://x/y.jpg)");
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        assert!(matches!(blocks[2], Block::Image(_)));
    }
}
