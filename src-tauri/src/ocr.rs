//! Local offline Chinese OCR engine for kindergarten artwork name tags & signatures.
//! Option A2: `PP-OCRv4_det` (DBNet text detector) + `PP-OCRv5_rec` (18,383-char recognizer
//! with dictionary embedded in ONNX `character` metadata) via `ort`.
use anyhow::{anyhow, bail, Context, Result};
use image::{imageops::FilterType, RgbImage};
use ort::{environment::Environment, session::Session, value::Tensor};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::OnceLock;

pub const OCR_DET_MODEL: &str = "ocr_det.onnx";
pub const OCR_REC_MODEL: &str = "ocr_rec.onnx";
pub const OCR_DET_URL: &str =
    "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv4/ch_PP-OCRv4_det_infer.onnx";
pub const OCR_REC_URL: &str =
    "https://huggingface.co/breezedeus/cnocr-ppocr-ch_PP-OCRv5/resolve/main/ch_PP-OCRv5_rec_infer.onnx";

const DET_MAX_SIDE: u32 = 960;
const DET_STRIDE: u32 = 32;
const DB_BIN_THRESH: f32 = 0.30;
const DB_BOX_THRESH: f32 = 0.50;
const DB_UNCLIP_RATIO: f32 = 1.6;
const DB_MIN_SIDE: f32 = 3.0;
const DB_MIN_AREA: usize = 6;

const REC_H: usize = 48;
const REC_MIN_W: usize = 16;
const REC_MAX_W: usize = 3200;
pub const MIN_REC_SCORE: f32 = 0.55;
const VERTICAL_ASPECT_RATIO: f32 = 1.5;

const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Generic placeholders / non-name labels that must never auto-derive a 2-char alias.
const GENERIC_NAME_TERMS: &[&str] = &[
    "宝宝", "小朋友", "同学", "宝贝", "孩子", "儿子", "女儿", "大宝", "二宝", "小宝", "大班",
    "中班", "小班", "托班", "学前班", "幼儿园", "未命名", "测试",
];

static ORT_ENV: OnceLock<Environment> = OnceLock::new();

