//! OCR on the same engine as everything else: PaddleOCR's PP-OCRv4 text detector
//! (DBNet, 4.7 MB) and PP-OCRv3 English recognizer (SVTR-LCNet, 9 MB), run through
//! ONNX Runtime on DirectML with the CPU as fallback. Both graphs and the character
//! dictionary are compiled into the exe.
//!
//! Pipeline: resize (longest side ≤ 1280, multiples of 32) → detector probability map
//! → connected components → boxes expanded like DB's unclip → each box cropped, scaled
//! to 48 px high and padded to a width bucket → recognizer → CTC decode → lines
//! assembled into rows/columns the way `pdf_layout` does it. The Python reference of
//! this pipeline (eval/ocr/proto.py) scores 98 % word accuracy on the synthetic set.

use crate::embed::ok;
use ort::ep::{DirectML, ExecutionProvider};
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const DET_BYTES: &[u8] = include_bytes!("../models/ocr/det.onnx");
const REC_BYTES: &[u8] = include_bytes!("../models/ocr/rec_en.onnx");
const DICT: &str = include_str!("../models/ocr/en_dict.txt");

/// Longest image side fed to the detector.
const DET_MAX: usize = 1280;
const DET_THRESH: f32 = 0.3;
const BOX_MIN_SCORE: f32 = 0.5;
const UNCLIP: f32 = 1.6;
/// Recognizer input height and the width buckets (so DirectML compiles a few shapes, not one per line).
const REC_H: usize = 48;
const REC_BUCKETS: [usize; 7] = [64, 128, 192, 256, 384, 512, 768];
/// Pages of a scanned PDF to read at most.
const MAX_PAGES: usize = 30;

pub struct Ocr {
    det: Session,
    rec: Session,
    dict: Vec<String>,
    pub backend: &'static str,
}

static OCR: OnceLock<Mutex<Option<Ocr>>> = OnceLock::new();

/// A decoded RGB image.
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// One recognised line: box in image pixels and its text.
struct Line {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    text: String,
}

fn session(bytes: &'static [u8], device: Option<i32>) -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let b = match device {
        Some(d) => ok(b.with_execution_providers([DirectML::default().with_device_id(d).build().error_on_failure()]))?,
        None => ok(b.with_intra_threads(4))?,
    };
    let mut b = ok(b.with_memory_pattern(false))?;
    ok(b.commit_from_memory(bytes))
}

impl Ocr {
    fn load() -> anyhow::Result<Ocr> {
        let dict: Vec<String> = DICT.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
        let force_cpu = std::env::var_os("BLACKHOLE_CPU").is_some();
        let gpu = crate::gpu::preferred().filter(|_| !force_cpu && DirectML::default().is_available().unwrap_or(false));
        if let Some(a) = gpu {
            if let (Ok(det), Ok(rec)) = (session(DET_BYTES, Some(a.index)), session(REC_BYTES, Some(a.index))) {
                return Ok(Ocr { det, rec, dict, backend: "DirectML" });
            }
        }
        Ok(Ocr { det: session(DET_BYTES, None)?, rec: session(REC_BYTES, None)?, dict, backend: "CPU" })
    }

