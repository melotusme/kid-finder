//! Local loopback-only HTTP bridge (127.0.0.1:27431) for the Chrome extension on qxc.hi.cn.
//! Reuses the desktop app's native ONNX Runtime FaceEngine and enrolled baby embeddings.
use crate::{db, download, face, media, AppState};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::stream::{self, StreamExt};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const BRIDGE_PORT: u16 = 27431;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 4000;

/// Cached per-URL faces: list of (normalized [x1,y1,x2,y2], 512-d L2-normalized embedding).
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
        ("POST", "/api/match-urls") => match serde_json::from_slice::<MatchReq>(&body) {
            Ok(req) => match api_match_urls(&app, req).await {
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

// ---------------- endpoints ----------------

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
        let c = st.db.lock().unwrap();
        let thresh: f32 = db::get_setting(&c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40);
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
            "threshold": thresh,
            "persons": persons,
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MatchResult {
    id: String,
    url: String,
    max_score: f32,
    face_count: usize,
    best_bbox: Option<[f32; 4]>,
    error: Option<String>,
}

fn is_allowed_image_url(url: &str) -> bool {
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
    // Load person refs & ensure engine is ready.
    let app_init = app.clone();
    let pid = req.person_id;
    let refs: Vec<Vec<f32>> = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
        let st = app_init.state::<AppState>();
        media::ensure_engine(&st)?;
        let c = st.db.lock().unwrap();
        let refs = media::person_refs(&c, pid)?;
        if refs.is_empty() {
            bail!("该宝宝还没有录入参考照");
        }
        Ok(refs)
    })
    .await??;

    // Split into cached vs uncached items.
    let mut cached_map: HashMap<String, CachedFaces> = HashMap::new();
    let mut to_fetch: Vec<MatchItem> = vec![];
    {
        let st = app.state::<AppState>();
        let cache = st.web_cache.lock().unwrap();
        for it in &req.items {
            if let Some(faces) = cache.get(&it.url) {
                cached_map.insert(it.url.clone(), faces.clone());
            } else if is_allowed_image_url(&it.url) {
                to_fetch.push(MatchItem { id: it.id.clone(), url: it.url.clone() });
            }
        }
    }

    // Download uncached thumbnails concurrently (up to 6 at once).
    let http = reqwest::Client::builder()
        .user_agent("kid-finder")
        .timeout(Duration::from_secs(20))
        .build()?;
    let downloaded: Vec<(String, Result<Vec<u8>>)> = stream::iter(to_fetch)
        .map(|it| {
            let http = http.clone();
            async move {
                let r = async {
                    let resp = http.get(&it.url).send().await?.error_for_status()?;
                    let bytes = resp.bytes().await?;
                    if bytes.len() > 15 * 1024 * 1024 {
                        bail!("图片过大");
                    }
                    Ok(bytes.to_vec())
                }
                .await;
                (it.url, r)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;

    // Run ONNX inference on downloaded images and populate cache.
    let app_inf = app.clone();
    let (new_faces, errors): (HashMap<String, CachedFaces>, HashMap<String, String>) =
        tauri::async_runtime::spawn_blocking(move || {
            let st = app_inf.state::<AppState>();
            let mut out_faces: HashMap<String, CachedFaces> = HashMap::new();
            let mut out_err: HashMap<String, String> = HashMap::new();
            for (url, bytes_res) in downloaded {
                match bytes_res {
                    Err(e) => {
                        out_err.insert(url, format!("{e:#}"));
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
                                out_faces.insert(url, f);
                            }
                            Err(e) => {
                                out_err.insert(url, format!("{e:#}"));
                            }
                        }
                    }
                }
            }
            if !out_faces.is_empty() {
                let mut cache = st.web_cache.lock().unwrap();
                if cache.len() + out_faces.len() > MAX_CACHE_ENTRIES {
                    cache.clear();
                }
                for (k, v) in &out_faces {
                    cache.insert(k.clone(), v.clone());
                }
            }
            (out_faces, out_err)
        })
        .await?;

    for (k, v) in new_faces {
        cached_map.insert(k, v);
    }

    // Score every requested item against the selected baby's reference embeddings.
    let mut results = Vec::with_capacity(req.items.len());
    for it in req.items {
        if !is_allowed_image_url(&it.url) {
            results.push(MatchResult {
                id: it.id,
                url: it.url,
                max_score: 0.0,
                face_count: 0,
                best_bbox: None,
                error: Some("不支持的图片来源".into()),
            });
            continue;
        }
        if let Some(err) = errors.get(&it.url) {
            results.push(MatchResult {
                id: it.id,
                url: it.url,
                max_score: 0.0,
                face_count: 0,
                best_bbox: None,
                error: Some(err.clone()),
            });
            continue;
        }
        let faces = cached_map.get(&it.url);
        let mut max_score = 0.0f32;
        let mut best_bbox: Option<[f32; 4]> = None;
        let face_count = faces.map(|f| f.len()).unwrap_or(0);
        if let Some(fs) = faces {
            for (bbox, emb) in fs {
                let sim = refs.iter().map(|q| face::cosine(q, emb)).fold(f32::MIN, f32::max);
                if sim > max_score {
                    max_score = sim;
                    best_bbox = Some(*bbox);
                }
            }
        }
        results.push(MatchResult {
            id: it.id,
            url: it.url,
            max_score,
            face_count,
            best_bbox,
            error: None,
        });
    }

    Ok(json!({ "ok": true, "results": results }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_allowlist_checks_scheme_and_host() {
        assert!(is_allowed_image_url("https://releasec.qxc.hi.cn/medias/abc/1.jpg?imageMogr2/thumbnail/960x/format/jpg"));
        assert!(is_allowed_image_url("https://foo.tcb.qcloud.la/a.jpg"));
        assert!(is_allowed_image_url("https://bucket.cos.ap-shanghai.myqcloud.com/a.jpg"));
        assert!(!is_allowed_image_url("http://releasec.qxc.hi.cn/medias/abc/1.jpg"));
        assert!(!is_allowed_image_url("http://127.0.0.1:27431/api/status"));
        assert!(!is_allowed_image_url("https://evil.example.com/qxc.hi.cn.jpg"));
        assert!(!is_allowed_image_url("file:///etc/passwd"));
    }

    #[test]
    fn cors_origin_allowlist() {
        assert_eq!(allowed_origin(Some("chrome-extension://abcdefghijklmnop")), Some("chrome-extension://abcdefghijklmnop"));
        assert_eq!(allowed_origin(Some("https://qxc.hi.cn")), Some("https://qxc.hi.cn"));
        assert_eq!(allowed_origin(Some("https://evil.com")), None);
        assert_eq!(allowed_origin(None), None);
    }
}