/// Shared process-global ONNX Runtime environment (`ort` allows only one `Environment::build()` per process).
pub fn ort_env() -> Result<&'static Environment> {
    if let Some(env) = ORT_ENV.get() {
        return Ok(env);
    }
    let env = ort::init()
        .with_name("kid-finder")
        .build()
        .map_err(|e| anyhow!("初始化 ONNX Runtime 失败: {e}"))?;
    let _ = ORT_ENV.set(env);
    ORT_ENV.get().ok_or_else(|| anyhow!("获取 ONNX Runtime 环境失败"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrBox {
    /// Normalized `[x1, y1, x2, y2]` in `0.0..=1.0` of the input `RgbImage`.
    pub bbox: [f32; 4],
    pub text: String,
    pub norm_text: String,
    pub score: f32,
}

pub struct OcrEngine {
    det: Session,
    rec: Session,
    /// Index 0 = `<blank>`, `1..=N` = ONNX metadata `"character"` lines, `N + 1` = `" "`.
    dict: Vec<String>,
}

impl OcrEngine {
    pub fn load(dir: &Path) -> Result<Self> {
        Self::load_with_env(dir, ort_env()?)
    }

    pub fn load_with_env(dir: &Path, env: &Environment) -> Result<Self> {
        Self::load_files(&dir.join(OCR_DET_MODEL), &dir.join(OCR_REC_MODEL), env)
    }

    pub fn load_files(det_path: &Path, rec_path: &Path, env: &Environment) -> Result<Self> {
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 4))
            .unwrap_or(2);
        let mk = |p: &Path| -> Result<Session> {
            Session::builder(env)
                .map_err(|e| anyhow!("{e}"))?
                .with_intra_threads(threads)
                .map_err(|e| anyhow!("{e}"))?
                .commit_from_file(p)
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("加载 OCR 模型 {} 失败", p.display()))
        };
        let det = mk(det_path)?;
        let rec = mk(rec_path)?;
        let dict = extract_embedded_dict(&rec)?;
        Ok(Self { det, rec, dict })
    }

    /// Number of character entries (excluding CTC blank) loaded from ONNX metadata.
    pub fn vocab_size(&self) -> usize {
        self.dict.len().saturating_sub(1)
    }

    pub fn analyze(&mut self, img: &RgbImage) -> Result<Vec<OcrBox>> {
        self.analyze_with_min_score(img, MIN_REC_SCORE)
    }

    pub fn analyze_with_min_score(&mut self, img: &RgbImage, min_score: f32) -> Result<Vec<OcrBox>> {
        let (orig_w, orig_h) = img.dimensions();
        if orig_w == 0 || orig_h == 0 {
            return Ok(vec![]);
        }
        let regions = self.detect_boxes(img)?;
        let mut out = Vec::with_capacity(regions.len());
        for (bbox, _det_score) in regions {
            let x0 = (bbox[0] * orig_w as f32).floor().clamp(0.0, (orig_w - 1) as f32) as u32;
            let y0 = (bbox[1] * orig_h as f32).floor().clamp(0.0, (orig_h - 1) as f32) as u32;
            let x1 = (bbox[2] * orig_w as f32).ceil().clamp((x0 + 1) as f32, orig_w as f32) as u32;
            let y1 = (bbox[3] * orig_h as f32).ceil().clamp((y0 + 1) as f32, orig_h as f32) as u32;
            let cw = (x1 - x0).max(1);
            let ch = (y1 - y0).max(1);
            let crop = image::imageops::crop_imm(img, x0, y0, cw, ch).to_image();
            // Rotate 90° CCW (`rotate270` in `image` crate moves top y=0 to left x=0) for vertical Chinese signatures.
            let oriented = if ch as f32 >= cw as f32 * VERTICAL_ASPECT_RATIO {
                image::imageops::rotate270(&crop)
            } else {
                crop
            };
            if let Some((text, score)) = self.recognize_crop(&oriented)? {
                if score < min_score {
                    continue;
                }
                let text = text.trim().to_string();
                let norm_text = normalize_ocr_text(&text);
                if text.is_empty() || norm_text.is_empty() {
                    continue;
                }
                out.push(OcrBox { bbox, text, norm_text, score });
            }
        }
        Ok(out)
    }

    fn detect_boxes(&mut self, img: &RgbImage) -> Result<Vec<([f32; 4], f32)>> {
        let (orig_w, orig_h) = img.dimensions();
        let max_side = orig_w.max(orig_h);
        let scale = if max_side > DET_MAX_SIDE {
            DET_MAX_SIDE as f32 / max_side as f32
        } else {
            1.0
        };
        let nw = ((orig_w as f32 * scale).round() as u32).clamp(1, DET_MAX_SIDE);
        let nh = ((orig_h as f32 * scale).round() as u32).clamp(1, DET_MAX_SIDE);
        let resized = if nw == orig_w && nh == orig_h {
            None
        } else {
            Some(image::imageops::resize(img, nw, nh, FilterType::Triangle))
        };
        let src = resized.as_ref().unwrap_or(img);

        let w_pad = (nw.div_ceil(DET_STRIDE) * DET_STRIDE).max(DET_STRIDE) as usize;
        let h_pad = (nh.div_ceil(DET_STRIDE) * DET_STRIDE).max(DET_STRIDE) as usize;
        let plane = w_pad * h_pad;
        let mut data = vec![0f32; 3 * plane];
        for y in 0..h_pad {
            let sy = (y as u32).min(nh - 1);
            for x in 0..w_pad {
                let sx = (x as u32).min(nw - 1);
                let p = src.get_pixel(sx, sy);
                let idx = y * w_pad + x;
                for c in 0..3 {
                    data[c * plane + idx] =
                        (p[c] as f32 / 255.0 - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
                }
            }
        }

        let input = Tensor::from_array(([1usize, 3, h_pad, w_pad], data))?;
        let outputs = self.det.run(ort::inputs![input])?;
        let (shape, prob) = outputs[0].try_extract_tensor::<f32>()?;
        if shape.len() != 4 || shape[2] as usize != h_pad || shape[3] as usize != w_pad {
            bail!("OCR 检测模型输出维度异常: {shape:?}");
        }
        Ok(extract_db_boxes(prob, w_pad, nw as usize, nh as usize))
    }

    fn recognize_crop(&mut self, crop: &RgbImage) -> Result<Option<(String, f32)>> {
        let (cw, ch) = crop.dimensions();
        if cw == 0 || ch == 0 {
            return Ok(None);
        }
        let w_dyn = ((cw as f32 * REC_H as f32 / ch as f32).round() as usize).clamp(REC_MIN_W, REC_MAX_W);
        let resized = image::imageops::resize(crop, w_dyn as u32, REC_H as u32, FilterType::Triangle);
        let plane = REC_H * w_dyn;
        let mut data = vec![0f32; 3 * plane];
        for (x, y, p) in resized.enumerate_pixels() {
            let idx = y as usize * w_dyn + x as usize;
            for c in 0..3 {
                data[c * plane + idx] = (p[c] as f32 / 255.0 - 0.5) / 0.5;
            }
        }
        let input = Tensor::from_array(([1usize, 3, REC_H, w_dyn], data))?;
        let outputs = self.rec.run(ort::inputs![input])?;
        let (shape, d) = outputs[0].try_extract_tensor::<f32>()?;
        if shape.len() != 3 || shape[1] <= 0 || shape[2] <= 1 {
            bail!("OCR 识别模型输出维度异常: {shape:?}");
        }
        let (steps, classes) = (shape[1] as usize, shape[2] as usize);
        Ok(ctc_greedy_decode(d, steps, classes, &self.dict))
    }
}