    /// Text boxes in image coordinates: (x0, y0, x1, y1, score).
    fn detect(&mut self, img: &Rgb) -> anyhow::Result<Vec<(f32, f32, f32, f32, f32)>> {
        let scale = (DET_MAX as f32 / img.w.max(img.h) as f32).min(1.0);
        let nw = (((img.w as f32 * scale / 32.0).round() as usize) * 32).max(32);
        let nh = (((img.h as f32 * scale / 32.0).round() as usize) * 32).max(32);
        let small = resize(img, nw, nh);
        let (mean, std) = ([0.485f32, 0.456, 0.406], [0.229f32, 0.224, 0.225]);
        let mut x = vec![0f32; 3 * nw * nh];
        for i in 0..nw * nh {
            for c in 0..3 {
                x[c * nw * nh + i] = (small.px[i * 3 + c] as f32 / 255.0 - mean[c]) / std[c];
            }
        }
        let outputs = ok(self.det.run(ort::inputs!["x" => ok(Tensor::from_array(([1usize, 3, nh, nw], x)))?]))?;
        let (_, prob) = ok(outputs[0].try_extract_tensor::<f32>())?;
        // Connected components on the thresholded map (4-neighbour flood fill).
        let mut label = vec![0u32; nw * nh];
        let mut boxes = Vec::new();
        let mut next = 1u32;
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..nw * nh {
            if prob[start] <= DET_THRESH || label[start] != 0 {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (nw, nh, 0usize, 0usize);
            let (mut sum, mut n) = (0f32, 0usize);
            label[start] = next;
            stack.push(start);
            while let Some(i) = stack.pop() {
                let (px, py) = (i % nw, i / nw);
                x0 = x0.min(px);
                x1 = x1.max(px);
                y0 = y0.min(py);
                y1 = y1.max(py);
                sum += prob[i];
                n += 1;
                for j in [i.checked_sub(1).filter(|_| px > 0), (px + 1 < nw).then_some(i + 1), i.checked_sub(nw), (py + 1 < nh).then_some(i + nw)].into_iter().flatten() {
                    if prob[j] > DET_THRESH && label[j] == 0 {
                        label[j] = next;
                        stack.push(j);
                    }
                }
            }
            next += 1;
            let score = sum / n as f32;
            if n < 10 || score < BOX_MIN_SCORE {
                continue;
            }
            // DB's unclip on the box: expand every side by area * ratio / perimeter.
            let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
            let d = bw * bh * UNCLIP / (2.0 * (bw + bh));
            let (sx, sy) = (img.w as f32 / nw as f32, img.h as f32 / nh as f32);
            boxes.push((
                ((x0 as f32 - d).max(0.0)) * sx,
                ((y0 as f32 - d).max(0.0)) * sy,
                ((x1 as f32 + 1.0 + d).min(nw as f32)) * sx,
                ((y1 as f32 + 1.0 + d).min(nh as f32)) * sy,
                score,
            ));
        }
        Ok(boxes)
    }

    /// Recognise one box; returns (text, mean confidence).
    fn recognize(&mut self, img: &Rgb, b: (f32, f32, f32, f32)) -> anyhow::Result<(String, f32)> {
        let (x0, y0, x1, y1) = (b.0.floor() as usize, b.1.floor() as usize, (b.2.ceil() as usize).min(img.w), (b.3.ceil() as usize).min(img.h));
        if x1 <= x0 + 1 || y1 <= y0 + 1 {
            return Ok((String::new(), 0.0));
        }
        let crop = crop(img, x0, y0, x1, y1);
        let cw = ((crop.w as f32 * REC_H as f32 / crop.h as f32).round() as usize).max(16);
        let bucket = REC_BUCKETS.iter().copied().find(|&b| b >= cw).unwrap_or(*REC_BUCKETS.last().unwrap());
        let scaled = resize(&crop, cw.min(bucket), REC_H);
        // Normalised to [-1, 1]; the padding to the right of the text stays 0.
        let mut x = vec![0f32; 3 * REC_H * bucket];
        for yy in 0..REC_H {
            for xx in 0..scaled.w {
                for c in 0..3 {
                    x[c * REC_H * bucket + yy * bucket + xx] = (scaled.px[(yy * scaled.w + xx) * 3 + c] as f32 / 255.0 - 0.5) / 0.5;
                }
            }
        }
        let outputs = ok(self.rec.run(ort::inputs!["x" => ok(Tensor::from_array(([1usize, 3, REC_H, bucket], x)))?]))?;
        let (shape, out) = ok(outputs[0].try_extract_tensor::<f32>())?;
        let (steps, classes) = (shape[1] as usize, shape[2] as usize);
        let (mut text, mut prev, mut conf, mut n) = (String::new(), 0usize, 0f32, 0usize);
        for t in 0..steps {
            let row = &out[t * classes..(t + 1) * classes];
            let (idx, c) = row.iter().enumerate().fold((0usize, f32::MIN), |b, (i, &v)| if v > b.1 { (i, v) } else { b });
            if idx != 0 && idx != prev {
                // Index 0 is CTC blank; the dictionary follows; the last class is the space.
                match self.dict.get(idx - 1) {
                    Some(ch) => text.push_str(ch),
                    None => text.push(' '),
                }
                conf += c;
                n += 1;
            }
            prev = idx;
        }
        Ok((text, if n > 0 { conf / n as f32 } else { 0.0 }))
    }

    /// Full pipeline on one image → text laid out in rows and columns.
    pub fn read(&mut self, img: &Rgb) -> anyhow::Result<String> {
        let boxes = self.detect(img)?;
        let mut lines: Vec<Line> = Vec::new();
        for (x0, y0, x1, y1, _) in boxes {
            let (text, conf) = self.recognize(img, (x0, y0, x1, y1))?;
            if !text.trim().is_empty() && conf > 0.3 {
                lines.push(Line { x0, y0, x1, y1, text });
            }
        }
        Ok(layout(lines))
    }
}

/// Rows by centre y (within half a line height), sorted by x; a horizontal gap wider
/// than 1.5 line heights becomes a column separator, like `pdf_layout`.
fn layout(mut lines: Vec<Line>) -> String {
    lines.sort_by(|a, b| ((a.y0 + a.y1) / 2.0).partial_cmp(&((b.y0 + b.y1) / 2.0)).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<(f32, Vec<Line>)> = Vec::new();
    for l in lines {
        let cy = (l.y0 + l.y1) / 2.0;
        let h = l.y1 - l.y0;
        match rows.last_mut() {
            Some(r) if (cy - r.0).abs() < 0.5 * h => {
                r.0 = (r.0 + cy) / 2.0;
                r.1.push(l);
            }
            _ => rows.push((cy, vec![l])),
        }
    }
    let mut out = String::new();
    let mut prev_bottom: Option<f32> = None;
    for (_, mut items) in rows {
        items.sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal));
        let top = items.iter().map(|l| l.y0).fold(f32::MAX, f32::min);
        let h = items.iter().map(|l| l.y1 - l.y0).fold(0.0, f32::max);
        if let Some(pb) = prev_bottom {
            if top - pb > h * 0.9 {
                out.push('\n'); // paragraph gap
            }
        }
        let mut last_right: Option<f32> = None;
        for l in &items {
            if let Some(r) = last_right {
                out.push_str(if l.x0 - r > 1.5 * (l.y1 - l.y0) { " | " } else { " " });
            }
            out.push_str(l.text.trim());
            last_right = Some(l.x1);
        }
        out.push('\n');
        prev_bottom = Some(items.iter().map(|l| l.y1).fold(0.0, f32::max));
    }
    out.trim_end().to_string()
}

