//! Layout-aware PDF text: glyphs are collected with their page positions and
//! re-assembled into rows (by baseline) and columns (by horizontal gaps), so a
//! form's labels and values come out on consecutive, aligned lines instead of
//! in content-stream order. `pdf-extract`'s plain writer turns a customs entry
//! into "S\nH\nI\nP\nP\nE\nR" and detached value clumps; this gives the model
//! `IMPORTING CARRIER | FROM PORT OF` over `TRANQUIL ACE 0124A | KOBE, JAPAN`.

use pdf_extract::{ColorSpace, MediaBox, OutputDev, OutputError, Path as PdfPath, PathOp, Transform};

/// Glyphs of one cell → "Label: value". The label is the cell's first row when a
/// smaller-font (or all-caps) row is followed by others; otherwise the rows joined.
fn cell_text(gs: Vec<Glyph>) -> String {
    let rows = rows_of(gs);
    if rows.is_empty() {
        return String::new();
    }
    // A big cell is a block of text (status logs, declarations): keep its lines.
    if rows.len() > 3 {
        return rows.iter().map(|r| r.2.as_str()).collect::<Vec<_>>().join("\n");
    }
    if rows.len() >= 2 {
        let first = &rows[0].2;
        let caps = first.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase()) && first.chars().any(|c| c.is_alphabetic());
        if caps || rows[0].3 < rows[1].3 - 0.5 {
            let value = rows[1..].iter().map(|r| r.2.as_str()).collect::<Vec<_>>().join(" ");
            return format!("{}: {}", first.trim_end_matches(':'), value);
        }
    }
    rows.iter().map(|r| r.2.as_str()).collect::<Vec<_>>().join(" ")
}

/// Rows (top y, left x, text, font size) from loose glyphs — the same grouping the
/// page-level layout uses, without column separators.
fn rows_of(mut glyphs: Vec<Glyph>) -> Vec<(f64, f64, String, f64)> {
    glyphs.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<Vec<Glyph>> = Vec::new();
    for g in glyphs {
        match rows.last_mut() {
            Some(row) if (g.y - row[0].y).abs() <= row[0].size.max(g.size) * 0.5 => row.push(g),
            _ => rows.push(vec![g]),
        }
    }
    let mut out = Vec::new();
    for mut row in rows {
        row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        let mut line = String::new();
        let mut last_end = f64::NEG_INFINITY;
        let mut last: Option<Glyph> = None;
        let size = row.iter().map(|g| g.size).fold(0.0, f64::max);
        let (top, left) = (row[0].y - size, row[0].x);
        for g in row {
            if let Some(l) = &last {
                if l.text == g.text && (g.x - l.x).abs() < (l.end - l.x).abs().max(0.05) * 0.45 {
                    continue;
                }
            }
            if last_end.is_finite() && g.x - last_end > size.max(1.0) * 0.12 {
                line.push(' ');
            }
            line.push_str(&g.text);
            last_end = g.end;
            last = Some(g.clone());
        }
        let line = line.trim().to_string();
        if line.chars().filter(|c| c.is_alphanumeric()).count() >= 1 {
            out.push((top, left, line, size));
        }
    }
    out
}

#[derive(Clone)]
struct Glyph {
    x: f64,
    y: f64,
    end: f64,
    size: f64,
    text: String,
}

/// A ruled line on the page (form cell borders), page coordinates.
#[derive(Clone, Copy)]
struct Seg {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

pub struct LayoutOutput {
    flip: Transform,
    glyphs: Vec<Glyph>,
    /// Horizontal / vertical rules drawn on the current page.
    segs: Vec<Seg>,
    pub out: String,
    /// Debug: print every glyph whose text occurs in this string.
    pub debug: Option<String>,
}

impl LayoutOutput {
    pub fn new() -> Self {
        LayoutOutput { flip: Transform::identity(), glyphs: Vec::new(), segs: Vec::new(), out: String::new(), debug: None }
    }

