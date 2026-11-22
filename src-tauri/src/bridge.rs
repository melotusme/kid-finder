//! Local loopback-only HTTP bridge (127.0.0.1:27431) for the Chrome extension on qxc.hi.cn.
//! Reuses the desktop app's native ONNX Runtime FaceEngine + OcrEngine, enrolled baby embeddings
//! and name/alias keywords, ffmpeg video analyzer/clipper, and persists scanned photo/video
//! embeddings & OCR text blocks in SQLite so already-scanned items are never downloaded again.
use crate::{db, download, face, media, ocr, storage, AppState};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::stream::{self, StreamExt};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const BRIDGE_PORT: u16 = 27431;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 500 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 4000;
const VIDEO_STREAM_CHUNK: usize = 64 * 1024;

/// Cached per-item faces: list of `(normalized [x1,y1,x2,y2], 512-d L2-normalized embedding)`.
pub type CachedFaces = Vec<([f32; 4], Vec<f32>)>;
/// Cached per-item OCR boxes: list of `(normalized [x1,y1,x2,y2], text, norm_text, score)`.
pub type CachedOcr = Vec<([f32; 4], String, String, f32)>;

#[derive(Clone, Debug, Default)]
pub struct CachedScan {
    pub faces: CachedFaces,
    pub ocr: CachedOcr,
    pub ocr_done: bool,
}

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
    let (path, query) = raw_path.split_once('?').unwrap_or((raw_path, ""));

    let mut content_len: usize = 0;
    let mut origin: Option<String> = None;
    let mut range_header: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let vt = v.trim();
            if kl == "content-length" {
                content_len = vt.parse().unwrap_or(0);
            } else if kl == "origin" {
                origin = Some(vt.to_string());
            } else if kl == "range" {
                range_header = Some(vt.to_string());
            }
        }
    }

    let cors_origin = allowed_origin(origin.as_deref());
    if method == "OPTIONS" {
        write_response(&mut stream, 204, cors_origin, &Value::Null).await?;
        return Ok(());
    }

    if matches!(method, "GET" | "HEAD") && path.starts_with("/api/video/") {
        let id_segment = &path["/api/video/".len()..];
        handle_video_request(
            &app,
            &mut stream,
            method == "HEAD",
            id_segment,
            query,
            range_header.as_deref(),
            cors_origin,
        )
        .await?;
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
    if o.starts_with("chrome-extension://")
        || matches!(
            o,
            "https://qxc.hi.cn"
                | "http://tauri.localhost"
                | "https://tauri.localhost"
                | "tauri://localhost"
                | "http://localhost:5173"
                | "http://127.0.0.1:5173"
        )
    {
        Some(o)
    } else {
        None
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, origin: Option<&str>, body: &Value) -> Result<()> {
    write_json_response(stream, status, origin, body, false).await
}

async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    origin: Option<&str>,
    body: &Value,
    head_only: bool,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let bytes = if status == 204 { Vec::new() } else { serde_json::to_vec(body)? };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Methods: GET, HEAD, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Range\r\nAccess-Control-Expose-Headers: Accept-Ranges, Content-Range, Content-Length\r\nAccess-Control-Allow-Private-Network: true\r\n",
        bytes.len()
    );
    if let Some(o) = origin {
        head.push_str(&format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !head_only && !bytes.is_empty() {
        stream.write_all(&bytes).await?;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

// ---------------- RFC 7233 Byte-Range Video Streaming ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRangeOutcome {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

/// Parses an HTTP `Range` request header per RFC 7233 against `total_len`.
pub fn parse_byte_range(range_header: Option<&str>, total_len: u64) -> ByteRangeOutcome {
    let Some(raw) = range_header.map(str::trim).filter(|s| !s.is_empty()) else {
        return ByteRangeOutcome::Full;
    };
    let Some(spec) = raw
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("bytes="))
        .and_then(|_| raw.get(6..))
    else {
        // RFC 7233 §3.1: ignore unknown range units and serve full representation.
        return ByteRangeOutcome::Full;
    };
    if total_len == 0 {
        return ByteRangeOutcome::Unsatisfiable;
    }
    let spec = spec.trim();
    if spec.contains(',') {
        // Multi-part ranges are not used by HTML5 <video>; serve 200 OK instead of failing.
        return ByteRangeOutcome::Full;
    }
    let Some((start_str, end_str)) = spec.split_once('-') else {
        return ByteRangeOutcome::Unsatisfiable;
    };
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    if start_str.is_empty() {
        // Suffix byte-range-spec: `bytes=-<suffix>`
        let Ok(suffix) = end_str.parse::<u64>() else {
            return ByteRangeOutcome::Unsatisfiable;
        };
        if suffix == 0 {
            return ByteRangeOutcome::Unsatisfiable;
        }
        let start = total_len.saturating_sub(suffix);
        ByteRangeOutcome::Partial {
            start,
            end: total_len - 1,
        }
    } else if end_str.is_empty() {
        // Open-ended byte-range-spec: `bytes=<start>-`
        let Ok(start) = start_str.parse::<u64>() else {
            return ByteRangeOutcome::Unsatisfiable;
        };
        if start >= total_len {
            return ByteRangeOutcome::Unsatisfiable;
        }
        ByteRangeOutcome::Partial {
            start,
            end: total_len - 1,
        }
    } else {
        // Bounded byte-range-spec: `bytes=<start>-<end>`
        let (Ok(start), Ok(end)) = (start_str.parse::<u64>(), end_str.parse::<u64>()) else {
            return ByteRangeOutcome::Unsatisfiable;
        };
        if start > end || start >= total_len {
            return ByteRangeOutcome::Unsatisfiable;
        }
        ByteRangeOutcome::Partial {
            start,
            end: end.min(total_len - 1),
        }
    }
}

fn parse_strict_media_id(id_segment: &str) -> Option<i64> {
    if id_segment.is_empty() || !id_segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id_segment.parse::<i64>().ok().filter(|&id| id > 0)
}

fn has_query_flag(query: &str, key: &str) -> bool {
    query.split('&').any(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        k == key && matches!(v, "1" | "true")
    })
}

fn video_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("3gp") => "video/3gpp",
        _ => "application/octet-stream",
    }
}

