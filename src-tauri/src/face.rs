//! Face detection (SCRFD det_10g) + recognition (ArcFace w600k_r50) via ONNX Runtime.
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageReader, Rgb, RgbImage};
use ort::{session::Session, value::Tensor};
use std::path::Path;

pub const DET_MODEL: &str = "det_10g.onnx";
pub const REC_MODEL: &str = "w600k_r50.onnx";

const DET_SIZE: usize = 960;
const DET_THRESH: f32 = 0.5;
const NMS_THRESH: f32 = 0.4;
/// Faces smaller than this (in px of the analysed image) give unreliable embeddings.
const MIN_FACE: f32 = 20.0;
/// Images larger than this are downscaled before analysis.
const MAX_SIDE: u32 = 2560;
const ARC_DST: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

#[derive(Clone, Debug)]
pub struct Face {
    /// x1, y1, x2, y2 in pixels of the analysed image
    pub bbox: [f32; 4],
    pub score: f32,
    pub embedding: Vec<f32>,
}

pub struct FaceEngine {
    det: Session,
    rec: Session,
}

type Det = ([f32; 4], f32, [[f32; 2]; 5]);

impl FaceEngine {
    pub fn load(dir: &Path) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let env = ort::init().with_name("kid-finder").build().map_err(|e| anyhow::anyhow!("初始化 ONNX Runtime 失败: {e}"))?;
        let mk = |f: &str| -> Result<Session> {
            let s = Session::builder(&env)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .with_intra_threads(threads)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .commit_from_file(dir.join(f))
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("加载模型 {f} 失败"))?;
            Ok(s)
        };
        Ok(Self { det: mk(DET_MODEL)?, rec: mk(REC_MODEL)? })
    }

    pub fn analyze(&mut self, img: &RgbImage) -> Result<Vec<Face>> {
        let dets = self.detect(img)?;
        let mut out = Vec::with_capacity(dets.len());
        for (bbox, score, kps) in dets {
            let aligned = align(img, &kps);
            let embedding = self.embed(&aligned)?;
            out.push(Face { bbox, score, embedding });
        }
        Ok(out)
    }

    fn detect(&mut self, img: &RgbImage) -> Result<Vec<Det>> {
        let (w, h) = img.dimensions();
        let s = DET_SIZE as f32;
        let scale = (s / w as f32).min(s / h as f32);
        let nw = ((w as f32 * scale).round() as u32).clamp(1, DET_SIZE as u32);
        let nh = ((h as f32 * scale).round() as u32).clamp(1, DET_SIZE as u32);
        let resized = image::imageops::resize(img, nw, nh, FilterType::Triangle);
        let plane = DET_SIZE * DET_SIZE;
        let mut data = vec![-127.5f32 / 128.0; 3 * plane];
        for (x, y, p) in resized.enumerate_pixels() {
            let i = y as usize * DET_SIZE + x as usize;
            for c in 0..3 {
                data[c * plane + i] = (p[c] as f32 - 127.5) / 128.0;
            }
        }
        let input = Tensor::from_array(([1usize, 3, DET_SIZE, DET_SIZE], data))?;
        let outputs = self.det.run(ort::inputs![input])?;

        let (mut scores, mut boxes, mut kpss) = (vec![], vec![], vec![]);
        for (_name, v) in outputs.iter() {
            let (shape, d) = v.try_extract_tensor::<f32>()?;
            match shape.last().copied().unwrap_or(0) {
                1 => scores.push(d.to_vec()),
                4 => boxes.push(d.to_vec()),
                10 => kpss.push(d.to_vec()),
                _ => {}
            }
        }
        if scores.len() != 3 || boxes.len() != 3 || kpss.len() != 3 {
            bail!("检测模型输出格式不符合预期");
        }
        // stride 8 has the most anchors
        for v in [&mut scores, &mut boxes, &mut kpss] {
            v.sort_by_key(|x| std::cmp::Reverse(x.len()));
        }

        let mut cands: Vec<Det> = vec![];
        for (level, stride) in [8usize, 16, 32].into_iter().enumerate() {
            let fw = DET_SIZE / stride;
            let (sc, bb, kp) = (&scores[level], &boxes[level], &kpss[level]);
            let na = (sc.len() / (fw * fw)).max(1);
            let st = stride as f32;
            for i in 0..sc.len() {
                if sc[i] < DET_THRESH {
                    continue;
                }
                let cell = i / na;
                let cx = ((cell % fw) * stride) as f32;
                let cy = ((cell / fw) * stride) as f32;
                let b = [
                    (cx - bb[i * 4] * st) / scale,
                    (cy - bb[i * 4 + 1] * st) / scale,
                    (cx + bb[i * 4 + 2] * st) / scale,
                    (cy + bb[i * 4 + 3] * st) / scale,
                ];
                let mut k5 = [[0f32; 2]; 5];
                for (j, k) in k5.iter_mut().enumerate() {
                    *k = [
                        (cx + kp[i * 10 + j * 2] * st) / scale,
                        (cy + kp[i * 10 + j * 2 + 1] * st) / scale,
                    ];
                }
                cands.push((b, sc[i], k5));
            }
        }
        cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut keep: Vec<Det> = vec![];
        for c in cands {
            if keep.iter().all(|k| iou(&k.0, &c.0) < NMS_THRESH) {
                keep.push(c);
            }
        }
        keep.retain(|(b, _, _)| (b[2] - b[0]).min(b[3] - b[1]) >= MIN_FACE);
        for (b, _, _) in keep.iter_mut() {
            b[0] = b[0].max(0.0);
            b[1] = b[1].max(0.0);
            b[2] = b[2].min(w as f32);
            b[3] = b[3].min(h as f32);
        }
        Ok(keep)
    }

    fn embed(&mut self, face: &RgbImage) -> Result<Vec<f32>> {
        let plane = 112 * 112;
        let mut data = vec![0f32; 3 * plane];
        for (x, y, p) in face.enumerate_pixels() {
            let i = y as usize * 112 + x as usize;
            for c in 0..3 {
                data[c * plane + i] = (p[c] as f32 - 127.5) / 127.5;
            }
        }
        let input = Tensor::from_array(([1usize, 3, 112, 112], data))?;
        let outputs = self.rec.run(ort::inputs![input])?;
        let (_, d) = outputs[0].try_extract_tensor::<f32>()?;
        let mut v = d.to_vec();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter_mut().for_each(|x| *x /= n);
        Ok(v)
    }
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    let ua = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if ua <= 0.0 { 0.0 } else { inter / ua }
}