    /// Straight segments of a path, in page coordinates (ctm then the page flip).
    fn add_path(&mut self, ctm: &Transform, path: &PdfPath, filled: bool) {
        let m = ctm.post_transform(&self.flip);
        let tx = |x: f64, y: f64| (x * m.m11 + y * m.m21 + m.m31, x * m.m12 + y * m.m22 + m.m32);
        let push = |s: &mut Vec<Seg>, a: (f64, f64), b: (f64, f64)| {
            let (dx, dy) = ((a.0 - b.0).abs(), (a.1 - b.1).abs());
            // Only rules: axis-aligned and at least a few points long.
            if (dx < 1.5 && dy > 4.0) || (dy < 1.5 && dx > 4.0) {
                s.push(Seg { x0: a.0.min(b.0), y0: a.1.min(b.1), x1: a.0.max(b.0), y1: a.1.max(b.1) });
            }
        };
        let (mut start, mut cur) = ((0.0, 0.0), (0.0, 0.0));
        for op in &path.ops {
            match *op {
                PathOp::MoveTo(x, y) => {
                    cur = tx(x, y);
                    start = cur;
                }
                PathOp::LineTo(x, y) => {
                    let p = tx(x, y);
                    push(&mut self.segs, cur, p);
                    cur = p;
                }
                PathOp::CurveTo(_, _, _, _, x, y) => cur = tx(x, y),
                PathOp::Rect(x, y, w, h) => {
                    let (a, b, c, d) = (tx(x, y), tx(x + w, y), tx(x + w, y + h), tx(x, y + h));
                    if filled && ((b.0 - a.0).abs() < 2.5 || (d.1 - a.1).abs() < 2.5) {
                        // A thin filled rectangle is how many forms draw their rules.
                        push(&mut self.segs, a, if (b.0 - a.0).abs() < 2.5 { d } else { b });
                    } else if !filled {
                        push(&mut self.segs, a, b);
                        push(&mut self.segs, b, c);
                        push(&mut self.segs, c, d);
                        push(&mut self.segs, d, a);
                    }
                    cur = a;
                    start = a;
                }
                PathOp::Close => {
                    push(&mut self.segs, cur, start);
                    cur = start;
                }
            }
        }
    }

    /// Cell a glyph sits in: nearest rules left/above that span it, and right/below.
    /// None when the page's rules don't box it in.
    fn cell_of(&self, hs: &[Seg], vs: &[Seg], g: &Glyph) -> Option<(i64, i64, i64, i64)> {
        let (gx, gy) = (g.x + (g.end - g.x) / 2.0, g.y - g.size * 0.35); // mid-glyph
        let left = vs.iter().filter(|s| s.x0 <= gx + 0.5 && s.y0 <= gy && s.y1 >= gy).map(|s| s.x0).fold(f64::NEG_INFINITY, f64::max);
        let right = vs.iter().filter(|s| s.x0 > gx + 0.5 && s.y0 <= gy && s.y1 >= gy).map(|s| s.x0).fold(f64::INFINITY, f64::min);
        let top = hs.iter().filter(|s| s.y0 <= gy && s.x0 <= gx && s.x1 >= gx).map(|s| s.y0).fold(f64::NEG_INFINITY, f64::max);
        let bottom = hs.iter().filter(|s| s.y0 > gy && s.x0 <= gx && s.x1 >= gx).map(|s| s.y0).fold(f64::INFINITY, f64::min);
        if !left.is_finite() || !top.is_finite() || !right.is_finite() || !bottom.is_finite() {
            return None;
        }
        if right - left > 600.0 || bottom - top > 200.0 {
            return None; // page frame, not a cell
        }
        Some((left.round() as i64, top.round() as i64, right.round() as i64, bottom.round() as i64))
    }