fn extract_embedded_dict(rec: &Session) -> Result<Vec<String>> {
    let meta = rec.metadata().map_err(|e| anyhow!("读取 OCR 模型元数据失败: {e}"))?;
    let raw = meta
        .custom("character")
        .context("OCR 识别模型缺少内嵌词表 metadata[\"character\"]")?;
    let chars: Vec<&str> = raw.lines().collect();
    if chars.is_empty() {
        bail!("OCR 识别模型内嵌词表为空");
    }
    let mut dict = Vec::with_capacity(chars.len() + 2);
    dict.push(String::new()); // 0 = CTC <blank>
    for ch in chars {
        dict.push(ch.to_string());
    }
    dict.push(" ".to_string()); // trailing space class (`use_space_char=True` -> index N+1)
    Ok(dict)
}

/// Pure-Rust DBNet probability map connected-components + unclip bounding-box extractor.
/// Returns normalized `([x1, y1, x2, y2], mean_prob)` sorted in top-to-bottom, left-to-right order.
pub fn extract_db_boxes(
    prob: &[f32],
    map_w: usize,
    valid_w: usize,
    valid_h: usize,
) -> Vec<([f32; 4], f32)> {
    if valid_w == 0 || valid_h == 0 || map_w < valid_w || prob.len() < map_w * valid_h {
        return vec![];
    }
    let mut visited = vec![false; valid_w * valid_h];
    let mut queue = VecDeque::with_capacity(256);
    let mut out = Vec::new();

    for sy in 0..valid_h {
        for sx in 0..valid_w {
            let start_vi = sy * valid_w + sx;
            if visited[start_vi] {
                continue;
            }
            let p0 = prob[sy * map_w + sx];
            if p0 <= DB_BIN_THRESH {
                visited[start_vi] = true;
                continue;
            }

            visited[start_vi] = true;
            queue.clear();
            queue.push_back((sx, sy));

            let (mut min_x, mut max_x) = (sx, sx);
            let (mut min_y, mut max_y) = (sy, sy);
            let mut sum_prob = 0.0f32;
            let mut count = 0usize;

            while let Some((cx, cy)) = queue.pop_front() {
                let pv = prob[cy * map_w + cx];
                sum_prob += pv;
                count += 1;
                if cx < min_x {
                    min_x = cx;
                }
                if cx > max_x {
                    max_x = cx;
                }
                if cy < min_y {
                    min_y = cy;
                }
                if cy > max_y {
                    max_y = cy;
                }

                let y_lo = cy.saturating_sub(1);
                let y_hi = (cy + 1).min(valid_h - 1);
                let x_lo = cx.saturating_sub(1);
                let x_hi = (cx + 1).min(valid_w - 1);
                for ny in y_lo..=y_hi {
                    for nx in x_lo..=x_hi {
                        let nvi = ny * valid_w + nx;
                        if !visited[nvi] {
                            visited[nvi] = true;
                            if prob[ny * map_w + nx] > DB_BIN_THRESH {
                                queue.push_back((nx, ny));
                            }
                        }
                    }
                }
            }

            let comp_w = (max_x - min_x + 1) as f32;
            let comp_h = (max_y - min_y + 1) as f32;
            if count < DB_MIN_AREA || comp_w < DB_MIN_SIDE || comp_h < DB_MIN_SIDE {
                continue;
            }
            let mean_prob = sum_prob / count as f32;
            if mean_prob < DB_BOX_THRESH {
                continue;
            }

            let db_dist = (comp_w * comp_h * DB_UNCLIP_RATIO) / (2.0 * (comp_w + comp_h));
            let pad = db_dist.max(0.45 * comp_w.min(comp_h)).max(2.0);

            let x1 = ((min_x as f32 - pad) / valid_w as f32).clamp(0.0, 1.0);
            let y1 = ((min_y as f32 - pad) / valid_h as f32).clamp(0.0, 1.0);
            let x2 = (((max_x + 1) as f32 + pad) / valid_w as f32).clamp(0.0, 1.0);
            let y2 = (((max_y + 1) as f32 + pad) / valid_h as f32).clamp(0.0, 1.0);
            if x2 > x1 && y2 > y1 {
                out.push(([x1, y1, x2, y2], mean_prob));
            }
        }
    }

    out.sort_by(|(a, _), (b, _)| {
        let ay = a[1];
        let by = b[1];
        let row_tol = ((a[3] - a[1]).min(b[3] - b[1])) * 0.5;
        if (ay - by).abs() <= row_tol {
            a[0].total_cmp(&b[0])
        } else {
            ay.total_cmp(&by)
        }
    });
    out
}