/// Resolves a registered `kind='video'` row in SQLite (or its cached H.264 `thumbs/preview_{id}.mp4`)
/// strictly by integer `media_id` — zero arbitrary filesystem path access.
pub fn resolve_video_file(
    c: &Connection,
    thumbs_dir: &Path,
    media_id: i64,
    want_preview: bool,
) -> Result<Option<PathBuf>> {
    let row: Option<(String, String, i64)> = c
        .query_row(
            "SELECT kind, path, status FROM media WHERE id=?1",
            [media_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, orig_path, status)) = row else {
        return Ok(None);
    };
    if kind != "video" || status == storage::STATUS_PURGED {
        return Ok(None);
    }

    let preview_path = thumbs_dir.join(format!("preview_{media_id}.mp4"));
    let preview_ready = preview_path
        .metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);

    if want_preview {
        return Ok(if preview_ready { Some(preview_path) } else { None });
    }
    if preview_ready {
        return Ok(Some(preview_path));
    }

    let orig = PathBuf::from(orig_path);
    if orig.is_file() {
        Ok(Some(orig))
    } else {
        Ok(None)
    }
}

async fn handle_video_request(
    app: &AppHandle,
    stream: &mut TcpStream,
    head_only: bool,
    id_segment: &str,
    query: &str,
    range_header: Option<&str>,
    cors_origin: Option<&str>,
) -> Result<()> {
    let app_clone = app.clone();
    serve_video_with_lookup(
        stream,
        head_only,
        id_segment,
        query,
        range_header,
        cors_origin,
        move |media_id, want_preview| {
            let st = app_clone.state::<AppState>();
            let thumbs_dir = st.paths().thumbs;
            let c = st.db.lock().unwrap();
            resolve_video_file(&c, &thumbs_dir, media_id, want_preview)
        },
    )
    .await
}

async fn serve_video_with_lookup<F>(
    stream: &mut TcpStream,
    head_only: bool,
    id_segment: &str,
    query: &str,
    range_header: Option<&str>,
    cors_origin: Option<&str>,
    lookup: F,
) -> Result<()>
where
    F: FnOnce(i64, bool) -> Result<Option<PathBuf>> + Send + 'static,
{
    let Some(media_id) = parse_strict_media_id(id_segment) else {
        write_json_response(
            stream,
            400,
            cors_origin,
            &json!({"ok": false, "error": "Invalid video id"}),
            head_only,
        )
        .await?;
        return Ok(());
    };
    let want_preview = has_query_flag(query, "preview");
    let resolved = tokio::task::spawn_blocking(move || lookup(media_id, want_preview)).await??;

    let Some(video_path) = resolved else {
        write_json_response(
            stream,
            404,
            cors_origin,
            &json!({"ok": false, "error": "Video not found"}),
            head_only,
        )
        .await?;
        return Ok(());
    };

    stream_video_file(stream, &video_path, head_only, range_header, cors_origin).await
}

async fn stream_video_file(
    stream: &mut TcpStream,
    video_path: &Path,
    head_only: bool,
    range_header: Option<&str>,
    cors_origin: Option<&str>,
) -> Result<()> {
    let mut file = match tokio::fs::File::open(video_path).await {
        Ok(f) => f,
        Err(_) => {
            write_json_response(
                stream,
                404,
                cors_origin,
                &json!({"ok": false, "error": "Video file missing on disk"}),
                head_only,
            )
            .await?;
            return Ok(());
        }
    };
    let total_len = file.metadata().await?.len();
    let mime = video_mime_type(video_path);

    let mut cors_headers = String::from(
        "Access-Control-Allow-Methods: GET, HEAD, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Range\r\n\
         Access-Control-Expose-Headers: Accept-Ranges, Content-Range, Content-Length\r\n\
         Access-Control-Allow-Private-Network: true\r\n",
    );
    if let Some(o) = cors_origin {
        cors_headers.push_str(&format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"));
    }

    match parse_byte_range(range_header, total_len) {
        ByteRangeOutcome::Unsatisfiable => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Range: bytes */{total_len}\r\n\
                 Content-Length: 0\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\
                 {cors_headers}\r\n"
            );
            stream.write_all(head.as_bytes()).await?;
            let _ = stream.shutdown().await;
            Ok(())
        }
        ByteRangeOutcome::Full => {
            let head = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: {mime}\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Length: {total_len}\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\
                 {cors_headers}\r\n"
            );
            stream.write_all(head.as_bytes()).await?;
            if !head_only && total_len > 0 {
                stream_file_range(&mut file, stream, 0, total_len).await?;
            }
            let _ = stream.shutdown().await;
            Ok(())
        }
        ByteRangeOutcome::Partial { start, end } => {
            let send_len = end - start + 1;
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Type: {mime}\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Range: bytes {start}-{end}/{total_len}\r\n\
                 Content-Length: {send_len}\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\
                 {cors_headers}\r\n"
            );
            stream.write_all(head.as_bytes()).await?;
            if !head_only && send_len > 0 {
                stream_file_range(&mut file, stream, start, send_len).await?;
            }
            let _ = stream.shutdown().await;
            Ok(())
        }
    }
}

async fn stream_file_range(
    file: &mut tokio::fs::File,
    stream: &mut TcpStream,
    start: u64,
    mut remaining: u64,
) -> Result<()> {
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let mut chunk = vec![0u8; VIDEO_STREAM_CHUNK];
    while remaining > 0 {
        let to_read = (remaining as usize).min(chunk.len());
        let n = file.read(&mut chunk[..to_read]).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&chunk[..n]).await?;
        remaining -= n as u64;
    }
    Ok(())
}

// ---------------- SQLite persistent web scan cache & scoring ----------------

fn normalize_item_id(id: &str, url: &str) -> String {
    let base = if !id.is_empty() { id } else { url };
    base.split('?').next().unwrap_or(base).to_string()
}

