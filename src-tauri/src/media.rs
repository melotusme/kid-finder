//! Import (with duplicate detection), background analysis, search and export.
use crate::{db, face, download, AppState};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager};

pub const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];
pub const VIDEO_EXT: &[&str] = &["mp4", "mov", "m4v", "avi", "mkv", "3gp", "webm"];

pub fn kind_of(p: &Path) -> Option<&'static str> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some("image")
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some("video")
    } else {
        None
    }
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Duplicate {
    pub name: String,
    pub existing_name: String,
    pub imported_at: String,
}

#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub added: usize,
    pub duplicates: Vec<Duplicate>,
    pub unsupported: usize,
    pub errors: Vec<String>,
}

/// Returns (orig_name, imported_at) if a file with this hash is already in the library.
pub fn find_by_sha(c: &Connection, sha: &str) -> Result<Option<(String, String)>> {
    Ok(c.query_row(
        "SELECT orig_name, imported_at FROM media WHERE sha256=?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?)
}

pub struct NewMedia<'a> {
    pub src: &'a Path,
    pub sha: &'a str,
    pub kind: &'a str,
    pub orig_name: &'a str,
    pub source: &'a str,
    pub remote_id: Option<&'a str>,
    pub remote_time: Option<&'a str>,
    pub move_file: bool,
}

pub fn store_file(st: &AppState, m: NewMedia) -> Result<i64> {
    let ext = m
        .src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| if m.kind == "video" { "mp4".into() } else { "jpg".into() });
    let category = if m.kind == "video" { "videos" } else { "photos" };
    let dir = st.paths.library.join(category).join(&m.sha[..2]);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{}.{ext}", m.sha));
    if m.move_file {
        if std::fs::rename(m.src, &dest).is_err() {
            std::fs::copy(m.src, &dest)?;
            let _ = std::fs::remove_file(m.src);
        }
    } else {
        std::fs::copy(m.src, &dest)?;
    }
    let c = st.db.lock().unwrap();
    c.execute(
        "INSERT INTO media(sha256,kind,path,orig_name,source,remote_id,remote_time,imported_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![m.sha, m.kind, dest.to_string_lossy(), m.orig_name, m.source, m.remote_id, m.remote_time, db::now()],
    )?;
    Ok(c.last_insert_rowid())
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    stage: &'a str,
    done: usize,
    total: usize,
    current: String,
}

pub fn import_paths(app: &AppHandle, paths: Vec<String>) -> Result<ImportReport> {
    let st = app.state::<AppState>();
    let mut files: Vec<PathBuf> = vec![];
    for p in paths {
        let p = PathBuf::from(p);
        if p.is_dir() {
            for e in walkdir::WalkDir::new(&p).into_iter().flatten() {
                if e.file_type().is_file() {
                    files.push(e.into_path());
                }
            }
        } else if p.is_file() {
            files.push(p);
        }
    }
    let mut rep = ImportReport::default();
    let total = files.len();
    for (i, f) in files.iter().enumerate() {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let _ = app.emit("progress", Progress { stage: "import", done: i, total, current: name.clone() });
        let Some(kind) = kind_of(f) else {
            rep.unsupported += 1;
            continue;
        };
        let r: Result<()> = (|| {
            let sha = sha256_file(f)?;
            let existing = find_by_sha(&st.db.lock().unwrap(), &sha)?;
            if let Some((existing_name, imported_at)) = existing {
                rep.duplicates.push(Duplicate { name: name.clone(), existing_name, imported_at });
                return Ok(());
            }
            store_file(&st, NewMedia { src: f, sha: &sha, kind, orig_name: &name, source: "local", remote_id: None, remote_time: None, move_file: false })?;
            rep.added += 1;
            Ok(())
        })();
        if let Err(e) = r {
            rep.errors.push(format!("{name}: {e:#}"));
        }
    }
    let _ = app.emit("progress", Progress { stage: "import", done: total, total, current: String::new() });
    Ok(rep)
}

// ---------------- background analysis ----------------

pub fn ensure_engine(st: &AppState) -> Result<()> {
    let mut g = st.engine.lock().unwrap();
    if g.is_none() {
        if !download::models_ready(&st.paths) {
            bail!("人脸模型尚未下载，请先在「设置」页下载模型");
        }
        *g = Some(face::FaceEngine::load(&st.paths.models)?);
    }
    Ok(())
}