/// CTC greedy decoder: collapses repeated indices and removes blank (`0`), averaging kept step probabilities.
pub fn ctc_greedy_decode(
    data: &[f32],
    steps: usize,
    classes: usize,
    dict: &[String],
) -> Option<(String, f32)> {
    if steps == 0 || classes <= 1 || data.len() < steps * classes {
        return None;
    }
    let mut text = String::new();
    let mut prob_sum = 0.0f32;
    let mut kept = 0usize;
    let mut prev_idx = 0usize;

    for t in 0..steps {
        let row = &data[t * classes..(t + 1) * classes];
        let mut best_idx = 0usize;
        let mut best_val = f32::NEG_INFINITY;
        let mut min_val = f32::INFINITY;
        for (i, &v) in row.iter().enumerate() {
            if v > best_val {
                best_val = v;
                best_idx = i;
            }
            if v < min_val {
                min_val = v;
            }
        }
        if best_idx != 0 && best_idx != prev_idx {
            let prob = if best_val > 1.001 || min_val < -0.001 {
                let sum_exp: f32 = row.iter().map(|&v| (v - best_val).exp()).sum();
                (1.0 / sum_exp.max(1e-12)).clamp(0.0, 1.0)
            } else {
                best_val.clamp(0.0, 1.0)
            };
            if let Some(tok) = dict.get(best_idx) {
                if !tok.is_empty() {
                    text.push_str(tok);
                    prob_sum += prob;
                    kept += 1;
                }
            }
        }
        prev_idx = best_idx;
    }

    if kept == 0 {
        None
    } else {
        Some((text, prob_sum / kept as f32))
    }
}

