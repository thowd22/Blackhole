//! Layout-aware PDF text: glyphs are collected with their page positions and
//! re-assembled into rows (by baseline) and columns (by horizontal gaps), so a
//! form's labels and values come out on consecutive, aligned lines instead of
//! in content-stream order. `pdf-extract`'s plain writer turns a customs entry
//! into "S\nH\nI\nP\nP\nE\nR" and detached value clumps; this gives the model
//! `IMPORTING CARRIER | FROM PORT OF` over `TRANQUIL ACE 0124A | KOBE, JAPAN`.

use pdf_extract::{ColorSpace, MediaBox, OutputDev, OutputError, Path as PdfPath, Transform};

#[derive(Clone)]
struct Glyph {
    x: f64,
    y: f64,
    end: f64,
    size: f64,
    text: String,
}

pub struct LayoutOutput {
    flip: Transform,
    glyphs: Vec<Glyph>,
    pub out: String,
    /// Debug: print every glyph whose text occurs in this string.
    pub debug: Option<String>,
}

impl LayoutOutput {
    pub fn new() -> Self {
        LayoutOutput { flip: Transform::identity(), glyphs: Vec::new(), out: String::new(), debug: None }
    }

    fn flush_page(&mut self) {
        let mut glyphs = std::mem::take(&mut self.glyphs);
        if glyphs.is_empty() {
            return;
        }
        // Rows: sort by baseline, start a new row when the baseline moves by more
        // than half a glyph height.
        glyphs.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal));
        let mut rows: Vec<Vec<Glyph>> = Vec::new();
        for g in glyphs {
            match rows.last_mut() {
                Some(row) if (g.y - row[0].y).abs() <= row[0].size.max(g.size) * 0.5 => row.push(g),
                _ => rows.push(vec![g]),
            }
        }
        let mut lines: Vec<String> = Vec::new();
        let mut prev_y: Option<(f64, f64)> = None;
        for mut row in rows {
            // A vertical gap well beyond the line pitch is a paragraph break; keep it as a
            // blank line so the chunker still sees paragraphs.
            let (y, size) = (row[0].y, row.iter().map(|g| g.size).fold(0.0, f64::max));
            if let Some((py, ps)) = prev_y {
                if y - py > ps.max(size) * 1.9 && !lines.is_empty() {
                    lines.push(String::new());
                }
            }
            prev_y = Some((y, size));
            row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
            let mut line = String::new();
            let mut last_end = f64::NEG_INFINITY;
            let mut last_size = 0.0f64;
            let mut last: Option<Glyph> = None;
            for g in row {
                // The same glyph drawn again at (nearly) the same spot — fake bold, or a
                // page whose content stream is painted twice. Keep one.
                if let Some(l) = &last {
                    // Threshold relative to the glyph's own advance so "ll" survives.
                    if l.text == g.text && (g.x - l.x).abs() < (l.end - l.x).abs().max(0.05) * 0.45 {
                        continue;
                    }
                }
                last = Some(g.clone());
                if last_end.is_finite() {
                    let gap = g.x - last_end;
                    let size = g.size.max(last_size).max(1.0);
                    if gap > size * 1.6 {
                        // Column boundary within the row.
                        line.push_str(" | ");
                    } else if gap > size * 0.12 {
                        line.push(' ');
                    }
                }
                line.push_str(&g.text);
                last_end = g.end;
                last_size = g.size;
            }
            let line = line.trim().to_string();
            // Vertical labels rendered one letter per line and stray marks carry nothing.
            if line.chars().filter(|c| c.is_alphanumeric()).count() >= 2 || line.chars().any(|c| c.is_ascii_digit()) {
                lines.push(line);
            } else if let Some(l) = lines.last_mut() {
                // dropped row: don't let its gap create a stray blank line later
                if l.is_empty() { lines.pop(); }
            }
        }
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        let text = lines.join("\n");
        // collapse runs of blank lines
        let mut compact = String::with_capacity(text.len());
        let mut blank = 0;
        for l in text.lines() {
            if l.is_empty() { blank += 1; if blank > 1 { continue; } } else { blank = 0; }
            compact.push_str(l);
            compact.push('\n');
        }
        self.out.push_str(compact.trim_end());
    }
}

impl OutputDev for LayoutOutput {
    fn begin_page(&mut self, _page: u32, media_box: &MediaBox, _art: Option<(f64, f64, f64, f64)>) -> Result<(), OutputError> {
        self.flip = Transform::row_major(1., 0., 0., -1., 0., media_box.ury - media_box.lly);
        Ok(())
    }
    fn end_page(&mut self) -> Result<(), OutputError> {
        self.flush_page();
        Ok(())
    }
    fn output_character(&mut self, trm: &Transform, width: f64, _spacing: f64, font_size: f64, ch: &str) -> Result<(), OutputError> {
        let pos = trm.post_transform(&self.flip);
        // Rendered glyph size: the font-size vector through the text matrix.
        let vx = trm.m11 * font_size + trm.m21 * font_size;
        let vy = trm.m12 * font_size + trm.m22 * font_size;
        let size = (vx * vy).abs().sqrt().max(0.5);
        let (x, y) = (pos.m31, pos.m32);
        if ch.trim().is_empty() {
            return Ok(());
        }
        if let Some(d) = &self.debug {
            if d.contains(ch) {
                eprintln!("glyph {ch:?} x={x:.2} y={y:.2} w={width:.3} fs={font_size:.2} size={size:.2} m11={:.3} m22={:.3}", trm.m11, trm.m22);
            }
        }
        self.glyphs.push(Glyph { x, y, end: x + width * size, size, text: ch.to_string() });
        Ok(())
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn stroke(&mut self, _: &Transform, _: &ColorSpace, _: &[f64], _: &PdfPath) -> Result<(), OutputError> {
        Ok(())
    }
    fn fill(&mut self, _: &Transform, _: &ColorSpace, _: &[f64], _: &PdfPath) -> Result<(), OutputError> {
        Ok(())
    }
}

/// Extract text from PDF bytes with layout preserved; falls back to the plain
/// extractor if the document cannot be walked.
pub fn extract(bytes: &[u8]) -> Result<String, String> {
    let doc = pdf_extract::Document::load_mem(bytes).map_err(|e| e.to_string())?;
    let mut out = LayoutOutput::new();
    pdf_extract::output_doc(&doc, &mut out).map_err(|e| e.to_string())?;
    Ok(out.out)
}