    fn flush_page(&mut self) {
        let mut glyphs = std::mem::take(&mut self.glyphs);
        let segs = std::mem::take(&mut self.segs);
        if glyphs.is_empty() {
            return;
        }
        // Forms: enough ruled lines to make cells → "LABEL: value" per cell, in reading
        // order; text outside the cells falls through to the row logic below.
        let hs: Vec<Seg> = segs.iter().copied().filter(|s| s.y1 - s.y0 < 1.5).collect();
        let vs: Vec<Seg> = segs.iter().copied().filter(|s| s.x1 - s.x0 < 1.5).collect();
        if hs.len() >= 6 && vs.len() >= 6 {
            let mut cells: std::collections::BTreeMap<(i64, i64, i64, i64), Vec<Glyph>> = std::collections::BTreeMap::new();
            let mut loose = Vec::new();
            for g in glyphs {
                match self.cell_of(&hs, &vs, &g) {
                    Some(c) => cells.entry(c).or_default().push(g),
                    None => loose.push(g),
                }
            }
            if cells.len() >= 4 {
                // Each cell → one line: sorted by the cell's top, then left.
                let mut lines: Vec<(f64, f64, String)> = cells.into_iter().map(|((l, t, _, _), gs)| (t as f64, l as f64, cell_text(gs))).filter(|(_, _, s)| !s.is_empty()).collect();
                // Loose text keeps its row grouping and is merged in by position.
                for (y, x, s, _) in rows_of(loose).into_iter() {
                    lines.push((y, x, s));
                }
                lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.partial_cmp(&b.1).unwrap()));
                let mut text = String::new();
                let mut last_top = f64::NEG_INFINITY;
                for (top, _, s) in lines {
                    if top - last_top > 6.0 && !text.is_empty() {
                        text.push('\n');
                    } else if !text.is_empty() {
                        text.push_str(" | ");
                    }
                    text.push_str(&s);
                    last_top = top;
                }
                if !self.out.is_empty() {
                    self.out.push_str("\n\n");
                }
                self.out.push_str(text.trim_end());
                return;
            } else {
                // Not a form after all: put the glyphs back for the row logic.
                let mut all = loose;
                for (_, gs) in cells {
                    all.extend(gs);
                }
                glyphs = all;
            }
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
                if l.is_empty() {
                    lines.pop();
                }
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
            if l.is_empty() {
                blank += 1;
                if blank > 1 {
                    continue;
                }
            } else {
                blank = 0;
            }
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
    fn stroke(&mut self, ctm: &Transform, _: &ColorSpace, _: &[f64], path: &PdfPath) -> Result<(), OutputError> {
        self.add_path(ctm, path, false);
        Ok(())
    }
    fn fill(&mut self, ctm: &Transform, _: &ColorSpace, _: &[f64], path: &PdfPath) -> Result<(), OutputError> {
        self.add_path(ctm, path, true);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One glyph per char, laid out left to right; a space in `text` becomes a real gap.
    fn glyphs(text: &str, y: f64, size: f64, x0: f64) -> Vec<Glyph> {
        let adv = size * 0.6;
        let mut out = Vec::new();
        let mut x = x0;
        for c in text.chars() {
            if c == ' ' {
                x += adv;
                continue;
            }
            out.push(Glyph { x, y, end: x + adv * 0.9, size, text: c.to_string() });
            x += adv;
        }
        out
    }

    fn lines(rows: &[(f64, f64, String, f64)]) -> Vec<String> {
        rows.iter().map(|r| r.2.clone()).collect()
    }

    #[test]
    fn glyphs_on_one_baseline_become_one_row_with_word_spaces() {
        let rows = rows_of(glyphs("KOBE JAPAN", 100.0, 10.0, 20.0));
        assert_eq!(lines(&rows), vec!["KOBE JAPAN"]);
    }

    #[test]
    fn a_small_baseline_jitter_stays_on_the_same_row() {
        let mut gs = glyphs("ACME", 100.0, 10.0, 20.0);
        gs.extend(glyphs("CORP", 102.0, 10.0, 60.0));
        assert_eq!(lines(&rows_of(gs)), vec!["ACME CORP"]);
    }

    #[test]
    fn a_real_line_break_starts_a_new_row_in_reading_order() {
        let mut gs = glyphs("second", 130.0, 10.0, 20.0);
        gs.extend(glyphs("first", 100.0, 10.0, 20.0));
        assert_eq!(lines(&rows_of(gs)), vec!["first", "second"]);
    }

    #[test]
    fn glyphs_out_of_stream_order_are_sorted_by_x() {
        let mut gs = glyphs("WORLD", 100.0, 10.0, 80.0);
        gs.extend(glyphs("HELLO", 100.0, 10.0, 20.0));
        assert_eq!(lines(&rows_of(gs)), vec!["HELLO WORLD"]);
    }

    #[test]
    fn an_overprinted_glyph_is_not_doubled() {
        // Fake-bold PDFs draw the same glyph twice, a hair apart.
        let mut gs = glyphs("VIN", 100.0, 10.0, 20.0);
        let mut dupe = gs[0].clone();
        dupe.x += 0.2;
        gs.insert(1, dupe);
        assert_eq!(lines(&rows_of(gs)), vec!["VIN"]);
    }

    #[test]
    fn rows_without_a_letter_or_digit_are_dropped() {
        let rows = rows_of(glyphs("--", 100.0, 10.0, 20.0));
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn a_caps_row_above_a_value_becomes_label_colon_value() {
        let mut gs = glyphs("IMPORTING CARRIER", 100.0, 10.0, 20.0);
        gs.extend(glyphs("TRANQUIL ACE 0124A", 118.0, 10.0, 20.0));
        assert_eq!(cell_text(gs), "IMPORTING CARRIER: TRANQUIL ACE 0124A");
    }

    #[test]
    fn a_label_already_ending_in_a_colon_does_not_get_two() {
        let mut gs = glyphs("TOTAL:", 100.0, 10.0, 20.0);
        gs.extend(glyphs("1,240.00", 118.0, 10.0, 20.0));
        assert_eq!(cell_text(gs), "TOTAL: 1,240.00");
    }

    #[test]
    fn a_smaller_first_row_is_the_label_even_in_mixed_case() {
        let mut gs = glyphs("Entry number", 100.0, 6.0, 20.0);
        gs.extend(glyphs("Bolded value", 118.0, 10.0, 20.0));
        assert_eq!(cell_text(gs), "Entry number: Bolded value");
    }

    #[test]
    fn two_rows_of_the_same_size_and_case_are_just_joined() {
        let mut gs = glyphs("Tranquil Ace", 100.0, 10.0, 20.0);
        gs.extend(glyphs("Voyage 0124A", 118.0, 10.0, 20.0));
        assert_eq!(cell_text(gs), "Tranquil Ace Voyage 0124A");
    }

    #[test]
    fn a_value_wrapped_over_two_rows_joins_onto_the_label() {
        let mut gs = glyphs("SHIPPER", 100.0, 10.0, 20.0);
        gs.extend(glyphs("ACME CORP", 118.0, 10.0, 20.0));
        gs.extend(glyphs("OSAKA", 136.0, 10.0, 20.0));
        assert_eq!(cell_text(gs), "SHIPPER: ACME CORP OSAKA");
    }

    #[test]
    fn a_block_of_text_keeps_its_lines() {
        let mut gs = Vec::new();
        for (i, line) in ["Line one", "Line two", "Line three", "Line four"].iter().enumerate() {
            gs.extend(glyphs(line, 100.0 + 18.0 * i as f64, 10.0, 20.0));
        }
        assert_eq!(cell_text(gs), "Line one\nLine two\nLine three\nLine four");
    }

    #[test]
    fn a_single_row_cell_is_its_own_text() {
        assert_eq!(cell_text(glyphs("KOBE, JAPAN", 100.0, 10.0, 20.0)), "KOBE, JAPAN");
    }

    #[test]
    fn an_empty_cell_is_empty_text() {
        assert_eq!(cell_text(Vec::new()), "");
        assert_eq!(cell_text(glyphs("- -", 100.0, 10.0, 20.0)), "");
    }
}