/// Strips whitespace, punctuation, symbols, and control characters, folds fullwidth ASCII,
/// and normalizes high-frequency calligraphy/OCR radical confusions (e.g. `袆` U+8866 -> `祎` U+794E).
pub fn normalize_ocr_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for raw in s.chars() {
        let c = fold_char(raw);
        if c.is_alphanumeric() {
            out.push(c);
        }
    }
    out
}

fn fold_char(c: char) -> char {
    let c = if ('\u{FF01}'..='\u{FF5E}').contains(&c) {
        char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
    } else {
        c
    };
    let c = c.to_ascii_lowercase();
    match c {
        // High-frequency `衤` (U+8866) vs `礻` (U+794E) confusion in handwritten/brush fonts for `祎`
        '袆' => '祎',
        _ => c,
    }
}

fn is_cjk_ideograph(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{20000}'..='\u{323AF}'
            | '\u{F900}'..='\u{FAFF}'
    )
}

/// For 3- or 4-character CJK full names (not matching generic labels like `宝宝`/`小朋友`/`同学` or class names),
/// derives the 2-character given name (e.g. `韩梓淏` -> `"梓淏"`, `欧阳沐宸` -> `"沐宸"`), otherwise `""`.
pub fn default_aliases_for_name(name: &str) -> String {
    let trimmed = name.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if (chars.len() != 3 && chars.len() != 4) || !chars.iter().copied().all(is_cjk_ideograph) {
        return String::new();
    }
    if trimmed.ends_with('班') || GENERIC_NAME_TERMS.iter().any(|g| trimmed.contains(g)) {
        return String::new();
    }
    let given: String = chars[chars.len() - 2..].iter().collect();
    normalize_ocr_text(&given)
}

/// Builds deduplicated normalized keywords from `name` and comma/delimiter-separated `aliases`.
/// Strictly rejects single-character keywords (`chars().count() >= 2` required) to prevent surname false positives.
pub fn build_person_keywords(name: &str, aliases: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_kw = |raw: &str| {
        let norm = normalize_ocr_text(raw);
        if norm.chars().count() >= 2 && !out.contains(&norm) {
            out.push(norm);
        }
    };

    push_kw(name);
    for part in aliases.split([',', '，', '、', ';', '；', '/', '|', '\n', '\r']) {
        push_kw(part);
    }
    out
}

/// Returns the longest matching keyword contained in `norm_text` (strictly ignoring any `< 2`-char entry).
pub fn match_keywords<'a>(norm_text: &str, keywords: &'a [String]) -> Option<&'a str> {
    if norm_text.is_empty() || keywords.is_empty() {
        return None;
    }
    let mut best: Option<(&'a str, usize)> = None;
    for kw in keywords {
        let len = kw.chars().count();
        if len < 2 || !norm_text.contains(kw.as_str()) {
            continue;
        }
        if best.map(|(_, bl)| len > bl).unwrap_or(true) {
            best = Some((kw.as_str(), len));
        }
    }
    best.map(|(s, _)| s)
}