fn resize(img: &Rgb, nw: usize, nh: usize) -> Rgb {
    // Bilinear, good enough for OCR inputs.
    let mut px = vec![0u8; nw * nh * 3];
    let (sx, sy) = (img.w as f32 / nw as f32, img.h as f32 / nh as f32);
    for y in 0..nh {
        let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy as usize).min(img.h - 1);
        let y1 = (y0 + 1).min(img.h - 1);
        let wy = fy - y0 as f32;
        for x in 0..nw {
            let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx as usize).min(img.w - 1);
            let x1 = (x0 + 1).min(img.w - 1);
            let wx = fx - x0 as f32;
            for c in 0..3 {
                let p = |xx: usize, yy: usize| img.px[(yy * img.w + xx) * 3 + c] as f32;
                let v = p(x0, y0) * (1.0 - wx) * (1.0 - wy) + p(x1, y0) * wx * (1.0 - wy) + p(x0, y1) * (1.0 - wx) * wy + p(x1, y1) * wx * wy;
                px[(y * nw + x) * 3 + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Rgb { w: nw, h: nh, px }
}

fn crop(img: &Rgb, x0: usize, y0: usize, x1: usize, y1: usize) -> Rgb {
    let (w, h) = (x1 - x0, y1 - y0);
    let mut px = Vec::with_capacity(w * h * 3);
    for y in y0..y1 {
        px.extend_from_slice(&img.px[(y * img.w + x0) * 3..(y * img.w + x1) * 3]);
    }
    Rgb { w, h, px }
}

// ---------------------------------------------------------------------------
// Decoding

pub fn decode_png(bytes: &[u8]) -> Option<Rgb> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let data = &buf[..info.buffer_size()];
    let px = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgb, png::BitDepth::Eight) => data.to_vec(),
        (png::ColorType::Rgba, png::BitDepth::Eight) => data.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => data.iter().flat_map(|&g| [g, g, g]).collect(),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => data.chunks(2).flat_map(|p| [p[0], p[0], p[0]]).collect(),
        _ => return None, // 16-bit / palette: rare for screenshots and scans
    };
    Some(Rgb { w, h, px })
}

pub fn decode_jpeg(bytes: &[u8]) -> Option<Rgb> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), opts);
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if px.len() != w * h * 3 {
        return None;
    }
    Some(Rgb { w, h, px })
}