pub fn analyze_file(st: &AppState, path: &Path) -> Result<(image::RgbImage, Vec<face::Face>)> {
    let img = face::load_image(path)?;
    let faces = st.engine.lock().unwrap().as_mut().ok_or_else(|| anyhow!("模型未加载"))?.analyze(&img)?;
    Ok((img, faces))
}

/// (time, normalized bbox, det score, embedding)
type FaceRow = (Option<f64>, [f32; 4], f32, Vec<f32>);

fn norm_rows(img: &image::RgbImage, faces: Vec<face::Face>, t: Option<f64>) -> Vec<FaceRow> {
    let (w, h) = (img.width() as f32, img.height() as f32);
    faces
        .into_iter()
        .map(|f| (t, [f.bbox[0] / w, f.bbox[1] / h, f.bbox[2] / w, f.bbox[3] / h], f.score, f.embedding))
        .collect()
}

pub fn spawn_processing(app: AppHandle) {
    let st = app.state::<AppState>();
    if st.processing.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn_blocking(move || {
        let r = process_all(&app);
        let st = app.state::<AppState>();
        st.processing.store(false, Ordering::SeqCst);
        let _ = app.emit("process-done", r.err().map(|e| format!("{e:#}")));
    });
}

fn process_all(app: &AppHandle) -> Result<()> {
    let st = app.state::<AppState>();
    ensure_engine(&st)?;
    let ffmpeg = download::find_ffmpeg(&st);
    let mut done = 0usize;
    loop {
        let (next, remaining): (Option<(i64, String, String, String)>, i64) = {
            let c = st.db.lock().unwrap();
            let n = c
                .query_row("SELECT id,kind,path,orig_name FROM media WHERE status=0 ORDER BY id LIMIT 1", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let rem: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE status=0", [], |r| r.get(0))?;
            (n, rem)
        };
        let Some((id, kind, path, name)) = next else { break };
        let _ = app.emit("progress", Progress { stage: "analyze", done, total: done + remaining as usize, current: name });
        let res: Result<(Vec<FaceRow>, Option<String>, Option<f64>)> = if kind == "image" {
            analyze_file(&st, Path::new(&path)).map(|(img, faces)| (norm_rows(&img, faces, None), None, None))
        } else {
            analyze_video(&st, ffmpeg.as_deref(), id, Path::new(&path))
        };
        let mut c = st.db.lock().unwrap();
        match res {
            Ok((rows, poster, duration)) => {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM faces WHERE media_id=?1", [id])?;
                for (t, b, s, e) in rows {
                    tx.execute(
                        "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![id, t, b[0], b[1], b[2], b[3], s, db::emb_to_blob(&e)],
                    )?;
                }
                tx.execute(
                    "UPDATE media SET status=1, error=NULL, poster=COALESCE(?2,poster), duration=COALESCE(?3,duration) WHERE id=?1",
                    params![id, poster, duration],
                )?;
                tx.commit()?;
            }
            Err(e) => {
                c.execute("UPDATE media SET status=2, error=?2 WHERE id=?1", params![id, format!("{e:#}")])?;
            }
        }
        done += 1;
    }
    let _ = app.emit("progress", Progress { stage: "analyze", done, total: done, current: String::new() });
    Ok(())
}

pub fn ffmpeg_cmd(ff: &Path) -> Command {
    let mut c = Command::new(ff);
    c.arg("-hide_banner").arg("-loglevel").arg("error");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn run(mut c: Command) -> Result<()> {
    let out = c.output().context("启动 ffmpeg 失败")?;
    if !out.status.success() {
        bail!("ffmpeg 出错: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn analyze_video(st: &AppState, ffmpeg: Option<&Path>, id: i64, path: &Path) -> Result<(Vec<FaceRow>, Option<String>, Option<f64>)> {
    let ff = ffmpeg.ok_or_else(|| anyhow!("未找到 ffmpeg，无法处理视频（请在「设置」页安装）"))?;
    let dir = st.paths.tmp.join(format!("v{id}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let mut c = ffmpeg_cmd(ff);
    c.arg("-i").arg(path).arg("-vf").arg("fps=1,scale='min(1280,iw)':-2").arg("-q:v").arg("3").arg(dir.join("%06d.jpg"));
    run(c)?;
    let mut frames: Vec<PathBuf> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).collect();
    frames.sort();
    if frames.is_empty() {
        bail!("视频没有可读取的画面");
    }
    let mut rows = vec![];
    for (n, f) in frames.iter().enumerate() {
        let (img, faces) = analyze_file(st, f)?;
        rows.extend(norm_rows(&img, faces, Some(n as f64)));
    }
    let poster_src = &frames[frames.len().min(2) - 1];
    let poster = st.paths.thumbs.join(format!("poster_{id}.jpg"));
    face::load_image(poster_src)?
        .pipe(|img| image::DynamicImage::ImageRgb8(img).thumbnail(480, 480))
        .save(&poster)?;
    let duration = frames.len() as f64;
    let _ = std::fs::remove_dir_all(&dir);
    Ok((rows, Some(poster.to_string_lossy().to_string()), Some(duration)))
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

// ---------------- search ----------------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub media_id: i64,
    pub kind: String,
    pub path: String,
    pub orig_name: String,
    pub poster: Option<String>,
    pub time: String,
    pub score: f32,
    pub face_id: i64,
    pub bbox: [f32; 4],
    pub duration: Option<f64>,
    pub segments: Vec<[f64; 2]>,
}

pub fn person_refs(c: &Connection, person_id: i64) -> Result<Vec<Vec<f32>>> {
    let mut s = c.prepare("SELECT embedding FROM refs WHERE person_id=?1")?;
    let v = s.query_map([person_id], |r| r.get::<_, Vec<u8>>(0))?.flatten().map(|b| db::blob_to_emb(&b)).collect();
    Ok(v)
}

pub fn search(c: &Connection, person_id: i64, threshold: f32) -> Result<Vec<Hit>> {
    let refs = person_refs(c, person_id)?;
    if refs.is_empty() {
        bail!("请先给宝宝录入至少一张清晰的正脸照片");
    }
    let rejected: HashSet<i64> = c
        .prepare("SELECT face_id FROM rejections WHERE person_id=?1")?
        .query_map([person_id], |r| r.get(0))?
        .flatten()
        .collect();
    struct Acc {
        score: f32,
        face_id: i64,
        bbox: [f32; 4],
        times: Vec<f64>,
    }
    let mut acc: HashMap<i64, Acc> = HashMap::new();
    let mut s = c.prepare("SELECT id, media_id, t, x1, y1, x2, y2, embedding FROM faces")?;
    let mut rows = s.query([])?;
    while let Some(r) = rows.next()? {
        let fid: i64 = r.get(0)?;
        if rejected.contains(&fid) {
            continue;
        }
        let emb = db::blob_to_emb(&r.get::<_, Vec<u8>>(7)?);
        let sim = refs.iter().map(|q| face::cosine(q, &emb)).fold(f32::MIN, f32::max);
        if sim < threshold {
            continue;
        }
        let mid: i64 = r.get(1)?;
        let t: Option<f64> = r.get(2)?;
        let bbox = [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?];
        let e = acc.entry(mid).or_insert(Acc { score: f32::MIN, face_id: fid, bbox, times: vec![] });
        if sim > e.score {
            e.score = sim;
            e.face_id = fid;
            e.bbox = bbox;
        }
        if let Some(t) = t {
            e.times.push(t);
        }
    }
    let mut hits = vec![];
    let mut ms = c.prepare("SELECT kind, path, orig_name, poster, COALESCE(remote_time, imported_at), duration FROM media WHERE id=?1")?;
    for (mid, a) in acc {
        let (kind, path, orig_name, poster, time, duration): (String, String, String, Option<String>, String, Option<f64>) =
            ms.query_row([mid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
        let segments = if kind == "video" { merge_segments(a.times, duration) } else { vec![] };
        hits.push(Hit { media_id: mid, kind, path, orig_name, poster, time, score: a.score, face_id: a.face_id, bbox: a.bbox, duration, segments });
    }
    hits.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(hits)
}

/// Turn the sampled timestamps (1 fps) where the kid appears into merged clips.
/// No lead-in/tail padding here: the UI applies the user's 提前/延后 settings.
pub fn merge_segments(mut times: Vec<f64>, duration: Option<f64>) -> Vec<[f64; 2]> {
    const GAP: f64 = 3.0;
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup();
    let mut segs: Vec<[f64; 2]> = vec![];
    for t in times {
        match segs.last_mut() {
            Some(s) if t - s[1] <= GAP => s[1] = t,
            _ => segs.push([t, t]),
        }
    }
    let end_max = duration.map(|d| d + 1.0).unwrap_or(f64::MAX);
    segs.into_iter().map(|[a, b]| [a, (b + 1.0).min(end_max)]).collect()
}

/// Apply lead-in / tail padding, clamp to video duration, and merge overlaps.
pub fn pad_segments(segs: &[[f64; 2]], lead: f64, tail: f64, duration: Option<f64>) -> Vec<[f64; 2]> {
    let max = duration.map(|d| d + 1.0).unwrap_or(f64::INFINITY);
    let mut sorted = segs.to_vec();
    sorted.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<[f64; 2]> = vec![];
    for [a, b] in sorted {
        let p = [(a - lead).max(0.0), (b + tail).min(max)];
        if let Some(last) = out.last_mut() {
            if p[0] <= last[1] {
                last[1] = last[1].max(p[1]);
                continue;
            }
        }
        out.push(p);
    }
    out
}

// ---------------- export ----------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportItem {
    pub media_id: i64,
    #[serde(default)]
    pub segments: Vec<[f64; 2]>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub images: usize,
    pub clips: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

fn safe_stem(name: &str) -> String {
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    stem.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect()
}

fn date_prefix(time: &str) -> String {
    time.chars().take(10).filter(|c| c.is_ascii_digit()).collect()
}

/// Build the ffmpeg command for one clip. `fade` seconds of fade-in/out (0 = hard cut).
fn clip_cmd(ff: &Path, src: &Path, a: f64, len: f64, fade: f64, audio_fx: bool, dest: &Path) -> Command {
    let mut c = ffmpeg_cmd(ff);
    c.arg("-y").arg("-ss").arg(format!("{a:.2}")).arg("-i").arg(src).arg("-t").arg(format!("{len:.2}"));
    let f = fade.min(len / 3.0);
    if f > 0.05 {
        let out = (len - f).max(0.0);
        c.arg("-vf").arg(format!("fade=t=in:st=0:d={f:.2},fade=t=out:st={out:.2}:d={f:.2}"));
        if audio_fx {
            c.arg("-af").arg(format!("afade=t=in:st=0:d={f:.2},afade=t=out:st={out:.2}:d={f:.2}"));
        }
    }
    c.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-c:a", "aac", "-movflags", "+faststart"]).arg(dest);
    c
}

pub fn export(app: &AppHandle, out_dir: &Path, items: Vec<ExportItem>, fade: f64) -> Result<ExportReport> {
    let st = app.state::<AppState>();
    std::fs::create_dir_all(out_dir)?;
    let ffmpeg = download::find_ffmpeg(&st);
    let mut rep = ExportReport::default();
    let total = items.len();
    for (i, it) in items.into_iter().enumerate() {
        let row: Option<(String, String, String, String)> = st.db.lock().unwrap().query_row(
            "SELECT kind, path, orig_name, COALESCE(remote_time, imported_at) FROM media WHERE id=?1",
            [it.media_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        let Some((kind, path, orig, time)) = row else { continue };
        let _ = app.emit("progress", Progress { stage: "export", done: i, total, current: orig.clone() });
        let src = Path::new(&path);
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
        let base = format!("{}_{}", date_prefix(&time), safe_stem(&orig));
        let r: Result<()> = (|| {
            if kind == "image" || it.segments.is_empty() {
                let dest = out_dir.join(format!("{base}.{ext}"));
                if dest.exists() {
                    rep.skipped += 1;
                } else {
                    std::fs::copy(src, &dest)?;
                    rep.images += 1;
                }
                return Ok(());
            }
            let ff = ffmpeg.as_deref().ok_or_else(|| anyhow!("未找到 ffmpeg，无法剪辑视频"))?;
            for (k, [a, b]) in it.segments.iter().enumerate() {
                let dest = out_dir.join(format!("{base}_片段{}_{:02}m{:02}s.mp4", k + 1, (*a as u64) / 60, (*a as u64) % 60));
                if dest.exists() {
                    rep.skipped += 1;
                    continue;
                }
                let len = (b - a).max(0.5);
                // Videos without an audio track make -af fail; retry without the audio fade.
                if run(clip_cmd(ff, src, *a, len, fade, true, &dest)).is_err() {
                    run(clip_cmd(ff, src, *a, len, fade, false, &dest))?;
                }
                rep.clips += 1;
            }
            Ok(())
        })();
        if let Err(e) = r {
            rep.errors.push(format!("{orig}: {e:#}"));
        }
    }
    let _ = app.emit("progress", Progress { stage: "export", done: total, total, current: String::new() });
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Paths;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    #[test]
    fn store_file_separates_photos_and_videos_and_keeps_legacy_paths() {
        let root = std::env::temp_dir().join(format!("kid_finder_media_test_{}", uuid::Uuid::new_v4()));
        let paths = Paths::new(root.clone()).unwrap();
        let conn = db::open(&paths.data.join("kid-finder.db")).unwrap();
        let st = AppState {
            paths,
            db: Mutex::new(conn),
            engine: Mutex::new(None),
            processing: AtomicBool::new(false),
            bridge_running: AtomicBool::new(false),
            web_cache: Mutex::new(HashMap::new()),
        };

        let src_img = st.paths.tmp.join("sample.jpg");
        let src_vid = st.paths.tmp.join("sample.mp4");
        std::fs::write(&src_img, b"fake-jpeg-bytes").unwrap();
        std::fs::write(&src_vid, b"fake-mp4-bytes").unwrap();
        let sha_img = sha256_file(&src_img).unwrap();
        let sha_vid = sha256_file(&src_vid).unwrap();

        let img_id = store_file(
            &st,
            NewMedia {
                src: &src_img,
                sha: &sha_img,
                kind: "image",
                orig_name: "sample.jpg",
                source: "local",
                remote_id: None,
                remote_time: None,
                move_file: false,
            },
        )
        .unwrap();

        let vid_id = store_file(
            &st,
            NewMedia {
                src: &src_vid,
                sha: &sha_vid,
                kind: "video",
                orig_name: "sample.mp4",
                source: "local",
                remote_id: None,
                remote_time: None,
                move_file: false,
            },
        )
        .unwrap();

        let expected_img = st.paths.library.join("photos").join(&sha_img[..2]).join(format!("{sha_img}.jpg"));
        let expected_vid = st.paths.library.join("videos").join(&sha_vid[..2]).join(format!("{sha_vid}.mp4"));
        assert!(expected_img.is_file());
        assert!(expected_vid.is_file());

        let c = st.db.lock().unwrap();
        let db_img_path: String = c.query_row("SELECT path FROM media WHERE id=?1", [img_id], |r| r.get(0)).unwrap();
        let db_vid_path: String = c.query_row("SELECT path FROM media WHERE id=?1", [vid_id], |r| r.get(0)).unwrap();
        assert_eq!(PathBuf::from(&db_img_path), expected_img);
        assert_eq!(PathBuf::from(&db_vid_path), expected_vid);

        // Verify backward compatibility with pre-existing legacy rows under library/<sha2>/...
        let legacy_dir = st.paths.library.join("ab");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_file = legacy_dir.join("abcdef123456.jpg");
        std::fs::write(&legacy_file, b"legacy-img").unwrap();
        c.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('abcdef123456','image',?1,'old.jpg','local','2026-01-01 00:00:00',1)",
            params![legacy_file.to_string_lossy()],
        )
        .unwrap();
        let legacy_mid = c.last_insert_rowid();
        c.execute("INSERT INTO persons(name, created_at) VALUES('宝宝', '2026-01-01 00:00:00')", []).unwrap();
        let pid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,NULL,'test','2026-01-01 00:00:00')",
            params![pid, db::emb_to_blob(&[1.0f32, 0.0f32])],
        )
        .unwrap();
        c.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,NULL,0.1,0.1,0.5,0.5,0.9,?2)",
            params![legacy_mid, db::emb_to_blob(&[1.0f32, 0.0f32])],
        )
        .unwrap();
        c.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,NULL,0.2,0.2,0.6,0.6,0.9,?2)",
            params![img_id, db::emb_to_blob(&[1.0f32, 0.0f32])],
        )
        .unwrap();

        let hits = search(&c, pid, 0.5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| PathBuf::from(&h.path) == legacy_file));
        assert!(hits.iter().any(|h| PathBuf::from(&h.path) == expected_img));

        drop(c);
        drop(st);
        let _ = std::fs::remove_dir_all(&root);
    }
}