/// Load persisted faces + OCR text blocks for an image `item_id` from SQLite if scanned before.
pub fn load_db_scan(c: &Connection, item_id: &str) -> Result<Option<CachedScan>> {
    let ocr_done_opt: Option<i64> = c
        .query_row("SELECT ocr_done FROM web_scans WHERE item_id=?1", [item_id], |r| r.get(0))
        .optional()?;
    let Some(ocr_done_int) = ocr_done_opt else {
        return Ok(None);
    };
    let mut s_faces = c.prepare("SELECT x1, y1, x2, y2, embedding FROM web_faces WHERE item_id=?1 ORDER BY id")?;
    let faces = s_faces
        .query_map([item_id], |r| {
            let bbox = [r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?];
            let blob: Vec<u8> = r.get(4)?;
            Ok((bbox, db::blob_to_emb(&blob)))
        })?
        .collect::<rusqlite::Result<CachedFaces>>()?;

    let mut s_ocr =
        c.prepare("SELECT x1, y1, x2, y2, text, norm_text, score FROM web_ocr_texts WHERE item_id=?1 ORDER BY id")?;
    let ocr = s_ocr
        .query_map([item_id], |r| {
            let bbox = [r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?];
            Ok((bbox, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<rusqlite::Result<CachedOcr>>()?;

    Ok(Some(CachedScan {
        faces,
        ocr,
        ocr_done: ocr_done_int != 0,
    }))
}

pub fn save_db_scans(c: &mut Connection, entries: &HashMap<String, CachedScan>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let now = db::now();
    let tx = c.transaction()?;
    for (item_id, scan) in entries {
        let ocr_done_int = if scan.ocr_done { 1i64 } else { 0i64 };
        tx.execute(
            "INSERT INTO web_scans(item_id, scanned_at, ocr_done) VALUES(?1, ?2, ?3) \
             ON CONFLICT(item_id) DO UPDATE SET scanned_at=excluded.scanned_at, ocr_done=excluded.ocr_done",
            params![item_id, now, ocr_done_int],
        )?;
        tx.execute("DELETE FROM web_faces WHERE item_id=?1", [item_id])?;
        for (bbox, emb) in &scan.faces {
            tx.execute(
                "INSERT INTO web_faces(item_id, x1, y1, x2, y2, embedding) VALUES(?1,?2,?3,?4,?5,?6)",
                params![item_id, bbox[0], bbox[1], bbox[2], bbox[3], db::emb_to_blob(emb)],
            )?;
        }
        if scan.ocr_done {
            tx.execute("DELETE FROM web_ocr_texts WHERE item_id=?1", [item_id])?;
            for (bbox, text, norm_text, score) in &scan.ocr {
                tx.execute(
                    "INSERT INTO web_ocr_texts(item_id, x1, y1, x2, y2, text, norm_text, score) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![item_id, bbox[0], bbox[1], bbox[2], bbox[3], text, norm_text, score],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn score_faces(faces: &CachedFaces, refs: &[Vec<f32>]) -> (f32, Option<[f32; 4]>) {
    if refs.is_empty() {
        return (0.0, None);
    }
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

/// Returns `(matched_keyword, full_text, bbox)` preferring longest keyword, then highest OCR score.
fn score_ocr<'a>(ocr_rows: &'a CachedOcr, keywords: &'a [String]) -> Option<(&'a str, &'a str, [f32; 4])> {
    if keywords.is_empty() || ocr_rows.is_empty() {
        return None;
    }
    let mut best: Option<(&'a str, &'a str, [f32; 4], usize, f32)> = None;
    for (bbox, text, norm_text, score) in ocr_rows {
        let Some(kw) = ocr::match_keywords(norm_text, keywords) else {
            continue;
        };
        let kw_len = kw.chars().count();
        let replace = match best {
            None => true,
            Some((_, _, _, prev_len, prev_score)) => {
                kw_len > prev_len || (kw_len == prev_len && *score > prev_score)
            }
        };
        if replace {
            best = Some((kw, text.as_str(), *bbox, kw_len, *score));
        }
    }
    best.map(|(kw, text, bbox, _, _)| (kw, text, bbox))
}

fn compute_match_source(face_hit: bool, ocr_hit: bool) -> &'static str {
    match (face_hit, ocr_hit) {
        (true, true) => "both",
        (true, false) => "face",
        (false, true) => "ocr",
        (false, false) => "none",
    }
}

struct VideoScore {
    media_id: i64,
    duration: Option<f64>,
    max_score: f32,
    face_count: usize,
    segments: Vec<[f64; 2]>,
}

/// Score an already-analyzed video in `media` / `faces` against `refs` at `threshold`.
fn score_analyzed_video(
    c: &Connection,
    media_id: i64,
    person_id: Option<i64>,
    refs: &[Vec<f32>],
    threshold: f32,
) -> Result<VideoScore> {
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
    app.state::<AppState>().paths().data.join("exports")
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

/// Deterministic SHA-256 hex digest over ordered rows of `persons(id, name, aliases)`,
/// `refs`, `rejections`, and `ocr_rejections`.
pub fn compute_refs_signature(c: &Connection) -> Result<String> {
    let mut h = Sha256::new();

    let mut p_stmt = c.prepare("SELECT id, name, aliases FROM persons ORDER BY id")?;
    let mut p_rows = p_stmt.query([])?;
    while let Some(r) = p_rows.next()? {
        let id: i64 = r.get(0)?;
        let name: String = r.get(1)?;
        let aliases: String = r.get(2)?;
        h.update(b"P:");
        h.update(id.to_le_bytes());
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        if !aliases.is_empty() {
            h.update(b":A:");
            h.update((aliases.len() as u64).to_le_bytes());
            h.update(aliases.as_bytes());
        }
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

    let mut ox_stmt = c.prepare("SELECT person_id, ocr_id FROM ocr_rejections ORDER BY person_id, ocr_id")?;
    let mut ox_rows = ox_stmt.query([])?;
    while let Some(r) = ox_rows.next()? {
        let person_id: i64 = r.get(0)?;
        let ocr_id: i64 = r.get(1)?;
        h.update(b"OX:");
        h.update(person_id.to_le_bytes());
        h.update(ocr_id.to_le_bytes());
        h.update(b"\n");
    }

    Ok(hex::encode(h.finalize()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgePerson {
    id: i64,
    name: String,
    aliases: String,
    ref_count: usize,
    keywords_count: usize,
    keywords: Vec<String>,
    avatar: Option<String>,
}

async fn api_status(app: &AppHandle) -> Result<Value> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let st = app.state::<AppState>();
        let paths = st.paths();
        let models_ready = download::models_ready(&paths);
        let ocr_models_ready = download::ocr_models_ready(&paths);
        let ffmpeg_ready = download::find_ffmpeg(&st).is_some();
        let export_root = default_export_root(&app).to_string_lossy().to_string();
        let c = st.db.lock().unwrap();
        let thresh: f32 = db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40);
        let scanned_count: i64 = c.query_row("SELECT COUNT(*) FROM web_scans", [], |r| r.get(0))?;
        let video_scanned_count: i64 =
            c.query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status=1", [], |r| r.get(0))?;
        let refs_signature = compute_refs_signature(&c)?;
        let mut ps: Vec<(i64, String, String)> = c
            .prepare("SELECT id, name, aliases FROM persons ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut ref_stmt = c.prepare("SELECT thumb FROM refs WHERE person_id=?1 ORDER BY id")?;
        let mut persons = Vec::with_capacity(ps.len());
        for (id, name, aliases) in ps.drain(..) {
            let thumbs: Vec<Option<String>> = ref_stmt
                .query_map([id], |r| r.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let ref_count = thumbs.len();
            let avatar = thumbs
                .into_iter()
                .flatten()
                .find_map(|p| std::fs::read(&p).ok())
                .map(|bytes| format!("data:image/jpeg;base64,{}", B64.encode(bytes)));
            let keywords = ocr::build_person_keywords(&name, &aliases);
            let keywords_count = keywords.len();
            persons.push(BridgePerson {
                id,
                name,
                aliases,
                ref_count,
                keywords_count,
                keywords,
                avatar,
            });
        }
        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "extensionVersion": crate::extension::bundled_extension_version(),
            "modelsReady": models_ready,
            "ocrModelsReady": ocr_models_ready,
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
    ocr_matched: bool,
    ocr_keyword: Option<String>,
    ocr_text: Option<String>,
    ocr_bbox: Option<[f32; 4]>,
    match_source: String,
    media_id: Option<i64>,
    duration: Option<f64>,
    segments: Vec<[f64; 2]>,
}

/// Fast local-only lookup (0 network, 0 ONNX) to identify which page photos AND videos have
/// already been scanned before and compute their face + OCR scores against the selected baby.
async fn api_check_scanned(app: &AppHandle, req: CheckScannedReq) -> Result<Value> {
    if req.ids.len() > 2000 {
        bail!("单次查询最多 2000 条");
    }
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let st = app_clone.state::<AppState>();
        let ocr_ready = download::ocr_models_ready(&st.paths());
        let c = st.db.lock().unwrap();
        let thresh = req
            .threshold
            .unwrap_or_else(|| db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40));
        let (refs, keywords) = match req.person_id {
            Some(pid) => (
                media::person_refs(&c, pid).unwrap_or_default(),
                media::person_keywords(&c, pid).unwrap_or_default(),
            ),
            None => (vec![], vec![]),
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
                let v_hit = !vs.segments.is_empty() || (!refs.is_empty() && vs.max_score >= thresh);
                out.push(CheckScannedItem {
                    id: raw_id,
                    kind: "video".into(),
                    scanned: true,
                    cached: true,
                    max_score: vs.max_score,
                    face_count: vs.face_count,
                    best_bbox: None,
                    ocr_matched: false,
                    ocr_keyword: None,
                    ocr_text: None,
                    ocr_bbox: None,
                    match_source: if v_hit { "face".into() } else { "none".into() },
                    media_id: Some(vs.media_id),
                    duration: vs.duration,
                    segments: vs.segments,
                });
                continue;
            }
            // 2. Check persisted photo scans in L1 + `web_scans`
            let scan_opt = if let Some(s) = cache.get(&key) {
                Some(s.clone())
            } else if let Some(s) = load_db_scan(&c, &key)? {
                if cache.len() < MAX_CACHE_ENTRIES {
                    cache.insert(key.clone(), s.clone());
                }
                Some(s)
            } else {
                None
            };
            match scan_opt {
                Some(scan) if scan.ocr_done || !ocr_ready || keywords.is_empty() => {
                    let (max_score, best_bbox) = score_faces(&scan.faces, &refs);
                    let ocr_hit_info = score_ocr(&scan.ocr, &keywords);
                    let face_hit = !refs.is_empty() && max_score >= thresh;
                    let ocr_hit = ocr_hit_info.is_some();
                    out.push(CheckScannedItem {
                        id: raw_id,
                        kind: "image".into(),
                        scanned: true,
                        cached: true,
                        max_score,
                        face_count: scan.faces.len(),
                        best_bbox,
                        ocr_matched: ocr_hit,
                        ocr_keyword: ocr_hit_info.map(|(kw, _, _)| kw.to_string()),
                        ocr_text: ocr_hit_info.map(|(_, t, _)| t.to_string()),
                        ocr_bbox: ocr_hit_info.map(|(_, _, b)| b),
                        match_source: compute_match_source(face_hit, ocr_hit).into(),
                        media_id: None,
                        duration: None,
                        segments: vec![],
                    });
                }
                // Unscanned OR scanned before OCR models were installed while baby has OCR keywords -> allow scan upgrade
                _ => {
                    out.push(CheckScannedItem {
                        id: raw_id,
                        kind: "unknown".into(),
                        scanned: false,
                        cached: false,
                        max_score: 0.0,
                        face_count: 0,
                        best_bbox: None,
                        ocr_matched: false,
                        ocr_keyword: None,
                        ocr_text: None,
                        ocr_bbox: None,
                        match_source: "none".into(),
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
    threshold: Option<f32>,
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
    ocr_matched: bool,
    ocr_keyword: Option<String>,
    ocr_text: Option<String>,
    ocr_bbox: Option<[f32; 4]>,
    match_source: String,
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
    let req_thresh = req.threshold;
    let items_meta: Vec<(String, String, String)> = req
        .items
        .iter()
        .map(|it| (it.id.clone(), it.url.clone(), normalize_item_id(&it.id, &it.url)))
        .collect();

    let (refs, keywords, thresh, mut cached_map, to_fetch, run_face, run_ocr): (
        Vec<Vec<f32>>,
        Vec<String>,
        f32,
        HashMap<String, CachedScan>,
        Vec<(String, String)>,
        bool,
        bool,
    ) = tauri::async_runtime::spawn_blocking(move || -> Result<_> {
        let st = app_init.state::<AppState>();
        let paths = st.paths();
        let face_ready = download::models_ready(&paths);
        let ocr_ready = download::ocr_models_ready(&paths);
        let c = st.db.lock().unwrap();
        let refs = media::person_refs(&c, pid)?;
        let keywords = media::person_keywords(&c, pid)?;
        if refs.is_empty() && keywords.is_empty() {
            bail!("该宝宝还没有录入参考照，也没有有效的姓名/署名关键词");
        }
        let thresh = req_thresh
            .unwrap_or_else(|| db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40));
        let mut cached: HashMap<String, CachedScan> = HashMap::new();
        let mut missing: Vec<(String, String)> = vec![];
        let mut mem = st.web_cache.lock().unwrap();
        for (_id, url, key) in &items_meta {
            if !force {
                if let Some(s) = mem.get(key) {
                    if s.ocr_done || !ocr_ready || keywords.is_empty() {
                        cached.insert(key.clone(), s.clone());
                        continue;
                    }
                }
                if let Some(s) = load_db_scan(&c, key)? {
                    if s.ocr_done || !ocr_ready || keywords.is_empty() {
                        if mem.len() < MAX_CACHE_ENTRIES {
                            mem.insert(key.clone(), s.clone());
                        }
                        cached.insert(key.clone(), s);
                        continue;
                    }
                }
            }
            if is_allowed_media_url(url) {
                missing.push((key.clone(), url.clone()));
            }
        }
        drop(mem);
        drop(c);
        if !missing.is_empty() {
            if !face_ready && !ocr_ready {
                bail!("识别模型尚未下载，请先在桌面端「设置」页下载模型");
            }
            if face_ready {
                media::ensure_engine(&st)?;
            }
            if ocr_ready {
                let _ = media::ensure_ocr_engine(&st)?;
            }
        }
        Ok((refs, keywords, thresh, cached, missing, face_ready, ocr_ready))
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
    let (new_scans, errors, scanned_count): (HashMap<String, CachedScan>, HashMap<String, String>, i64) =
        tauri::async_runtime::spawn_blocking(move || -> Result<_> {
            let st = app_inf.state::<AppState>();
            let mut out_scans: HashMap<String, CachedScan> = HashMap::new();
            let mut out_err: HashMap<String, String> = HashMap::new();
            for (key, bytes_res) in downloaded {
                match bytes_res {
                    Err(e) => {
                        out_err.insert(key, format!("{e:#}"));
                    }
                    Ok(bytes) => {
                        let analyzed: Result<CachedScan> = (|| {
                            let img = face::load_image_bytes(&bytes)?;
                            let (w, h) = (img.width() as f32, img.height() as f32);
                            let faces = if run_face {
                                let raw_faces = st
                                    .engine
                                    .lock()
                                    .unwrap()
                                    .as_mut()
                                    .ok_or_else(|| anyhow!("人脸模型未加载"))?
                                    .analyze(&img)?;
                                raw_faces
                                    .into_iter()
                                    .map(|f| ([f.bbox[0] / w, f.bbox[1] / h, f.bbox[2] / w, f.bbox[3] / h], f.embedding))
                                    .collect()
                            } else {
                                vec![]
                            };
                            let (ocr, ocr_done) = if run_ocr {
                                let boxes = media::analyze_ocr_img(&st, &img)?.unwrap_or_default();
                                let rows = boxes
                                    .into_iter()
                                    .map(|b| (b.bbox, b.text, b.norm_text, b.score))
                                    .collect();
                                (rows, true)
                            } else {
                                (vec![], false)
                            };
                            Ok(CachedScan { faces, ocr, ocr_done })
                        })();
                        match analyzed {
                            Ok(scan) => {
                                out_scans.insert(key, scan);
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
                if !out_scans.is_empty() {
                    save_db_scans(&mut c, &out_scans)?;
                }
                c.query_row("SELECT COUNT(*) FROM web_scans", [], |r| r.get(0))?
            };
            if !out_scans.is_empty() {
                let mut cache = st.web_cache.lock().unwrap();
                if cache.len() + out_scans.len() > MAX_CACHE_ENTRIES {
                    cache.clear();
                }
                for (k, v) in &out_scans {
                    cache.insert(k.clone(), v.clone());
                }
            }
            Ok((out_scans, out_err, total_scanned))
        })
        .await??;

    for (k, v) in new_scans {
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
                ocr_matched: false,
                ocr_keyword: None,
                ocr_text: None,
                ocr_bbox: None,
                match_source: "none".into(),
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
                ocr_matched: false,
                ocr_keyword: None,
                ocr_text: None,
                ocr_bbox: None,
                match_source: "none".into(),
                error: Some(err.clone()),
            });
            continue;
        }
        let scan = cached_map.get(&key);
        let face_count = scan.map(|s| s.faces.len()).unwrap_or(0);
        let (max_score, best_bbox) = scan.map(|s| score_faces(&s.faces, &refs)).unwrap_or((0.0, None));
        let ocr_hit_info = scan.and_then(|s| score_ocr(&s.ocr, &keywords));
        let face_hit = !refs.is_empty() && max_score >= thresh;
        let ocr_hit = ocr_hit_info.is_some();
        results.push(MatchResult {
            id: it.id,
            url: it.url,
            kind: "image".into(),
            cached,
            max_score,
            face_count,
            best_bbox,
            ocr_matched: ocr_hit,
            ocr_keyword: ocr_hit_info.map(|(kw, _, _)| kw.to_string()),
            ocr_text: ocr_hit_info.map(|(_, t, _)| t.to_string()),
            ocr_bbox: ocr_hit_info.map(|(_, _, b)| b),
            match_source: compute_match_source(face_hit, ocr_hit).into(),
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

async fn api_analyze_video(app: &AppHandle, req: AnalyzeVideoReq) -> Result<Value> {
    let key = normalize_item_id(&req.id, &req.url);
    let raw_url = req.url.split('?').next().unwrap_or(&req.url).to_string();
    if !is_allowed_media_url(&raw_url) {
        bail!("不支持的视频来源");
    }

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

    let tmp_path = {
        let st = app.state::<AppState>();
        let name = raw_url.rsplit('/').next().unwrap_or("video.mp4");
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        st.paths().tmp.join(format!("ext_vid_{}.{ext}", uuid::Uuid::new_v4()))
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

        let (media_id, stored_path, already_done) = match existing {
            Some((id, path, status)) if status != storage::STATUS_PURGED => {
                let _ = std::fs::remove_file(&tmp_path);
                st.db.lock().unwrap().execute(
                    "UPDATE media SET remote_id=COALESCE(remote_id, ?1) WHERE id=?2",
                    params![key, id],
                )?;
                (id, path, status == 1)
            }
            Some((id, _old_path, _purged)) => {
                // Previously purged tombstone: re-store physical video so it can be clipped if now matched
                let paths = st.paths();
                let ext = tmp_path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
                let dir = paths.library.join("videos").join(&sha[..2]);
                std::fs::create_dir_all(&dir)?;
                let dest = dir.join(format!("{sha}.{ext}"));
                if std::fs::rename(&tmp_path, &dest).is_err() {
                    std::fs::copy(&tmp_path, &dest)?;
                    let _ = std::fs::remove_file(&tmp_path);
                }
                let has_faces: i64 = st
                    .db
                    .lock()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM faces WHERE media_id=?1", [id], |r| r.get(0))?;
                let new_status = if has_faces > 0 { 1 } else { 0 };
                st.db.lock().unwrap().execute(
                    "UPDATE media SET path=?1, status=?2, remote_id=COALESCE(remote_id, ?3) WHERE id=?4",
                    params![dest.to_string_lossy(), new_status, key, id],
                )?;
                (id, dest.to_string_lossy().to_string(), new_status == 1)
            }
            None => {
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
            }
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
                        "UPDATE media SET status=1, ocr_done=1, error=NULL, poster=COALESCE(?2,poster), duration=COALESCE(?3,duration) WHERE id=?1",
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
            let aliases = ocr::default_aliases_for_name(name);
            let c = st.db.lock().unwrap();
            c.execute(
                "INSERT INTO persons(name, aliases, created_at) VALUES(?1, ?2, ?3)",
                params![name, aliases, db::now()],
            )?;
            c.last_insert_rowid()
        };

        let thumbs_dir = st.paths().thumbs;
        let (mut added, mut duplicates, mut no_face, mut multi_face) = (0usize, 0usize, 0usize, 0usize);
        for raw in &req.images {
            let b64 = raw.split_once(',').map(|(_, b)| b).unwrap_or(raw.as_str());
            let bytes = B64.decode(b64.trim()).map_err(|e| anyhow!("图片解码失败: {e}"))?;
            let sha = media::sha256_bytes(&bytes);
            if media::is_duplicate_ref_sha(&st.db.lock().unwrap(), person_id, &sha)? {
                duplicates += 1;
                continue;
            }
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
            if media::is_duplicate_ref_embedding(&st.db.lock().unwrap(), person_id, &best.embedding)? {
                duplicates += 1;
                continue;
            }
            if faces.len() > 1 {
                multi_face += 1;
            }
            let thumb = thumbs_dir.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
            face::face_thumb(&img, &best.bbox, 160).save(&thumb)?;
            st.db.lock().unwrap().execute(
                "INSERT INTO refs(person_id, embedding, thumb, source, sha256, created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![person_id, db::emb_to_blob(&best.embedding), thumb.to_string_lossy(), "chrome-ext", sha, db::now()],
            )?;
            added += 1;
        }
        let _ = app_clone.emit("persons-changed", ());
        Ok(json!({
            "ok": true,
            "personId": person_id,
            "added": added,
            "duplicates": duplicates,
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
        assert_eq!(allowed_origin(Some("http://tauri.localhost")), Some("http://tauri.localhost"));
        assert_eq!(allowed_origin(Some("https://tauri.localhost")), Some("https://tauri.localhost"));
        assert_eq!(allowed_origin(Some("tauri://localhost")), Some("tauri://localhost"));
        assert_eq!(allowed_origin(Some("http://localhost:5173")), Some("http://localhost:5173"));
        assert_eq!(allowed_origin(Some("https://evil.com")), None);
        assert_eq!(allowed_origin(None), None);
    }

    #[test]
    fn byte_range_rfc7233_parsing() {
        let total = 10_000u64;
        assert_eq!(parse_byte_range(None, total), ByteRangeOutcome::Full);
        assert_eq!(parse_byte_range(Some(""), total), ByteRangeOutcome::Full);
        assert_eq!(
            parse_byte_range(Some("bytes=0-1023"), total),
            ByteRangeOutcome::Partial { start: 0, end: 1023 }
        );
        assert_eq!(
            parse_byte_range(Some("bytes=500-"), total),
            ByteRangeOutcome::Partial { start: 500, end: 9999 }
        );
        assert_eq!(
            parse_byte_range(Some("bytes=-256"), total),
            ByteRangeOutcome::Partial { start: 9744, end: 9999 }
        );
        // Suffix larger than file clamps to full file [0, total - 1]
        assert_eq!(
            parse_byte_range(Some("bytes=-99999"), total),
            ByteRangeOutcome::Partial { start: 0, end: 9999 }
        );
        // End past EOF clamps to total - 1 when start < total
        assert_eq!(
            parse_byte_range(Some("bytes=9000-50000"), total),
            ByteRangeOutcome::Partial { start: 9000, end: 9999 }
        );
        // Single-byte range at final byte
        assert_eq!(
            parse_byte_range(Some("bytes=9999-9999"), total),
            ByteRangeOutcome::Partial { start: 9999, end: 9999 }
        );
        // Out-of-range / inverted / malformed -> 416 Unsatisfiable
        assert_eq!(parse_byte_range(Some("bytes=10000-"), total), ByteRangeOutcome::Unsatisfiable);
        assert_eq!(parse_byte_range(Some("bytes=10000-10500"), total), ByteRangeOutcome::Unsatisfiable);
        assert_eq!(parse_byte_range(Some("bytes=500-200"), total), ByteRangeOutcome::Unsatisfiable);
        assert_eq!(parse_byte_range(Some("bytes=-0"), total), ByteRangeOutcome::Unsatisfiable);
        assert_eq!(parse_byte_range(Some("bytes=abc-def"), total), ByteRangeOutcome::Unsatisfiable);
        assert_eq!(parse_byte_range(Some("bytes=0-0"), 0), ByteRangeOutcome::Unsatisfiable);

        // Strict integer media_id validation rejects traversal & non-digits
        assert_eq!(parse_strict_media_id("42"), Some(42));
        assert_eq!(parse_strict_media_id("0"), None);
        assert_eq!(parse_strict_media_id("-1"), None);
        assert_eq!(parse_strict_media_id("../1"), None);
        assert_eq!(parse_strict_media_id("1/../../etc"), None);
        assert_eq!(parse_strict_media_id("12abc"), None);
    }

    #[tokio::test]
    async fn video_streaming_endpoint_end_to_end() {
        use std::sync::{Arc, Mutex};

        let tmp_root = std::env::temp_dir().join(format!("kid_finder_vid_stream_{}", uuid::Uuid::new_v4()));
        let thumbs_dir = tmp_root.join("thumbs");
        let ext_wx_dir = tmp_root.join("xwechat_files").join("msg").join("video");
        std::fs::create_dir_all(&thumbs_dir).unwrap();
        std::fs::create_dir_all(&ext_wx_dir).unwrap();

        // 150 KB deterministic payload (> 2 * 64KB chunk size to exercise chunked streaming)
        let orig_bytes: Vec<u8> = (0..153_600usize).map(|i| ((i * 31 + 7) % 251) as u8).collect();
        let orig_vid_path = ext_wx_dir.join("sample_hevc.mp4");
        std::fs::write(&orig_vid_path, &orig_bytes).unwrap();

        let img_path = tmp_root.join("photo.jpg");
        std::fs::write(&img_path, b"not-a-video").unwrap();

        let conn = db::open(&tmp_root.join("kid-finder.db")).unwrap();
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('sha_v','video',?1,'sample.mp4','wechat-local','2026-11-21 10:00:00',1)",
            params![orig_vid_path.to_string_lossy()],
        )
        .unwrap();
        let vid_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('sha_i','image',?1,'photo.jpg','local','2026-11-21 10:00:00',1)",
            params![img_path.to_string_lossy()],
        )
        .unwrap();
        let img_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('sha_p','video',?1,'purged.mp4','local','2026-11-21 10:00:00',?2)",
            params![orig_vid_path.to_string_lossy(), storage::STATUS_PURGED],
        )
        .unwrap();
        let purged_id = conn.last_insert_rowid();

        let db_arc = Arc::new(Mutex::new(conn));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let thumbs_srv = thumbs_dir.clone();
        let db_srv = db_arc.clone();

        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let db_c = db_srv.clone();
                let thumbs_c = thumbs_srv.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(2048);
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        let Ok(n) = stream.read(&mut tmp).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = find_header_end(&buf) {
                            break pos;
                        }
                    };
                    let head = std::str::from_utf8(&buf[..header_end]).unwrap();
                    let mut lines = head.split("\r\n");
                    let first = lines.next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let raw_path = parts.next().unwrap_or("/");
                    let (path, query) = raw_path.split_once('?').unwrap_or((raw_path, ""));
                    let mut origin = None;
                    let mut range_hdr = None;
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            let kl = k.trim().to_ascii_lowercase();
                            if kl == "origin" {
                                origin = Some(v.trim().to_string());
                            } else if kl == "range" {
                                range_hdr = Some(v.trim().to_string());
                            }
                        }
                    }
                    let cors = allowed_origin(origin.as_deref());
                    let id_seg = path.strip_prefix("/api/video/").unwrap_or("");
                    let _ = serve_video_with_lookup(
                        &mut stream,
                        method == "HEAD",
                        id_seg,
                        query,
                        range_hdr.as_deref(),
                        cors,
                        move |mid, want_prev| {
                            let c = db_c.lock().unwrap();
                            resolve_video_file(&c, &thumbs_c, mid, want_prev)
                        },
                    )
                    .await;
                });
            }
        });

        let send_raw = |req: String| async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(req.as_bytes()).await.unwrap();
            let mut resp = Vec::new();
            client.read_to_end(&mut resp).await.unwrap();
            let sep = find_header_end(&resp).expect("HTTP header separator");
            let head_str = String::from_utf8_lossy(&resp[..sep]).to_string();
            let body = resp[sep + 4..].to_vec();
            (head_str, body)
        };

        // 1) Full GET without Range -> 200 OK + Accept-Ranges: bytes + full 150KB body
        let (h200, b200) = send_raw(format!(
            "GET /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://tauri.localhost\r\n\r\n"
        ))
        .await;
        assert!(h200.starts_with("HTTP/1.1 200 OK\r\n"), "{h200}");
        assert!(h200.contains("Content-Type: video/mp4\r\n"));
        assert!(h200.contains("Accept-Ranges: bytes\r\n"));
        assert!(h200.contains("Content-Length: 153600\r\n"));
        assert!(h200.contains("Access-Control-Allow-Origin: http://tauri.localhost\r\n"));
        assert_eq!(b200, orig_bytes);

        // 2) Bounded Range `bytes=0-1023` -> 206 Partial Content
        let (h206_a, b206_a) = send_raw(format!(
            "GET /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=0-1023\r\n\r\n"
        ))
        .await;
        assert!(h206_a.starts_with("HTTP/1.1 206 Partial Content\r\n"), "{h206_a}");
        assert!(h206_a.contains("Content-Range: bytes 0-1023/153600\r\n"));
        assert!(h206_a.contains("Content-Length: 1024\r\n"));
        assert_eq!(b206_a, orig_bytes[0..1024]);

        // 3) Open-ended Range `bytes=500-` -> 206 Partial Content across multiple 64KB chunks
        let (h206_b, b206_b) = send_raw(format!(
            "GET /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=500-\r\n\r\n"
        ))
        .await;
        assert!(h206_b.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(h206_b.contains("Content-Range: bytes 500-153599/153600\r\n"));
        assert!(h206_b.contains("Content-Length: 153100\r\n"));
        assert_eq!(b206_b, orig_bytes[500..]);

        // 4) Suffix Range `bytes=-256` -> 206 Partial Content tail bytes
        let (h206_c, b206_c) = send_raw(format!(
            "GET /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=-256\r\n\r\n"
        ))
        .await;
        assert!(h206_c.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(h206_c.contains("Content-Range: bytes 153344-153599/153600\r\n"));
        assert!(h206_c.contains("Content-Length: 256\r\n"));
        assert_eq!(b206_c, orig_bytes[153_344..]);

        // 5) Out-of-range `bytes=200000-200500` -> 416 Range Not Satisfiable
        let (h416, b416) = send_raw(format!(
            "GET /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=200000-200500\r\n\r\n"
        ))
        .await;
        assert!(h416.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"), "{h416}");
        assert!(h416.contains("Content-Range: bytes */153600\r\n"));
        assert!(b416.is_empty());

        // 6) HEAD request -> headers match 206, 0 body bytes
        let (h_head, b_head) = send_raw(format!(
            "HEAD /api/video/{vid_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=0-1023\r\n\r\n"
        ))
        .await;
        assert!(h_head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(h_head.contains("Content-Length: 1024\r\n"));
        assert!(b_head.is_empty());

        // 7) Preview proxy resolution: 404 before proxy exists, 200/206 once `thumbs/preview_<id>.mp4` exists
        let (h_no_prev, _) = send_raw(format!(
            "GET /api/video/{vid_id}?preview=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ))
        .await;
        assert!(h_no_prev.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let preview_bytes: Vec<u8> = (0..4096usize).map(|i| (i % 255) as u8).collect();
        std::fs::write(thumbs_dir.join(format!("preview_{vid_id}.mp4")), &preview_bytes).unwrap();

        let (h_prev, b_prev) = send_raw(format!(
            "GET /api/video/{vid_id}?preview=1&t=123 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=100-299\r\n\r\n"
        ))
        .await;
        assert!(h_prev.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(h_prev.contains("Content-Range: bytes 100-299/4096\r\n"));
        assert_eq!(b_prev, preview_bytes[100..300]);

        // 8) Security boundaries: image row -> 404, purged row -> 404, path traversal -> 400
        let (h_img, _) = send_raw(format!("GET /api/video/{img_id} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")).await;
        assert!(h_img.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let (h_purged, _) = send_raw(format!("GET /api/video/{purged_id} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")).await;
        assert!(h_purged.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let (h_trav, _) = send_raw("GET /api/video/../1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_string()).await;
        assert!(h_trav.starts_with("HTTP/1.1 400 Bad Request\r\n"));

        srv.abort();
        drop(db_arc);
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn persistent_web_scan_cache_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_db_test_{}.db", uuid::Uuid::new_v4()));
        let mut conn = db::open(&tmp).unwrap();
        let id_with_both = "https://releasec.qxc.hi.cn/medias/g1/p1_s.jpg";
        let id_no_face = "https://releasec.qxc.hi.cn/medias/g1/p2_s.jpg";

        assert!(load_db_scan(&conn, id_with_both).unwrap().is_none());
        let mut entries = HashMap::new();
        entries.insert(
            id_with_both.to_string(),
            CachedScan {
                faces: vec![([0.1, 0.2, 0.3, 0.4], vec![0.6, 0.8])],
                ocr: vec![([0.5, 0.7, 0.9, 0.85], "大二班 韩梓淏".into(), "大二班韩梓淏".into(), 0.96)],
                ocr_done: true,
            },
        );
        entries.insert(
            id_no_face.to_string(),
            CachedScan {
                faces: vec![],
                ocr: vec![],
                ocr_done: true,
            },
        );
        save_db_scans(&mut conn, &entries).unwrap();

        let loaded = load_db_scan(&conn, id_with_both).unwrap().expect("should exist");
        assert!(loaded.ocr_done);
        assert_eq!(loaded.faces.len(), 1);
        assert_eq!(loaded.faces[0].0, [0.1, 0.2, 0.3, 0.4]);
        let (sim, bbox) = score_faces(&loaded.faces, &[vec![0.6, 0.8]]);
        assert!((sim - 1.0).abs() < 1e-5);
        assert_eq!(bbox, Some([0.1, 0.2, 0.3, 0.4]));

        let kws = ocr::build_person_keywords("韩梓淏", "梓淏");
        let (kw, text, obbox) = score_ocr(&loaded.ocr, &kws).expect("should match OCR");
        assert_eq!(kw, "韩梓淏");
        assert_eq!(text, "大二班 韩梓淏");
        assert_eq!(obbox, [0.5, 0.7, 0.9, 0.85]);

        let empty = load_db_scan(&conn, id_no_face).unwrap().expect("0-face scan should still be cached");
        assert!(empty.faces.is_empty());
        assert!(empty.ocr.is_empty());

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn pad_segments_merges_overlaps_and_clamps() {
        let merged = media::pad_segments(&[[1.0, 3.0], [5.0, 8.0], [20.0, 22.0]], 2.0, 2.0, Some(22.0));
        assert_eq!(merged, vec![[0.0, 10.0], [18.0, 23.0]]);
    }

    #[test]
    fn refs_signature_and_rescoring_track_ref_alias_and_rejection_changes() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_sig_test_{}.db", uuid::Uuid::new_v4()));
        let conn = db::open(&tmp).unwrap();

        let sig_empty = compute_refs_signature(&conn).unwrap();
        assert_eq!(sig_empty.len(), 64);
        assert_eq!(sig_empty, compute_refs_signature(&conn).unwrap());

        conn.execute("INSERT INTO persons(name, created_at) VALUES('朵朵', '2026-01-01 00:00:00')", [])
            .unwrap();
        let pid = conn.last_insert_rowid();
        let sig_person = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_person, sig_empty);

        // Updating aliases must also change refsSignature so Chrome extension re-evaluates OCR matches!
        conn.execute("UPDATE persons SET aliases='小朵朵,朵宝' WHERE id=?1", [pid]).unwrap();
        let sig_aliases = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_aliases, sig_person);

        conn.execute("UPDATE persons SET name='小朵朵', aliases='' WHERE id=?1", [pid]).unwrap();
        let sig_renamed = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_renamed, sig_aliases);

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
        let _fid_frame1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,12.0,0.1,0.1,0.3,0.3,0.9,?2)",
            params![mid, db::emb_to_blob(&[0.6f32, 0.8f32])],
        )
        .unwrap();
        let fid_frame2 = conn.last_insert_rowid();

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
        assert_eq!(vs1.segments, vec![[2.0, 3.0]]);

        conn.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,NULL,'test','2026-01-01 00:00:02')",
            params![pid, db::emb_to_blob(&[0.6f32, 0.8f32])],
        )
        .unwrap();
        let ref2_id = conn.last_insert_rowid();
        let sig_ref2 = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_ref2, sig_ref1);

        let refs_v2 = media::person_refs(&conn, pid).unwrap();
        let vs2 = score_analyzed_video(&conn, mid, Some(pid), &refs_v2, 0.75).unwrap();
        assert_eq!(vs2.segments, vec![[2.0, 3.0], [12.0, 13.0]]);

        conn.execute("INSERT INTO rejections(face_id, person_id) VALUES(?1, ?2)", params![fid_frame2, pid])
            .unwrap();
        let sig_rej = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_rej, sig_ref2);

        // OCR rejection must also update refsSignature
        conn.execute("INSERT INTO ocr_rejections(ocr_id, person_id) VALUES(99, ?1)", [pid]).unwrap();
        let sig_ocr_rej = compute_refs_signature(&conn).unwrap();
        assert_ne!(sig_ocr_rej, sig_rej);

        conn.execute("DELETE FROM refs WHERE id=?1", [ref2_id]).unwrap();
        conn.execute("DELETE FROM refs WHERE id=?1", [ref1_id]).unwrap();
        conn.execute("DELETE FROM rejections WHERE person_id=?1", [pid]).unwrap();
        conn.execute("DELETE FROM ocr_rejections WHERE person_id=?1", [pid]).unwrap();
        conn.execute("DELETE FROM persons WHERE id=?1", [pid]).unwrap();
        assert_eq!(compute_refs_signature(&conn).unwrap(), sig_empty);

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }
}