pub fn decode_file(path: &Path) -> Option<Rgb> {
    let bytes = std::fs::read(path).ok()?;
    match path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("png") => decode_png(&bytes),
        Some("jpg") | Some("jpeg") => decode_jpeg(&bytes),
        _ => decode_png(&bytes).or_else(|| decode_jpeg(&bytes)),
    }
}

// ---------------------------------------------------------------------------
// Entry points

fn with_ocr<T>(f: impl FnOnce(&mut Ocr) -> T) -> Option<T> {
    let cell = OCR.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if guard.is_none() {
        match Ocr::load() {
            Ok(o) => {
                crate::util::log(&format!("ocr: PP-OCRv4 det + v3 en rec on {}", o.backend));
                *guard = Some(o);
            }
            Err(e) => {
                crate::util::log(&format!("ocr: could not load: {e}"));
                return None;
            }
        }
    }
    Some(f(guard.as_mut().unwrap()))
}

/// Release the sessions (they reload on the next image).
pub fn unload() {
    if let Some(cell) = OCR.get() {
        *cell.lock().unwrap() = None;
    }
}

/// OCR text for a decoded image, or None when the models can't load.
pub fn read_image(img: &Rgb) -> Option<String> {
    let t = Instant::now();
    let text = with_ocr(|o| o.read(img))?.ok()?;
    crate::util::log(&format!("ocr: {}×{} → {} words in {:.0} ms", img.w, img.h, text.split_whitespace().count(), t.elapsed().as_secs_f32() * 1000.0));
    Some(text)
}

pub fn read_file(path: &Path) -> Option<String> {
    let img = decode_file(path)?;
    read_image(&img)
}

/// Scanned PDFs: no glyphs to extract, but each page carries its scan as an image.
/// Reads JPEG (DCTDecode) and raw 8-bit RGB/grey (FlateDecode) images, page by page.
pub fn read_pdf_images(bytes: &[u8]) -> Option<String> {
    let doc = pdf_extract::Document::load_mem(bytes).ok()?;
    let mut pages_text = Vec::new();
    for (_, page_id) in doc.get_pages().into_iter().take(MAX_PAGES) {
        let Ok(images) = doc.get_page_images(page_id) else { continue };
        for im in images {
            if im.width < 200 || im.height < 100 {
                continue; // logos, rules, icons
            }
            let filters = im.filters.clone().unwrap_or_default();
            let rgb = if filters.iter().any(|f| f == "DCTDecode") {
                decode_jpeg(im.content)
            } else if filters.is_empty() || filters.iter().all(|f| f == "FlateDecode") {
                let raw = if filters.is_empty() {
                    im.content.to_vec()
                } else {
                    match doc.get_object(im.id).and_then(|o| o.as_stream()).and_then(|s| s.decompressed_content()) {
                        Ok(d) => d,
                        Err(_) => continue,
                    }
                };
                let (w, h) = (im.width as usize, im.height as usize);
                match (im.color_space.as_deref(), im.bits_per_component) {
                    (Some("DeviceRGB"), Some(8)) if raw.len() >= w * h * 3 => Some(Rgb { w, h, px: raw[..w * h * 3].to_vec() }),
                    (Some("DeviceGray"), Some(8)) if raw.len() >= w * h => Some(Rgb { w, h, px: raw[..w * h].iter().flat_map(|&g| [g, g, g]).collect() }),
                    (Some("DeviceGray"), Some(1)) if raw.len() * 8 >= w * h => {
                        let stride = w.div_ceil(8);
                        let mut px = Vec::with_capacity(w * h * 3);
                        for y in 0..h {
                            for x in 0..w {
                                let bit = raw.get(y * stride + x / 8).map(|b| (b >> (7 - x % 8)) & 1).unwrap_or(1);
                                let g = if bit == 1 { 255 } else { 0 };
                                px.extend_from_slice(&[g, g, g]);
                            }
                        }
                        Some(Rgb { w, h, px })
                    }
                    _ => None,
                }
            } else {
                None // CCITT / JBIG2 / JPX: not decoded (yet)
            };
            if let Some(img) = rgb {
                if let Some(t) = read_image(&img) {
                    if !t.trim().is_empty() {
                        pages_text.push(t);
                    }
                }
            }
        }
    }
    (!pages_text.is_empty()).then(|| pages_text.join("\n\n"))
}