/// Similarity-transform the face so the 5 landmarks land on the ArcFace template.
fn align(img: &RgbImage, kps: &[[f32; 2]; 5]) -> RgbImage {
    let (mut sx, mut sy, mut dx, mut dy) = (0f32, 0f32, 0f32, 0f32);
    for i in 0..5 {
        sx += kps[i][0];
        sy += kps[i][1];
        dx += ARC_DST[i][0];
        dy += ARC_DST[i][1];
    }
    sx /= 5.0;
    sy /= 5.0;
    dx /= 5.0;
    dy /= 5.0;
    let (mut na, mut nb, mut den) = (0f32, 0f32, 0f32);
    for i in 0..5 {
        let (px, py) = (kps[i][0] - sx, kps[i][1] - sy);
        let (qx, qy) = (ARC_DST[i][0] - dx, ARC_DST[i][1] - dy);
        na += px * qx + py * qy;
        nb += px * qy - py * qx;
        den += px * px + py * py;
    }
    let den = den.max(1e-6);
    let (a, b) = (na / den, nb / den);
    let tx = dx - (a * sx - b * sy);
    let ty = dy - (b * sx + a * sy);
    let det = (a * a + b * b).max(1e-12);
    let mut out = RgbImage::new(112, 112);
    for v in 0..112u32 {
        for u in 0..112u32 {
            let (qx, qy) = (u as f32 - tx, v as f32 - ty);
            let px = (a * qx + b * qy) / det;
            let py = (-b * qx + a * qy) / det;
            out.put_pixel(u, v, bilinear(img, px, py));
        }
    }
    out
}

fn bilinear(img: &RgbImage, x: f32, y: f32) -> Rgb<u8> {
    let (w, h) = img.dimensions();
    if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
        return Rgb([0, 0, 0]);
    }
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let (p00, p10, p01, p11) = (img.get_pixel(x0, y0), img.get_pixel(x1, y0), img.get_pixel(x0, y1), img.get_pixel(x1, y1));
    let mut o = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        o[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    Rgb(o)
}

/// Load an image with EXIF orientation applied, downscaled to at most MAX_SIDE.
pub fn load_image(path: &Path) -> Result<RgbImage> {
    let dec = ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    decode_with_orientation(dec)
}

/// Load an image from in-memory bytes with EXIF orientation applied, downscaled to MAX_SIDE.
pub fn load_image_bytes(bytes: &[u8]) -> Result<RgbImage> {
    let dec = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.into_decoder()?;
    decode_with_orientation(dec)
}

fn decode_with_orientation(mut dec: impl ImageDecoder) -> Result<RgbImage> {
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(orientation);
    if img.width().max(img.height()) > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle);
    }
    Ok(img.to_rgb8())
}

/// Square crop around a face (with margin), for thumbnails.
pub fn face_thumb(img: &RgbImage, bbox: &[f32; 4], size: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    let cx = (bbox[0] + bbox[2]) / 2.0;
    let cy = (bbox[1] + bbox[3]) / 2.0;
    let side = ((bbox[2] - bbox[0]).max(bbox[3] - bbox[1]) * 1.6).max(8.0);
    let x0 = (cx - side / 2.0).clamp(0.0, w as f32 - 1.0) as u32;
    let y0 = (cy - side / 2.0).clamp(0.0, h as f32 - 1.0) as u32;
    let cw = (side as u32).min(w - x0).max(1);
    let ch = (side as u32).min(h - y0).max(1);
    let crop = image::imageops::crop_imm(img, x0, y0, cw, ch).to_image();
    image::imageops::resize(&crop, size, size, FilterType::Triangle)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
