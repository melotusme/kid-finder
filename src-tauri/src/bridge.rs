//! Local loopback-only HTTP bridge (127.0.0.1:27431) for the Chrome extension on qxc.hi.cn.
//! Reuses the desktop app's native ONNX Runtime FaceEngine, enrolled baby embeddings, ffmpeg
//! video analyzer/clipper, and persists scanned photo/video embeddings in SQLite so already-scanned
//! items are never downloaded or inferred again across restarts or incremental daily scans.
use crate::{db, download, face, media, AppState};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::stream::{self, StreamExt};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const BRIDGE_PORT: u16 = 27431;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 500 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 4000;

/// Cached per-item faces: list of (normalized [x1,y1,x2,y2], 512-d L2-normalized embedding).
pub type CachedFaces = Vec<([f32; 4], Vec<f32>)>;

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let addr = format!("127.0.0.1:{BRIDGE_PORT}");
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                app.state::<AppState>().bridge_running.store(true, Ordering::SeqCst);
                l
            }
            Err(e) => {
                eprintln!("[bridge] failed to bind {addr}: {e}");
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let app_clone = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = handle_conn(app_clone, stream).await;
            });
        }
    });
}

async fn handle_conn(app: AppHandle, mut stream: TcpStream) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            write_response(&mut stream, 431, None, &json!({"ok": false, "error": "Headers too large"})).await?;
            return Ok(());
        }
    };

    let head = std::str::from_utf8(&buf[..header_end])?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    let mut content_len: usize = 0;
    let mut origin: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let vt = v.trim();
            if kl == "content-length" {
                content_len = vt.parse().unwrap_or(0);
            } else if kl == "origin" {
                origin = Some(vt.to_string());
            }
        }
    }

    let cors_origin = allowed_origin(origin.as_deref());
    if method == "OPTIONS" {
        write_response(&mut stream, 204, cors_origin, &Value::Null).await?;
        return Ok(());
    }

    if content_len > MAX_BODY_BYTES {
        write_response(&mut stream, 413, cors_origin, &json!({"ok": false, "error": "请求体过大"})).await?;
        return Ok(());
    }

    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);

    let (status, payload) = match (method, path) {
        ("GET", "/api/status") => match api_status(&app).await {
            Ok(v) => (200, v),
            Err(e) => (500, json!({"ok": false, "error": format!("{e:#}")})),
        },
        ("POST", "/api/check-scanned") => match serde_json::from_slice::<CheckScannedReq>(&body) {
            Ok(req) => match api_check_scanned(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/match-urls") => match serde_json::from_slice::<MatchReq>(&body) {
            Ok(req) => match api_match_urls(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/analyze-video") => match serde_json::from_slice::<AnalyzeVideoReq>(&body) {
            Ok(req) => match api_analyze_video(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/export-clips") => match serde_json::from_slice::<ExportClipsReq>(&body) {
            Ok(req) => match api_export_clips(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/open-export-dir") => match serde_json::from_slice::<OpenExportDirReq>(&body) {
            Ok(req) => match api_open_export_dir(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/enroll") => match serde_json::from_slice::<EnrollReq>(&body) {
            Ok(req) => match api_enroll(&app, req).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/threshold") => match serde_json::from_slice::<ThresholdReq>(&body) {
            Ok(req) => match api_threshold(&app, req.value).await {
                Ok(v) => (200, v),
                Err(e) => (400, json!({"ok": false, "error": format!("{e:#}")})),
            },
            Err(e) => (400, json!({"ok": false, "error": format!("参数错误: {e}")})),
        },
        ("POST", "/api/clear-web-cache") => match api_clear_web_cache(&app).await {
            Ok(v) => (200, v),
            Err(e) => (500, json!({"ok": false, "error": format!("{e:#}")})),
        },
        _ => (404, json!({"ok": false, "error": "Not found"})),
    };

    write_response(&mut stream, status, cors_origin, &payload).await?;
    Ok(())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn allowed_origin(origin: Option<&str>) -> Option<&str> {
    let o = origin?;
    if o.starts_with("chrome-extension://") || o == "https://qxc.hi.cn" {
        Some(o)
    } else {
        None
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, origin: Option<&str>, body: &Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let bytes = if status == 204 { Vec::new() } else { serde_json::to_vec(body)? };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\n",
        bytes.len()
    );
    if let Some(o) = origin {
        head.push_str(&format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !bytes.is_empty() {
        stream.write_all(&bytes).await?;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

// ---------------- SQLite persistent web scan cache & video scoring ----------------

fn normalize_item_id(id: &str, url: &str) -> String {
    let base = if !id.is_empty() { id } else { url };
    base.split('?').next().unwrap_or(base).to_string()
}

/// Load persisted faces for an image item_id from SQLite if it has been scanned before.
/// Returns `Ok(Some(vec![]))` when the photo was scanned and had 0 faces, or `Ok(None)` if never scanned.
pub fn load_db_scan(c: &Connection, item_id: &str) -> Result<Option<CachedFaces>> {
    let exists: Option<i64> = c
        .query_row("SELECT 1 FROM web_scans WHERE item_id=?1", [item_id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut s = c.prepare("SELECT x1, y1, x2, y2, embedding FROM web_faces WHERE item_id=?1")?;
    let rows = s
        .query_map([item_id], |r| {
            let bbox = [r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?];
            let blob: Vec<u8> = r.get(4)?;
            Ok((bbox, db::blob_to_emb(&blob)))
        })?
        .collect::<rusqlite::Result<CachedFaces>>()?;
    Ok(Some(rows))
}

pub fn save_db_scans(c: &mut Connection, entries: &HashMap<String, CachedFaces>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let now = db::now();
    let tx = c.transaction()?;
    for (item_id, faces) in entries {
        tx.execute(
            "INSERT INTO web_scans(item_id, scanned_at) VALUES(?1, ?2) ON CONFLICT(item_id) DO UPDATE SET scanned_at=excluded.scanned_at",
            params![item_id, now],
        )?;
        tx.execute("DELETE FROM web_faces WHERE item_id=?1", [item_id])?;
        for (bbox, emb) in faces {
            tx.execute(
                "INSERT INTO web_faces(item_id, x1, y1, x2, y2, embedding) VALUES(?1,?2,?3,?4,?5,?6)",
                params![item_id, bbox[0], bbox[1], bbox[2], bbox[3], db::emb_to_blob(emb)],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn score_faces(faces: &CachedFaces, refs: &[Vec<f32>]) -> (f32, Option<[f32; 4]>) {
    let mut max_score = 0.0f32;
    let mut best_bbox: Option<[f32; 4]> = None;
    for (bbox, emb) in faces {
        let sim = refs.iter().map(|q| face::cosine(q, emb)).fold(f32::MIN, f32::max);
        if sim > max_score {
            max_score = sim;
            best_bbox = Some(*bbox);
        }
    }
    (max_score, best_bbox)
}

struct VideoScore {
    media_id: i64,
    duration: Option<f64>,
    max_score: f32,
    face_count: usize,
    segments: Vec<[f64; 2]>,
}

/// Score an already-analyzed video in `media` / `faces` against `refs` at `threshold`.
fn score_analyzed_video(c: &Connection, media_id: i64, person_id: Option<i64>, refs: &[Vec<f32>], threshold: f32) -> Result<VideoScore> {
    let duration: Option<f64> = c.query_row("SELECT duration FROM media WHERE id=?1", [media_id], |r| r.get(0))?;
    let rejected: std::collections::HashSet<i64> = match person_id {
        Some(pid) => c
            .prepare("SELECT face_id FROM rejections WHERE person_id=?1")?
            .query_map([pid], |r| r.get(0))?
            .flatten()
            .collect(),
        None => std::collections::HashSet::new(),
    };
    let mut s = c.prepare("SELECT id, t, embedding FROM faces WHERE media_id=?1")?;
    let mut rows = s.query([media_id])?;
    let mut max_score = 0.0f32;
    let mut face_count = 0usize;
    let mut times: Vec<f64> = vec![];
    while let Some(r) = rows.next()? {
        face_count += 1;
        let fid: i64 = r.get(0)?;
        if rejected.contains(&fid) || refs.is_empty() {
            continue;
        }
        let t: Option<f64> = r.get(1)?;
        let emb = db::blob_to_emb(&r.get::<_, Vec<u8>>(2)?);
        let sim = refs.iter().map(|q| face::cosine(q, &emb)).fold(f32::MIN, f32::max);
        if sim > max_score {
            max_score = sim;
        }
        if sim >= threshold {
            if let Some(ts) = t {
                times.push(ts);
            }
        }
    }
    let segments = if times.is_empty() { vec![] } else { media::merge_segments(times, duration) };
    Ok(VideoScore { media_id, duration, max_score, face_count, segments })
}

fn find_analyzed_video_by_remote(c: &Connection, remote_id: &str) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT id FROM media WHERE remote_id=?1 AND kind='video' AND status=1",
        [remote_id],
        |r| r.get(0),
    )
    .optional()?)
}

fn default_export_root(app: &AppHandle) -> PathBuf {
    if let Ok(dl) = app.path().download_dir() {
        return dl.join("找娃助手导出");
    }
    app.state::<AppState>().paths.data.join("exports")
}

fn safe_folder_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    if cleaned.is_empty() { "宝宝".into() } else { cleaned }
}

// ---------------- endpoints ----------------

/// Deterministic SHA-256 hex digest over ordered rows of `persons`, `refs`, and `rejections`.
/// Changes whenever any baby is added/renamed/deleted, any reference photo is added/deleted/confirmed,
/// or any face rejection is added/removed.
pub fn compute_refs_signature(c: &Connection) -> Result<String> {
    let mut h = Sha256::new();

    let mut p_stmt = c.prepare("SELECT id, name FROM persons ORDER BY id")?;
    let mut p_rows = p_stmt.query([])?;
    while let Some(r) = p_rows.next()? {
        let id: i64 = r.get(0)?;
        let name: String = r.get(1)?;
        h.update(b"P:");
        h.update(id.to_le_bytes());
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update(b"\n");
    }

    let mut r_stmt = c.prepare("SELECT id, person_id, embedding FROM refs ORDER BY id")?;
    let mut r_rows = r_stmt.query([])?;
    while let Some(r) = r_rows.next()? {
        let id: i64 = r.get(0)?;
        let person_id: i64 = r.get(1)?;
        let emb: Vec<u8> = r.get(2)?;
        h.update(b"R:");
        h.update(id.to_le_bytes());
        h.update(person_id.to_le_bytes());
        h.update((emb.len() as u64).to_le_bytes());
        h.update(&emb);
        h.update(b"\n");
    }

    let mut x_stmt = c.prepare("SELECT person_id, face_id FROM rejections ORDER BY person_id, face_id")?;
    let mut x_rows = x_stmt.query([])?;
    while let Some(r) = x_rows.next()? {
        let person_id: i64 = r.get(0)?;
        let face_id: i64 = r.get(1)?;
        h.update(b"X:");
        h.update(person_id.to_le_bytes());
        h.update(face_id.to_le_bytes());
        h.update(b"\n");
    }

    Ok(hex::encode(h.finalize()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgePerson {
    id: i64,
    name: String,
    ref_count: usize,
    avatar: Option<String>,
}

async fn api_status(app: &AppHandle) -> Result<Value> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let st = app.state::<AppState>();
        let models_ready = download::models_ready(&st.paths);
        let ffmpeg_ready = download::find_ffmpeg(&st).is_some();
        let export_root = default_export_root(&app).to_string_lossy().to_string();
        let c = st.db.lock().unwrap();
        let thresh: f32 = db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40);
        let scanned_count: i64 = c.query_row("SELECT COUNT(*) FROM web_scans", [], |r| r.get(0))?;
        let video_scanned_count: i64 =
            c.query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status=1", [], |r| r.get(0))?;
        let refs_signature = compute_refs_signature(&c)?;
        let mut ps: Vec<(i64, String)> = c
            .prepare("SELECT id, name FROM persons ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut ref_stmt = c.prepare("SELECT thumb FROM refs WHERE person_id=?1 ORDER BY id")?;
        let mut persons = Vec::with_capacity(ps.len());
        for (id, name) in ps.drain(..) {
            let thumbs: Vec<Option<String>> = ref_stmt
                .query_map([id], |r| r.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let ref_count = thumbs.len();
            let avatar = thumbs
                .into_iter()
                .flatten()
                .find_map(|p| std::fs::read(&p).ok())
                .map(|bytes| format!("data:image/jpeg;base64,{}", B64.encode(bytes)));
            persons.push(BridgePerson { id, name, ref_count, avatar });
        }
        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "modelsReady": models_ready,
            "ffmpegReady": ffmpeg_ready,
            "threshold": thresh,
            "scannedCount": scanned_count,
            "videoScannedCount": video_scanned_count,
            "defaultExportDir": export_root,
            "refsSignature": refs_signature,
            "persons": persons,
        }))
    })
    .await?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckScannedReq {
    person_id: Option<i64>,
    threshold: Option<f32>,
    ids: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckScannedItem {
    id: String,
    kind: String,
    scanned: bool,
    cached: bool,
    max_score: f32,
    face_count: usize,
    best_bbox: Option<[f32; 4]>,
    media_id: Option<i64>,
    duration: Option<f64>,
    segments: Vec<[f64; 2]>,
}

/// Fast local-only lookup (0 network, 0 ONNX) to identify which page photos AND videos have
/// already been scanned before and compute their scores/segments against the selected baby.
async fn api_check_scanned(app: &AppHandle, req: CheckScannedReq) -> Result<Value> {
    if req.ids.len() > 2000 {
        bail!("单次查询最多 2000 条");
    }
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let st = app_clone.state::<AppState>();
        let c = st.db.lock().unwrap();
        let thresh = req
            .threshold
            .unwrap_or_else(|| db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40));
        let refs = match req.person_id {
            Some(pid) => media::person_refs(&c, pid).unwrap_or_default(),
            None => vec![],
        };
        let scanned_count: i64 = c.query_row("SELECT COUNT(*) FROM web_scans", [], |r| r.get(0))?;
        let video_scanned_count: i64 =
            c.query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status=1", [], |r| r.get(0))?;
        let mut cache = st.web_cache.lock().unwrap();
        let mut out = Vec::with_capacity(req.ids.len());
        for raw_id in req.ids {
            let key = normalize_item_id(&raw_id, "");
            // 1. Check analyzed videos in `media` table
            if let Some(mid) = find_analyzed_video_by_remote(&c, &key)? {
                let vs = score_analyzed_video(&c, mid, req.person_id, &refs, thresh)?;
                out.push(CheckScannedItem {
                    id: raw_id,
                    kind: "video".into(),
                    scanned: true,
                    cached: true,
                    max_score: vs.max_score,
                    face_count: vs.face_count,
                    best_bbox: None,
                    media_id: Some(vs.media_id),
                    duration: vs.duration,
                    segments: vs.segments,
                });
                continue;
            }
            // 2. Check persisted photo scans in L1 + `web_scans`
            let faces_opt = if let Some(f) = cache.get(&key) {
                Some(f.clone())
            } else if let Some(f) = load_db_scan(&c, &key)? {
                if cache.len() < MAX_CACHE_ENTRIES {
                    cache.insert(key.clone(), f.clone());
                }
                Some(f)
            } else {
                None
            };
            match faces_opt {
                Some(fs) => {
                    let (max_score, best_bbox) = if refs.is_empty() { (0.0, None) } else { score_faces(&fs, &refs) };
                    out.push(CheckScannedItem {
                        id: raw_id,
                        kind: "image".into(),
                        scanned: true,
                        cached: true,
                        max_score,
                        face_count: fs.len(),
                        best_bbox,
                        media_id: None,
                        duration: None,
                        segments: vec![],
                    });
                }
                None => {
                    out.push(CheckScannedItem {
                        id: raw_id,
                        kind: "unknown".into(),
                        scanned: false,
                        cached: false,
                        max_score: 0.0,
                        face_count: 0,
                        best_bbox: None,
                        media_id: None,
                        duration: None,
                        segments: vec![],
                    });
                }
            }
        }
        Ok(json!({
            "ok": true,
            "scannedCount": scanned_count,
            "videoScannedCount": video_scanned_count,
            "results": out
        }))
    })
    .await?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchItem {
    id: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchReq {
    person_id: i64,
    items: Vec<MatchItem>,
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MatchResult {
    id: String,
    url: String,
    kind: String,
    cached: bool,
    max_score: f32,
    face_count: usize,
    best_bbox: Option<[f32; 4]>,
    error: Option<String>,
}

fn is_allowed_media_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else { return false };
    let host = host.to_ascii_lowercase();
    host == "qxc.hi.cn"
        || host.ends_with(".qxc.hi.cn")
        || host.ends_with(".myqcloud.com")
        || host.ends_with(".qcloud.la")
        || host.ends_with(".tcloudbase.com")
        || host.ends_with(".aliyuncs.com")
}

async fn api_match_urls(app: &AppHandle, req: MatchReq) -> Result<Value> {
    if req.items.len() > 64 {
        bail!("单次批量最多 64 张图片");
    }

    let app_init = app.clone();
    let pid = req.person_id;
    let force = req.force;
    let items_meta: Vec<(String, String, String)> = req
        .items
        .iter()
        .map(|it| (it.id.clone(), it.url.clone(), normalize_item_id(&it.id, &it.url)))
        .collect();

    let (refs, mut cached_map, to_fetch): (Vec<Vec<f32>>, HashMap<String, CachedFaces>, Vec<(String, String)>) =
        tauri::async_runtime::spawn_blocking(move || -> Result<_> {
            let st = app_init.state::<AppState>();
            let c = st.db.lock().unwrap();
            let refs = media::person_refs(&c, pid)?;
            if refs.is_empty() {
                bail!("该宝宝还没有录入参考照");
            }
            let mut cached: HashMap<String, CachedFaces> = HashMap::new();
            let mut missing: Vec<(String, String)> = vec![];
            let mut mem = st.web_cache.lock().unwrap();
            for (_id, url, key) in &items_meta {
                if !force {
                    if let Some(f) = mem.get(key) {
                        cached.insert(key.clone(), f.clone());
                        continue;
                    }
                    if let Some(f) = load_db_scan(&c, key)? {
                        if mem.len() < MAX_CACHE_ENTRIES {
                            mem.insert(key.clone(), f.clone());
                        }
                        cached.insert(key.clone(), f);
                        continue;
                    }
                }
                if is_allowed_media_url(url) {
                    missing.push((key.clone(), url.clone()));
                }
            }
            drop(mem);
            drop(c);
            if !missing.is_empty() {
                media::ensure_engine(&st)?;
            }
            Ok((refs, cached, missing))
        })
        .await??;

    let was_cached_keys: std::collections::HashSet<String> = cached_map.keys().cloned().collect();

    let downloaded: Vec<(String, Result<Vec<u8>>)> = if to_fetch.is_empty() {
        vec![]
    } else {
        let http = reqwest::Client::builder()
            .user_agent("kid-finder")
            .timeout(Duration::from_secs(20))
            .build()?;
        stream::iter(to_fetch)
            .map(|(key, url)| {
                let http = http.clone();
                async move {
                    let r = async {
                        let resp = http.get(&url).send().await?.error_for_status()?;
                        let bytes = resp.bytes().await?;
                        if bytes.len() > 15 * 1024 * 1024 {
                            bail!("图片过大");
                        }
                        Ok(bytes.to_vec())
                    }
                    .await;
                    (key, r)
                }
            })
            .buffer_unordered(6)
            .collect()
            .await
    };

    let app_inf = app.clone();
    let (new_faces, errors, scanned_count): (HashMap<String, CachedFaces>, HashMap<String, String>, i64) =
        tauri::async_runtime::spawn_blocking(move || -> Result<_> {
            let st = app_inf.state::<AppState>();
            let mut out_faces: HashMap<String, CachedFaces> = HashMap::new();
            let mut out_err: HashMap<String, String> = HashMap::new();
            for (key, bytes_res) in downloaded {
                match bytes_res {
                    Err(e) => {
                        out_err.insert(key, format!("{e:#}"));
                    }
                    Ok(bytes) => {
                        let analyzed: Result<CachedFaces> = (|| {
                            let img = face::load_image_bytes(&bytes)?;
                            let (w, h) = (img.width() as f32, img.height() as f32);
                            let faces = st
                                .engine
                                .lock()
                                .unwrap()
                                .as_mut()
                                .ok_or_else(|| anyhow!("模型未加载"))?
                                .analyze(&img)?;
                            Ok(faces
                                .into_iter()
                                .map(|f| ([f.bbox[0] / w, f.bbox[1] / h, f.bbox[2] / w, f.bbox[3] / h], f.embedding))
                                .collect())
                        })();
                        match analyzed {
                            Ok(f) => {
                                out_faces.insert(key, f);
                            }
                            Err(e) => {
                                out_err.insert(key, format!("{e:#}"));
                            }
                        }
                    }
                }
            }
            let total_scanned: i64 = {
                let mut c = st.db.lock().unwrap();
                if !out_faces.is_empty() {
                    save_db_scans(&mut c, &out_faces)?;
                }
                c.query_row("SELECT COUNT(*) FROM web_scans", [], |r| r.get(0))?
            };
            if !out_faces.is_empty() {
                let mut cache = st.web_cache.lock().unwrap();
                if cache.len() + out_faces.len() > MAX_CACHE_ENTRIES {
                    cache.clear();
                }
                for (k, v) in &out_faces {
                    cache.insert(k.clone(), v.clone());
                }
            }
            Ok((out_faces, out_err, total_scanned))
        })
        .await??;

    for (k, v) in new_faces {
        cached_map.insert(k, v);
    }

    let mut results = Vec::with_capacity(req.items.len());
    for it in req.items {
        let key = normalize_item_id(&it.id, &it.url);
        let cached = was_cached_keys.contains(&key);
        if !cached && !is_allowed_media_url(&it.url) {
            results.push(MatchResult {
                id: it.id,
                url: it.url,
                kind: "image".into(),
                cached: false,
                max_score: 0.0,
                face_count: 0,
                best_bbox: None,
                error: Some("不支持的图片来源".into()),
            });
            continue;
        }
        if let Some(err) = errors.get(&key) {
            results.push(MatchResult {
                id: it.id,
                url: it.url,
                kind: "image".into(),
                cached: false,
                max_score: 0.0,
                face_count: 0,
                best_bbox: None,
                error: Some(err.clone()),
            });
            continue;
        }
        let faces = cached_map.get(&key);
        let face_count = faces.map(|f| f.len()).unwrap_or(0);
        let (max_score, best_bbox) = faces.map(|fs| score_faces(fs, &refs)).unwrap_or((0.0, None));
        results.push(MatchResult {
            id: it.id,
            url: it.url,
            kind: "image".into(),
            cached,
            max_score,
            face_count,
            best_bbox,
            error: None,
        });
    }

    Ok(json!({ "ok": true, "scannedCount": scanned_count, "results": results }))
}

// ---------------- video download, 1fps analysis & auto-clip export ----------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeVideoReq {
    person_id: i64,
    id: String,
    url: String,
    threshold: Option<f32>,
}

/// Downloads a qxc.hi.cn video into the local library (if not already present), runs full 1fps
/// ffmpeg + ONNX face analysis, persists results in `media` / `faces`, and returns matching segments.
async fn api_analyze_video(app: &AppHandle, req: AnalyzeVideoReq) -> Result<Value> {
    let key = normalize_item_id(&req.id, &req.url);
    let raw_url = req.url.split('?').next().unwrap_or(&req.url).to_string();
    if !is_allowed_media_url(&raw_url) {
        bail!("不支持的视频来源");
    }

    // 1. Check if this video has already been analyzed before (0 download, 0 inference!).
    let app_check = app.clone();
    let key_check = key.clone();
    let pid = req.person_id;
    let req_thresh = req.threshold;
    let pre = tauri::async_runtime::spawn_blocking(move || -> Result<(Vec<Vec<f32>>, f32, Option<VideoScore>, Option<PathBuf>)> {
        let st = app_check.state::<AppState>();
        let c = st.db.lock().unwrap();
        let refs = media::person_refs(&c, pid)?;
        if refs.is_empty() {
            bail!("该宝宝还没有录入参考照");
        }
        let thresh = req_thresh
            .unwrap_or_else(|| db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40));
        if let Some(mid) = find_analyzed_video_by_remote(&c, &key_check)? {
            let vs = score_analyzed_video(&c, mid, Some(pid), &refs, thresh)?;
            return Ok((refs, thresh, Some(vs), None));
        }
        drop(c);
        media::ensure_engine(&st)?;
        let ff = download::find_ffmpeg(&st)
            .ok_or_else(|| anyhow!("处理视频需要 ffmpeg，请先在桌面端「设置」点「自动安装」"))?;
        Ok((refs, thresh, None, Some(ff)))
    })
    .await??;

    let (refs, thresh, cached_score, ffmpeg_path) = pre;
    if let Some(vs) = cached_score {
        let app_cnt = app.clone();
        let video_count: i64 = tauri::async_runtime::spawn_blocking(move || -> Result<i64> {
            Ok(app_cnt
                .state::<AppState>()
                .db
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status=1", [], |r| r.get(0))?)
        })
        .await??;
        return Ok(json!({
            "ok": true,
            "id": req.id,
            "kind": "video",
            "cached": true,
            "mediaId": vs.media_id,
            "maxScore": vs.max_score,
            "faceCount": vs.face_count,
            "duration": vs.duration,
            "segments": vs.segments,
            "videoScannedCount": video_count,
        }));
    }

    let ff = ffmpeg_path.ok_or_else(|| anyhow!("缺少 ffmpeg"))?;

    // 2. Stream-download the full MP4 into st.paths.tmp.
    let tmp_path = {
        let st = app.state::<AppState>();
        let name = raw_url.rsplit('/').next().unwrap_or("video.mp4");
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        st.paths.tmp.join(format!("ext_vid_{}.{ext}", uuid::Uuid::new_v4()))
    };
    let http = reqwest::Client::builder()
        .user_agent("kid-finder")
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()?;
    let resp = http.get(&raw_url).send().await?.error_for_status()?;
    if let Some(len) = resp.content_length() {
        if len > MAX_VIDEO_BYTES {
            bail!("视频超过 500MB，已跳过");
        }
    }
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut downloaded_len: u64 = 0;
    let mut body_stream = resp.bytes_stream();
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk?;
        downloaded_len += chunk.len() as u64;
        if downloaded_len > MAX_VIDEO_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            bail!("视频超过 500MB，已跳过");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    // 3. Deduplicate by sha256, store in library, analyze at 1fps, save faces in SQLite, score.
    let app_run = app.clone();
    let orig_name = raw_url.rsplit('/').next().unwrap_or("video.mp4").to_string();
    let tmp_for_cleanup = tmp_path.clone();
    let out = tauri::async_runtime::spawn_blocking(move || -> Result<(VideoScore, bool, i64)> {
        let st = app_run.state::<AppState>();
        let sha = media::sha256_file(&tmp_path)?;
        let existing: Option<(i64, String, i64)> = st
            .db
            .lock()
            .unwrap()
            .query_row("SELECT id, path, status FROM media WHERE sha256=?1", [&sha], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;

        let (media_id, stored_path, already_done) = if let Some((id, path, status)) = existing {
            let _ = std::fs::remove_file(&tmp_path);
            st.db.lock().unwrap().execute(
                "UPDATE media SET remote_id=COALESCE(remote_id, ?1) WHERE id=?2",
                params![key, id],
            )?;
            (id, path, status == 1)
        } else {
            let id = media::store_file(
                &st,
                media::NewMedia {
                    src: &tmp_path,
                    sha: &sha,
                    kind: "video",
                    orig_name: &orig_name,
                    source: "qxc-ext",
                    remote_id: Some(&key),
                    remote_time: None,
                    move_file: true,
                },
            )?;
            let path: String = st
                .db
                .lock()
                .unwrap()
                .query_row("SELECT path FROM media WHERE id=?1", [id], |r| r.get(0))?;
            (id, path, false)
        };

        if !already_done {
            let res = media::analyze_video(&st, Some(&ff), media_id, Path::new(&stored_path));
            let mut c = st.db.lock().unwrap();
            match res {
                Ok((rows, poster, duration)) => {
                    let tx = c.transaction()?;
                    tx.execute("DELETE FROM faces WHERE media_id=?1", [media_id])?;
                    for (t, b, s, e) in rows {
                        tx.execute(
                            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                            params![media_id, t, b[0], b[1], b[2], b[3], s, db::emb_to_blob(&e)],
                        )?;
                    }
                    tx.execute(
                        "UPDATE media SET status=1, error=NULL, poster=COALESCE(?2,poster), duration=COALESCE(?3,duration) WHERE id=?1",
                        params![media_id, poster, duration],
                    )?;
                    tx.commit()?;
                }
                Err(e) => {
                    c.execute("UPDATE media SET status=2, error=?2 WHERE id=?1", params![media_id, format!("{e:#}")])?;
                    return Err(e);
                }
            }
        }

        let c = st.db.lock().unwrap();
        let vs = score_analyzed_video(&c, media_id, Some(pid), &refs, thresh)?;
        let video_count: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status=1", [], |r| r.get(0))?;
        let _ = app_run.emit("process-done", Option::<String>::None);
        Ok((vs, already_done, video_count))
    })
    .await;

    let _ = tokio::fs::remove_file(&tmp_for_cleanup).await;
    let (vs, was_cached, video_count) = out??;

    Ok(json!({
        "ok": true,
        "id": req.id,
        "kind": "video",
        "cached": was_cached,
        "mediaId": vs.media_id,
        "maxScore": vs.max_score,
        "faceCount": vs.face_count,
        "duration": vs.duration,
        "segments": vs.segments,
        "videoScannedCount": video_count,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportClipsReq {
    person_id: i64,
    threshold: Option<f32>,
    media_ids: Vec<i64>,
    lead: Option<f64>,
    tail: Option<f64>,
    fade: Option<f64>,
    #[serde(default)]
    open_folder: bool,
}

/// Automatically crops matched videos to baby-only clips using ffmpeg (with lead/tail padding & fade)
/// and writes them into `<Downloads>/找娃助手导出/<宝宝名>/`.
async fn api_export_clips(app: &AppHandle, req: ExportClipsReq) -> Result<Value> {
    if req.media_ids.is_empty() {
        bail!("没有需要裁剪的视频");
    }
    let lead = req.lead.unwrap_or(2.0).clamp(0.0, 30.0);
    let tail = req.tail.unwrap_or(2.0).clamp(0.0, 30.0);
    let fade = req.fade.unwrap_or(0.5).clamp(0.0, 3.0);
    let app_clone = app.clone();

    let (out_dir, rep) = tauri::async_runtime::spawn_blocking(move || -> Result<(PathBuf, media::ExportReport)> {
        let st = app_clone.state::<AppState>();
        let (person_name, refs, thresh, items) = {
            let c = st.db.lock().unwrap();
            let pname: String = c
                .query_row("SELECT name FROM persons WHERE id=?1", [req.person_id], |r| r.get(0))
                .unwrap_or_else(|_| "宝宝".into());
            let refs = media::person_refs(&c, req.person_id)?;
            if refs.is_empty() {
                bail!("该宝宝还没有录入参考照");
            }
            let thresh = req
                .threshold
                .unwrap_or_else(|| db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40));
            let mut export_items = Vec::new();
            for mid in &req.media_ids {
                let vs = score_analyzed_video(&c, *mid, Some(req.person_id), &refs, thresh)?;
                if vs.segments.is_empty() {
                    continue;
                }
                let padded = media::pad_segments(&vs.segments, lead, tail, vs.duration);
                export_items.push(media::ExportItem { media_id: *mid, segments: padded });
            }
            (pname, refs, thresh, export_items)
        };
        let _ = (refs, thresh);
        if items.is_empty() {
            bail!("选中的视频在当前严格程度下没有命中宝宝片段");
        }
        let out_dir = default_export_root(&app_clone).join(safe_folder_name(&person_name));
        let rep = media::export(&app_clone, &out_dir, items, fade)?;
        Ok((out_dir, rep))
    })
    .await??;

    if req.open_folder && (rep.clips > 0 || rep.skipped > 0) {
        let _ = app.opener().open_path(out_dir.to_string_lossy().to_string(), None::<&str>);
    }

    Ok(json!({
        "ok": true,
        "outDir": out_dir.to_string_lossy(),
        "clips": rep.clips,
        "skipped": rep.skipped,
        "errors": rep.errors,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenExportDirReq {
    person_id: Option<i64>,
}

async fn api_open_export_dir(app: &AppHandle, req: OpenExportDirReq) -> Result<Value> {
    let root = default_export_root(app);
    let dir = if let Some(pid) = req.person_id {
        let app_c = app.clone();
        let pname: Option<String> = tauri::async_runtime::spawn_blocking(move || {
            app_c
                .state::<AppState>()
                .db
                .lock()
                .unwrap()
                .query_row("SELECT name FROM persons WHERE id=?1", [pid], |r| r.get(0))
                .ok()
        })
        .await?;
        match pname {
            Some(n) => root.join(safe_folder_name(&n)),
            None => root,
        }
    } else {
        root
    };
    std::fs::create_dir_all(&dir)?;
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(json!({ "ok": true, "outDir": dir.to_string_lossy() }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollReq {
    person_id: Option<i64>,
    name: Option<String>,
    images: Vec<String>,
}

async fn api_enroll(app: &AppHandle, req: EnrollReq) -> Result<Value> {
    if req.images.is_empty() {
        bail!("请至少选择一张照片");
    }
    let app_clone = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let st = app_clone.state::<AppState>();
        media::ensure_engine(&st)?;
        let person_id = if let Some(pid) = req.person_id {
            pid
        } else {
            let name = req.name.as_deref().unwrap_or("宝宝").trim();
            let name = if name.is_empty() { "宝宝" } else { name };
            let c = st.db.lock().unwrap();
            c.execute("INSERT INTO persons(name, created_at) VALUES(?1, ?2)", params![name, db::now()])?;
            c.last_insert_rowid()
        };

        let (mut added, mut no_face, mut multi_face) = (0usize, 0usize, 0usize);
        for raw in &req.images {
            let b64 = raw.split_once(',').map(|(_, b)| b).unwrap_or(raw.as_str());
            let bytes = B64.decode(b64.trim()).map_err(|e| anyhow!("图片解码失败: {e}"))?;
            let img = face::load_image_bytes(&bytes)?;
            let faces = st
                .engine
                .lock()
                .unwrap()
                .as_mut()
                .ok_or_else(|| anyhow!("模型未加载"))?
                .analyze(&img)?;
            let area = |f: &face::Face| (f.bbox[2] - f.bbox[0]) * (f.bbox[3] - f.bbox[1]);
            let Some(best) = faces.iter().max_by(|a, b| area(a).total_cmp(&area(b))) else {
                no_face += 1;
                continue;
            };
            if faces.len() > 1 {
                multi_face += 1;
            }
            let thumb = st.paths.thumbs.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
            face::face_thumb(&img, &best.bbox, 160).save(&thumb)?;
            st.db.lock().unwrap().execute(
                "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,?3,?4,?5)",
                params![person_id, db::emb_to_blob(&best.embedding), thumb.to_string_lossy(), "chrome-ext", db::now()],
            )?;
            added += 1;
        }
        let _ = app_clone.emit("persons-changed", ());
        Ok(json!({
            "ok": true,
            "personId": person_id,
            "added": added,
            "noFace": no_face,
            "multiFace": multi_face,
        }))
    })
    .await??;
    Ok(res)
}

#[derive(Deserialize)]
struct ThresholdReq {
    value: f32,
}

async fn api_threshold(app: &AppHandle, value: f32) -> Result<Value> {
    let clamped = value.clamp(0.15, 0.85);
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        let st = app_clone.state::<AppState>();
        let c = st.db.lock().unwrap();
        db::set_setting(&c, "threshold", &clamped.to_string())?;
        Ok(())
    })
    .await??;
    Ok(json!({ "ok": true, "threshold": clamped }))
}

pub fn clear_web_cache_sync(st: &AppState) -> Result<()> {
    st.db.lock().unwrap().execute("DELETE FROM web_scans", [])?;
    st.web_cache.lock().unwrap().clear();
    Ok(())
}

async fn api_clear_web_cache(app: &AppHandle) -> Result<Value> {
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        clear_web_cache_sync(&app_clone.state::<AppState>())
    })
    .await??;
    Ok(json!({ "ok": true, "scannedCount": 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_allowlist_checks_scheme_and_host() {
        assert!(is_allowed_media_url("https://releasec.qxc.hi.cn/medias/abc/1.jpg?imageMogr2/thumbnail/960x/format/jpg"));
        assert!(is_allowed_media_url("https://releasec.qxc.hi.cn/medias/abc/1.mp4"));
        assert!(is_allowed_media_url("https://foo.tcb.qcloud.la/a.jpg"));
        assert!(is_allowed_media_url("https://bucket.cos.ap-shanghai.myqcloud.com/a.jpg"));
        assert!(!is_allowed_media_url("http://releasec.qxc.hi.cn/medias/abc/1.jpg"));
        assert!(!is_allowed_media_url("http://127.0.0.1:27431/api/status"));
        assert!(!is_allowed_media_url("https://evil.example.com/qxc.hi.cn.jpg"));
        assert!(!is_allowed_media_url("file:///etc/passwd"));
    }

    #[test]
    fn cors_origin_allowlist() {
        assert_eq!(allowed_origin(Some("chrome-extension://abcdefghijklmnop")), Some("chrome-extension://abcdefghijklmnop"));
        assert_eq!(allowed_origin(Some("https://qxc.hi.cn")), Some("https://qxc.hi.cn"));
        assert_eq!(allowed_origin(Some("https://evil.com")), None);
        assert_eq!(allowed_origin(None), None);
    }

    #[test]
    fn persistent_web_scan_cache_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_db_test_{}.db", uuid::Uuid::new_v4()));
        let mut conn = db::open(&tmp).unwrap();
        let id_with_face = "https://releasec.qxc.hi.cn/medias/g1/p1_s.jpg";
        let id_no_face = "https://releasec.qxc.hi.cn/medias/g1/p2_s.jpg";

        assert!(load_db_scan(&conn, id_with_face).unwrap().is_none());
        let mut entries = HashMap::new();
        entries.insert(id_with_face.to_string(), vec![([0.1, 0.2, 0.3, 0.4], vec![0.6, 0.8])]);
        entries.insert(id_no_face.to_string(), vec![]);
        save_db_scans(&mut conn, &entries).unwrap();

        let loaded = load_db_scan(&conn, id_with_face).unwrap().expect("should exist");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, [0.1, 0.2, 0.3, 0.4]);
        let (sim, bbox) = score_faces(&loaded, &[vec![0.6, 0.8]]);
        assert!((sim - 1.0).abs() < 1e-5);
        assert_eq!(bbox, Some([0.1, 0.2, 0.3, 0.4]));

        let empty = load_db_scan(&conn, id_no_face).unwrap().expect("0-face scan should still be cached");
        assert!(empty.is_empty());

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn pad_segments_merges_overlaps_and_clamps() {
        let merged = media::pad_segments(&[[1.0, 3.0], [5.0, 8.0], [20.0, 22.0]], 2.0, 2.0, Some(22.0));
        assert_eq!(merged, vec![[0.0, 10.0], [18.0, 23.0]]);
    }

    #[test]
    fn refs_signature_and_rescoring_track_ref_and_rejection_changes() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_sig_test_{}.db", uuid::Uuid::new_v4()));
        let conn = db::open(&tmp).unwrap();

        let sig_empty = compute_refs_signature(&conn).unwrap();
        assert_eq!(sig_empty.len(), 64);
        assert_eq!(sig_empty, compute_refs_signature(&conn).unwrap());

        // 1. Add person -> signature changes
        conn.execute("INSERT INTO persons(name, created_at) VALUES('朵朵', '2026-01-01 00:00:00')", [])
            .unwrap();
        let pid = conn.last_insert_rowid();
        let sig_person = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_person, sig_empty);

        // Rename person -> signature changes
        conn.execute("UPDATE persons SET name='小朵朵' WHERE id=?1", [pid]).unwrap();
        let sig_renamed = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_renamed, sig_person);

        // Prepare cached photo faces and an analyzed video with two frames
        let photo_faces: CachedFaces = vec![([0.1, 0.1, 0.4, 0.4], vec![0.6f32, 0.8f32])];
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,remote_id,imported_at,status,duration) VALUES('sha_v1','video','/tmp/v1.mp4','v1.mp4','qxc-ext','vid-1','2026-01-01 00:00:00',1,20.0)",
            [],
        )
        .unwrap();
        let mid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,2.0,0.0,0.0,0.2,0.2,0.9,?2)",
            params![mid, db::emb_to_blob(&[1.0f32, 0.0f32])],
        )
        .unwrap();
        let fid_frame1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,12.0,0.1,0.1,0.3,0.3,0.9,?2)",
            params![mid, db::emb_to_blob(&[0.6f32, 0.8f32])],
        )
        .unwrap();
        let fid_frame2 = conn.last_insert_rowid();

        // 2. Add first ref [1.0, 0.0] -> photo sim is 0.6, video hits frame 1 (1.0) & frame 2 (0.6) at thresh 0.75 -> only frame 1
        conn.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,NULL,'test','2026-01-01 00:00:01')",
            params![pid, db::emb_to_blob(&[1.0f32, 0.0f32])],
        )
        .unwrap();
        let ref1_id = conn.last_insert_rowid();
        let sig_ref1 = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_ref1, sig_renamed);

        let refs_v1 = media::person_refs(&conn, pid).unwrap();
        let (photo_sim1, _) = score_faces(&photo_faces, &refs_v1);
        assert!((photo_sim1 - 0.6).abs() < 1e-5);
        let vs1 = score_analyzed_video(&conn, mid, Some(pid), &refs_v1, 0.75).unwrap();
        assert!((vs1.max_score - 1.0).abs() < 1e-5);
        assert_eq!(vs1.segments, vec![[2.0, 3.0]]);

        // 3. Add second ref [0.6, 0.8] -> signature changes, photo sim jumps to 1.0, video now matches both frames!
        conn.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,NULL,'test','2026-01-01 00:00:02')",
            params![pid, db::emb_to_blob(&[0.6f32, 0.8f32])],
        )
        .unwrap();
        let ref2_id = conn.last_insert_rowid();
        let sig_ref2 = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_ref2, sig_ref1);

        let refs_v2 = media::person_refs(&conn, pid).unwrap();
        let (photo_sim2, _) = score_faces(&photo_faces, &refs_v2);
        assert!((photo_sim2 - 1.0).abs() < 1e-5);
        let vs2 = score_analyzed_video(&conn, mid, Some(pid), &refs_v2, 0.75).unwrap();
        assert_eq!(vs2.segments, vec![[2.0, 3.0], [12.0, 13.0]]);

        // 4. Reject frame 2 -> signature changes, video segments recompute without frame 2
        conn.execute("INSERT INTO rejections(face_id, person_id) VALUES(?1, ?2)", params![fid_frame2, pid])
            .unwrap();
        let sig_rej = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_rej, sig_ref2);
        let vs_rej = score_analyzed_video(&conn, mid, Some(pid), &refs_v2, 0.75).unwrap();
        assert_eq!(vs_rej.segments, vec![[2.0, 3.0]]);
        let _ = fid_frame1;

        // 5. Delete ref 2 -> signature changes, photo sim drops back to 0.6
        conn.execute("DELETE FROM refs WHERE id=?1", [ref2_id]).unwrap();
        let sig_del_ref2 = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_del_ref2, sig_rej);
        let refs_v3 = media::person_refs(&conn, pid).unwrap();
        let (photo_sim3, _) = score_faces(&photo_faces, &refs_v3);
        assert!((photo_sim3 - 0.6).abs() < 1e-5);

        // 6. Delete remaining ref -> 0 refs gives 0 score & empty segments
        conn.execute("DELETE FROM refs WHERE id=?1", [ref1_id]).unwrap();
        let sig_zero_refs = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_zero_refs, sig_del_ref2);
        let refs_empty = media::person_refs(&conn, pid).unwrap();
        assert!(refs_empty.is_empty());
        let vs_zero = score_analyzed_video(&conn, mid, Some(pid), &refs_empty, 0.40).unwrap();
        assert_eq!(vs_zero.max_score, 0.0);
        assert!(vs_zero.segments.is_empty());

        // 7. Delete rejections + person -> returns to empty signature
        conn.execute("DELETE FROM rejections WHERE person_id=?1", [pid]).unwrap();
        conn.execute("DELETE FROM persons WHERE id=?1", [pid]).unwrap();
        assert_eq!(compute_refs_signature(&conn).unwrap(), sig_empty);

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }
}