/// Finds the best matching `OcrBox` for `keywords` (preferring longest matched keyword, then highest OCR score).
pub fn match_ocr_boxes<'a>(
    boxes: &'a [OcrBox],
    keywords: &'a [String],
) -> Option<(&'a OcrBox, &'a str)> {
    let mut best: Option<(&'a OcrBox, &'a str, usize)> = None;
    for b in boxes {
        let Some(kw) = match_keywords(&b.norm_text, keywords) else {
            continue;
        };
        let kw_len = kw.chars().count();
        let replace = match best {
            None => true,
            Some((prev_b, _, prev_len)) => {
                kw_len > prev_len || (kw_len == prev_len && b.score > prev_b.score)
            }
        };
        if replace {
            best = Some((b, kw, kw_len));
        }
    }
    best.map(|(b, kw, _)| (b, kw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use std::path::PathBuf;

    #[test]
    fn test_normalize_ocr_text() {
        // Verify U+6DCF (`淏`) and U+6D5B (`浛`) are preserved while punctuation/whitespace/controls are stripped.
        assert_eq!('淏' as u32, 0x6DCF);
        assert_eq!(
            normalize_ocr_text(" 姓名：韩梓淏（大二班）\n\t·#1 "),
            "姓名韩梓淏大二班1"
        );
        // Glyph variant `袆` (U+8866) -> `祎` (U+794E)
        assert_eq!(normalize_ocr_text("陈袆铭 · 王玥萱 · 李浛熠"), "陈祎铭王玥萱李浛熠");
        // Fullwidth ASCII folding + case insensitivity
        assert_eq!(normalize_ocr_text("Ｌｅｏ ２０２６！"), "leo2026");
        // Pure punctuation / whitespace normalizes to empty
        assert_eq!(normalize_ocr_text("  --- 、，。！？【】()  "), "");
    }

    #[test]
    fn test_default_aliases_for_name() {
        // 3-char & 4-char CJK full names (including `淏` U+6DCF and `祎` U+794E)
        assert_eq!(default_aliases_for_name("韩梓淏"), "梓淏");
        assert_eq!(default_aliases_for_name("  韩梓淏  "), "梓淏");
        assert_eq!(default_aliases_for_name("陈袆铭"), "祎铭");
        assert_eq!(default_aliases_for_name("欧阳沐宸"), "沐宸");

        // 1-char, 2-char, 5-char names -> empty
        assert_eq!(default_aliases_for_name("淏"), "");
        assert_eq!(default_aliases_for_name("梓淏"), "");
        assert_eq!(default_aliases_for_name("王伟"), "");
        assert_eq!(default_aliases_for_name("爱新觉罗玄烨"), "");

        // Generic placeholders & class labels -> empty
        assert_eq!(default_aliases_for_name("宝宝"), "");
        assert_eq!(default_aliases_for_name("我家宝宝"), "");
        assert_eq!(default_aliases_for_name("张小朋友"), "");
        assert_eq!(default_aliases_for_name("李同学"), "");
        assert_eq!(default_aliases_for_name("大二班"), "");
        assert_eq!(default_aliases_for_name("中一班"), "");
        assert_eq!(default_aliases_for_name("Leo1"), "");
    }

    #[test]
    fn test_build_person_keywords_rejects_single_chars_and_dedupes() {
        let kws = build_person_keywords("韩梓淏", "韩, 梓淏，淏淏、韩梓淏; 小淏 / A / 王");
        assert_eq!(kws, vec!["韩梓淏", "梓淏", "淏淏", "小淏"]);

        // Single-character person name must also be rejected
        assert!(build_person_keywords("韩", "李, 王, 1, a").is_empty());

        // Variant normalization deduplicates `陈袆铭` and `陈祎铭`
        assert_eq!(build_person_keywords("陈祎铭", "陈袆铭, 祎铭"), vec!["陈祎铭", "祎铭"]);
    }

    #[test]
    fn test_match_keywords_longest_and_safe() {
        let kws = build_person_keywords("韩梓淏", "梓淏, 淏淏");
        // Both "韩梓淏" (3) and "梓淏" (2) match -> returns longest ("韩梓淏")
        assert_eq!(match_keywords("大二班韩梓淏作品", &kws), Some("韩梓淏"));
        // Given-name-only sticker -> matches "梓淏"
        assert_eq!(match_keywords("姓名梓淏", &kws), Some("梓淏"));
        // Nickname match
        assert_eq!(match_keywords("作者淏淏", &kws), Some("淏淏"));
        // Lookalike wrong character `溴` must NOT match `淏`
        assert_eq!(match_keywords("大二班韩梓溴", &kws), None);

        // Defense-in-depth: manually passed 1-char keyword in slice must still be rejected
        let manual = vec!["韩".to_string(), "".to_string()];
        assert_eq!(match_keywords("韩沐宸", &manual), None);
    }

    #[test]
    fn test_extract_db_boxes_synthetic_probability_map() {
        let (map_w, valid_w, valid_h) = (128usize, 120usize, 96usize);
        let mut prob = vec![0.02f32; map_w * valid_h];

        // 1) Valid horizontal text region: x in 20..70, y in 12..22 (50x10, prob = 0.88)
        for y in 12..22 {
            for x in 20..70 {
                prob[y * map_w + x] = 0.88;
            }
        }
        // 2) Valid vertical signature region: x in 92..100, y in 30..82 (8x52, prob = 0.91)
        for y in 30..82 {
            for x in 92..100 {
                prob[y * map_w + x] = 0.91;
            }
        }
        // 3) Low-confidence blob above binary threshold 0.30 but below box threshold 0.50 -> must be filtered
        for y in 60..72 {
            for x in 15..40 {
                prob[y * map_w + x] = 0.41;
            }
        }
        // 4) High-confidence tiny 2x2 speckle (< DB_MIN_SIDE / DB_MIN_AREA) -> must be filtered
        for y in 5..7 {
            for x in 5..7 {
                prob[y * map_w + x] = 0.95;
            }
        }

        let boxes = extract_db_boxes(&prob, map_w, valid_w, valid_h);
        assert_eq!(boxes.len(), 2, "expected exactly 2 valid text boxes, got {boxes:?}");

        // Box 0: horizontal text region (topmost) with unclip expansion beyond core [20/120, 12/96, 70/120, 22/96]
        let (hb, h_score) = boxes[0];
        assert!((h_score - 0.88).abs() < 1e-4);
        assert!(hb[0] < 20.0 / valid_w as f32 && hb[0] >= 0.0);
        assert!(hb[1] < 12.0 / valid_h as f32 && hb[1] >= 0.0);
        assert!(hb[2] > 70.0 / valid_w as f32 && hb[2] <= 1.0);
        assert!(hb[3] > 22.0 / valid_h as f32 && hb[3] <= 1.0);

        // Box 1: vertical signature region, aspect ratio >= 1.5 in pixel space
        let (vb, v_score) = boxes[1];
        assert!((v_score - 0.91).abs() < 1e-4);
        let vw_px = (vb[2] - vb[0]) * valid_w as f32;
        let vh_px = (vb[3] - vb[1]) * valid_h as f32;
        assert!(
            vh_px >= vw_px * VERTICAL_ASPECT_RATIO,
            "vertical box should trigger CCW rotation: vw={vw_px}, vh={vh_px}"
        );
    }

    #[test]
    fn test_ctc_greedy_decode_repeats_and_blank() {
        // 0 = blank, 1 = "韩", 2 = "梓", 3 = "淏", 4 = " "
        let dict = vec![
            "".to_string(),
            "韩".to_string(),
            "梓".to_string(),
            "淏".to_string(),
            " ".to_string(),
        ];
        // Sequence: [blank, 淏(0.9), 淏(0.8), blank, 淏(0.95)] -> "淏淏" (mean of 0.90 and 0.95 = 0.925)
        let steps = [
            [0.95, 0.01, 0.01, 0.02, 0.01],
            [0.02, 0.02, 0.04, 0.90, 0.02],
            [0.05, 0.03, 0.10, 0.80, 0.02], // consecutive duplicate collapsed
            [0.92, 0.02, 0.02, 0.02, 0.02], // blank resets repeat state
            [0.01, 0.01, 0.02, 0.95, 0.01], // second `淏` emitted
        ];
        let flat: Vec<f32> = steps.iter().flatten().copied().collect();
        let (text, score) = ctc_greedy_decode(&flat, 5, 5, &dict).expect("should decode");
        assert_eq!(text, "淏淏");
        assert!((score - 0.925).abs() < 1e-4);
    }

    fn locate_cached_ocr_models() -> Option<(PathBuf, PathBuf)> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(custom) = std::env::var("KID_FINDER_OCR_MODEL_DIR") {
            dirs.push(PathBuf::from(custom));
        }
        dirs.push(std::env::temp_dir());
        if let Ok(appdata) = std::env::var("APPDATA") {
            dirs.push(PathBuf::from(&appdata).join("com.kidfinder.app").join("models"));
            dirs.push(PathBuf::from(appdata).join("kid-finder").join("models"));
        }

        let candidates = [
            (OCR_DET_MODEL, OCR_REC_MODEL),
            ("ch_PP-OCRv4_det_infer.onnx", "ch_PP-OCRv5_rec_infer.onnx"),
        ];
        for d in dirs {
            for (det_name, rec_name) in candidates {
                let det = d.join(det_name);
                let rec = d.join(rec_name);
                let ok_det = std::fs::metadata(&det).map(|m| m.len() > 1_000_000).unwrap_or(false);
                let ok_rec = std::fs::metadata(&rec).map(|m| m.len() > 1_000_000).unwrap_or(false);
                if ok_det && ok_rec {
                    return Some((det, rec));
                }
            }
        }
        None
    }

    /// Renders crisp block digits/letters ("AB12") on a white canvas so the smoke test has zero external font dependency.
    fn render_synthetic_banner() -> RgbImage {
        // 5x7 bitmap glyphs for 'A', 'B', '1', '2' scaled 6x (30x42 px per glyph)
        const GLYPHS: [&[&str; 7]; 4] = [
            &["01110", "10001", "10001", "11111", "10001", "10001", "10001"], // A
            &["11110", "10001", "10001", "11110", "10001", "10001", "11110"], // B
            &["00100", "01100", "00100", "00100", "00100", "00100", "01110"], // 1
            &["01110", "10001", "00001", "00110", "01000", "10000", "11111"], // 2
        ];
        let mut img = RgbImage::from_pixel(320, 120, Rgb([255, 255, 255]));
        let scale = 6u32;
        for (gi, g) in GLYPHS.iter().enumerate() {
            let ox = 40 + gi as u32 * 44;
            let oy = 38;
            for (ry, row) in g.iter().enumerate() {
                for (rx, ch) in row.chars().enumerate() {
                    if ch == '1' {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                img.put_pixel(
                                    ox + rx as u32 * scale + dx,
                                    oy + ry as u32 * scale + dy,
                                    Rgb([20, 20, 20]),
                                );
                            }
                        }
                    }
                }
            }
        }
        img
    }

    #[test]
    fn test_ocr_engine_smoke_if_models_present() -> Result<()> {
        let Some((det_path, rec_path)) = locate_cached_ocr_models() else {
            eprintln!("skipping OcrEngine ONNX smoke test: OCR models not present locally");
            return Ok(());
        };
        let mut engine = OcrEngine::load_files(&det_path, &rec_path, ort_env()?)?;
        assert!(engine.vocab_size() >= 18_000);
        assert!(engine.dict.iter().any(|s| s == "淏"), "PP-OCRv5 dict must contain U+6DCF `淏`");

        let img = render_synthetic_banner();
        let tmp_img = std::env::temp_dir().join(format!("kf_ocr_smoke_{}.png", uuid::Uuid::new_v4()));
        img.save(&tmp_img)?;
        let loaded = image::open(&tmp_img)?.to_rgb8();
        let _ = std::fs::remove_file(&tmp_img);

        let boxes = engine.analyze(&loaded)?;
        assert!(!boxes.is_empty(), "expected OcrEngine to detect synthetic banner text");
        Ok(())
    }
}
