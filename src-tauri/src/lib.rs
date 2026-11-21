mod bridge;
mod db;
mod download;
mod extension;
mod face;
mod media;
pub mod ocr;
mod qxc;
pub mod storage;
pub mod wechat;

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

#[derive(Clone, Debug)]
pub struct Paths {
    /// Fixed `<appDataDir>`: hosts `kid-finder.db`, `storage.json`, `models/`, `bin/`, `chrome-extension/`.
    pub data: PathBuf,
    /// Configurable storage root (defaults to `data`, or custom drive via `storage.json`): hosts `library/`, `thumbs/`, `tmp/`.
    pub storage_root: PathBuf,
    pub models: PathBuf,
    pub library: PathBuf,
    pub thumbs: PathBuf,
    pub bin: PathBuf,
    pub tmp: PathBuf,
}

impl Paths {
    pub fn new(data: PathBuf) -> Result<Self> {
        Self::with_storage_root(data.clone(), data)
    }

    pub fn with_storage_root(data: PathBuf, storage_root: PathBuf) -> Result<Self> {
        let p = Self {
            models: data.join("models"),
            bin: data.join("bin"),
            library: storage_root.join("library"),
            thumbs: storage_root.join("thumbs"),
            tmp: storage_root.join("tmp"),
            data,
            storage_root,
        };
        for d in [&p.data, &p.storage_root, &p.models, &p.library, &p.thumbs, &p.bin, &p.tmp] {
            std::fs::create_dir_all(d)?;
        }
        Ok(p)
    }
}

pub struct AppState {
    pub app_data_dir: PathBuf,
    pub paths: RwLock<Paths>,
    pub db: Mutex<Connection>,
    pub engine: Mutex<Option<face::FaceEngine>>,
    pub ocr_engine: Mutex<Option<ocr::OcrEngine>>,
    pub processing: AtomicBool,
    pub ocr_backfilling: AtomicBool,
    pub bridge_running: AtomicBool,
    pub web_cache: Mutex<HashMap<String, bridge::CachedScan>>,
}

impl AppState {
    pub fn paths(&self) -> Paths {
        self.paths.read().unwrap().clone()
    }
}

type CmdResult<T> = std::result::Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?.map_err(err)
}

// ---------------- status / settings ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    app_version: String,
    extension_version: String,
    models_ready: bool,
    ocr_models_ready: bool,
    ffmpeg: Option<String>,
    processing: bool,
    total: i64,
    pending: i64,
    failed: i64,
    images: i64,
    videos: i64,
    qxc_user: Option<String>,
    threshold: f32,
    data_dir: String,
    storage_root: String,
    wechat_watch_dir: Option<String>,
    wechat_watch_autoclean: bool,
    bridge_running: bool,
    bridge_port: u16,
    extension_dir: String,
    web_scanned: i64,
    refs_signature: String,
}

#[tauri::command]
async fn get_status(app: AppHandle) -> CmdResult<Status> {
    blocking(move || {
        let st = app.state::<AppState>();
        let paths = st.paths();
        let ffmpeg = download::find_ffmpeg(&st).map(|p| p.to_string_lossy().to_string());
        let qxc_user = qxc::saved_token(&st).map(|t| t.1);
        let c = st.db.lock().unwrap();
        let n = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        let purged = storage::STATUS_PURGED;
        let active_images: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE kind='image' AND status != ?1", [purged], |r| r.get(0))?;
        let active_videos: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE kind='video' AND status != ?1", [purged], |r| r.get(0))?;
        let active_total: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE status != ?1", [purged], |r| r.get(0))?;
        let wechat_watch_dir = db::get_setting(&c, "wechat_watch_dir").filter(|s| !s.trim().is_empty());
        let wechat_watch_autoclean = db::get_setting(&c, "wechat_watch_autoclean").as_deref() == Some("1");
        Ok(Status {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            extension_version: extension::bundled_extension_version().to_string(),
            models_ready: download::models_ready(&paths),
            ocr_models_ready: download::ocr_models_ready(&paths),
            ffmpeg,
            processing: st.processing.load(Ordering::SeqCst),
            total: active_total,
            pending: n("SELECT COUNT(*) FROM media WHERE status=0")?,
            failed: n("SELECT COUNT(*) FROM media WHERE status=2")?,
            images: active_images,
            videos: active_videos,
            qxc_user,
            threshold: threshold(&c),
            data_dir: paths.data.to_string_lossy().to_string(),
            storage_root: paths.storage_root.to_string_lossy().to_string(),
            wechat_watch_dir,
            wechat_watch_autoclean,
            bridge_running: st.bridge_running.load(Ordering::SeqCst),
            bridge_port: bridge::BRIDGE_PORT,
            extension_dir: extension::extension_dir(&paths).to_string_lossy().to_string(),
            web_scanned: n("SELECT COUNT(*) FROM web_scans")?,
            refs_signature: bridge::compute_refs_signature(&c)?,
        })
    })
    .await
}

fn threshold(c: &Connection) -> f32 {
    db::get_setting(c, "threshold").and_then(|v| v.parse().ok()).unwrap_or(0.40)
}

#[tauri::command]
async fn set_threshold(app: AppHandle, value: f32) -> CmdResult<()> {
    blocking(move || db::set_setting(&app.state::<AppState>().db.lock().unwrap(), "threshold", &value.to_string())).await
}

#[tauri::command]
async fn download_models(app: AppHandle) -> CmdResult<()> {
    let paths = app.state::<AppState>().paths();
    download::download_models(&app, &paths).await.map_err(err)?;
    media::spawn_ocr_backfill(app.clone());
    media::spawn_processing(app);
    Ok(())
}

#[tauri::command]
async fn install_ffmpeg(app: AppHandle) -> CmdResult<String> {
    let paths = app.state::<AppState>().paths();
    download::install_ffmpeg(&app, &paths).await.map(|p| p.to_string_lossy().to_string()).map_err(err)
}

// ---------------- persons ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefInfo {
    id: i64,
    thumb: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Person {
    id: i64,
    name: String,
    aliases: String,
    keywords: Vec<String>,
    refs: Vec<RefInfo>,
}

#[tauri::command]
fn default_aliases(name: String) -> String {
    ocr::default_aliases_for_name(&name)
}

#[tauri::command]
async fn list_persons(app: AppHandle) -> CmdResult<Vec<Person>> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        let mut ps: Vec<Person> = c
            .prepare("SELECT id, name, aliases FROM persons ORDER BY id")?
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                let name: String = r.get(1)?;
                let aliases: String = r.get(2)?;
                let keywords = ocr::build_person_keywords(&name, &aliases);
                Ok(Person { id, name, aliases, keywords, refs: vec![] })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut s = c.prepare("SELECT id, thumb FROM refs WHERE person_id=?1 ORDER BY id")?;
        for p in ps.iter_mut() {
            p.refs = s.query_map([p.id], |r| Ok(RefInfo { id: r.get(0)?, thumb: r.get(1)? }))?.collect::<rusqlite::Result<_>>()?;
        }
        Ok(ps)
    })
    .await
}

#[tauri::command]
async fn add_person(app: AppHandle, name: String, aliases: Option<String>) -> CmdResult<i64> {
    let a = app.clone();
    let id = blocking(move || {
        let st = a.state::<AppState>();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("名字不能为空");
        }
        let final_aliases = match aliases {
            Some(s) => s.trim().to_string(),
            None => ocr::default_aliases_for_name(trimmed),
        };
        let c = st.db.lock().unwrap();
        c.execute(
            "INSERT INTO persons(name, aliases, created_at) VALUES(?1, ?2, ?3)",
            params![trimmed, final_aliases, db::now()],
        )?;
        Ok(c.last_insert_rowid())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(id)
}

#[tauri::command]
async fn rename_person(app: AppHandle, id: i64, name: String) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("名字不能为空");
        }
        let st = a.state::<AppState>();
        let c = st.db.lock().unwrap();
        let cur_aliases: String = c.query_row("SELECT aliases FROM persons WHERE id=?1", [id], |r| r.get(0))?;
        let next_aliases = if cur_aliases.trim().is_empty() {
            ocr::default_aliases_for_name(trimmed)
        } else {
            cur_aliases
        };
        c.execute("UPDATE persons SET name=?2, aliases=?3 WHERE id=?1", params![id, trimmed, next_aliases])?;
        Ok(())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(())
}

#[tauri::command]
async fn update_person_profile(app: AppHandle, id: i64, name: String, aliases: String) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("名字不能为空");
        }
        a.state::<AppState>().db.lock().unwrap().execute(
            "UPDATE persons SET name=?2, aliases=?3 WHERE id=?1",
            params![id, trimmed, aliases.trim()],
        )?;
        Ok(())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(())
}

#[tauri::command]
async fn delete_person(app: AppHandle, id: i64) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        let st = a.state::<AppState>();
        let c = st.db.lock().unwrap();
        if let Ok(mut s) = c.prepare("SELECT thumb FROM refs WHERE person_id=?1") {
            if let Ok(rows) = s.query_map([id], |r| r.get::<_, Option<String>>(0)) {
                for t in rows.flatten().flatten() {
                    let _ = std::fs::remove_file(t);
                }
            }
        }
        c.execute("DELETE FROM rejections WHERE person_id=?1", [id])?;
        c.execute("DELETE FROM ocr_rejections WHERE person_id=?1", [id])?;
        c.execute("DELETE FROM persons WHERE id=?1", [id])?;
        Ok(())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(())
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RefReport {
    added: usize,
    no_face: Vec<String>,
    multi_face: Vec<String>,
    errors: Vec<String>,
}

/// Enrol reference photos. When a photo has several faces the largest one is used.
#[tauri::command]
async fn add_refs(app: AppHandle, person_id: i64, paths: Vec<String>) -> CmdResult<RefReport> {
    let a = app.clone();
    let rep = blocking(move || {
        let st = a.state::<AppState>();
        media::ensure_engine(&st)?;
        let thumbs_dir = st.paths().thumbs;
        let mut rep = RefReport::default();
        for p in paths {
            let pb = PathBuf::from(&p);
            let name = pb.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(p.clone());
            let r: Result<()> = (|| {
                let (img, faces) = media::analyze_file(&st, &pb)?;
                let area = |f: &face::Face| (f.bbox[2] - f.bbox[0]) * (f.bbox[3] - f.bbox[1]);
                let Some(best) = faces.iter().max_by(|a, b| area(a).total_cmp(&area(b))) else {
                    rep.no_face.push(name.clone());
                    return Ok(());
                };
                if faces.len() > 1 {
                    rep.multi_face.push(name.clone());
                }
                let thumb = thumbs_dir.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
                face::face_thumb(&img, &best.bbox, 160).save(&thumb)?;
                st.db.lock().unwrap().execute(
                    "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,?3,?4,?5)",
                    params![person_id, db::emb_to_blob(&best.embedding), thumb.to_string_lossy(), p, db::now()],
                )?;
                rep.added += 1;
                Ok(())
            })();
            if let Err(e) = r {
                rep.errors.push(format!("{name}: {e:#}"));
            }
        }
        Ok(rep)
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(rep)
}

#[tauri::command]
async fn delete_ref(app: AppHandle, id: i64) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        let st = a.state::<AppState>();
        let c = st.db.lock().unwrap();
        if let Ok(Some(t)) = c.query_row("SELECT thumb FROM refs WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)) {
            let _ = std::fs::remove_file(t);
        }
        c.execute("DELETE FROM refs WHERE id=?1", [id])?;
        Ok(())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(())
}

// ---------------- import / WeChat / analysis ----------------

#[tauri::command]
async fn import_paths(app: AppHandle, paths: Vec<String>) -> CmdResult<media::ImportReport> {
    let a = app.clone();
    let rep = blocking(move || media::import_paths(&a, paths)).await?;
    media::spawn_processing(app);
    Ok(rep)
}

#[tauri::command]
async fn discover_wechat() -> CmdResult<Vec<wechat::WechatSource>> {
    blocking(|| Ok(wechat::discover_wechat_sources())).await
}

#[tauri::command]
async fn import_wechat_videos(app: AppHandle) -> CmdResult<media::ImportReport> {
    let a = app.clone();
    let rep = blocking(move || media::import_wechat_videos(&a)).await?;
    if rep.added > 0 {
        media::spawn_processing(app);
    }
    Ok(rep)
}

#[tauri::command]
async fn set_wechat_watch(app: AppHandle, dir: Option<String>, autoclean: Option<bool>) -> CmdResult<()> {
    let a = app.clone();
    let should_poll = blocking(move || {
        let st = a.state::<AppState>();
        let paths = st.paths();
        let c = st.db.lock().unwrap();
        let mut active_dir = false;
        if let Some(raw) = dir {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                db::del_setting(&c, "wechat_watch_dir")?;
            } else {
                let p = PathBuf::from(trimmed);
                if media::is_wechat_internal_path(&p) {
                    bail!("请勿直接监听微信内部缓存目录（xwechat_files / WeChat Files）；请新建一个独立的「微信导出中转」空文件夹");
                }
                if storage::is_path_inside(&p, &paths.storage_root)
                    || storage::is_path_inside(&paths.storage_root, &p)
                    || storage::is_path_inside(&p, &st.app_data_dir)
                {
                    bail!("中转监听文件夹不能与找娃助手自身的数据/存储目录重叠");
                }
                std::fs::create_dir_all(&p)?;
                db::set_setting(&c, "wechat_watch_dir", trimmed)?;
                active_dir = true;
            }
        } else {
            active_dir = db::get_setting(&c, "wechat_watch_dir").filter(|s| !s.trim().is_empty()).is_some();
        }
        if let Some(clean) = autoclean {
            db::set_setting(&c, "wechat_watch_autoclean", if clean { "1" } else { "0" })?;
        }
        Ok(active_dir)
    })
    .await?;

    if should_poll {
        let a2 = app.clone();
        let added = blocking(move || media::poll_wechat_watch_once(&a2.state::<AppState>())).await?;
        if added > 0 {
            let _ = app.emit("watch-imported", added);
            media::spawn_processing(app);
        }
    }
    Ok(())
}

#[tauri::command]
fn start_processing(app: AppHandle) {
    media::spawn_processing(app);
}

#[tauri::command]
async fn retry_failed(app: AppHandle) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        a.state::<AppState>().db.lock().unwrap().execute("UPDATE media SET status=0 WHERE status=2", [])?;
        Ok(())
    })
    .await?;
    media::spawn_processing(app);
    Ok(())
}

// ---------------- search / feedback / export ----------------

#[tauri::command]
async fn search(app: AppHandle, person_id: i64, threshold: f32) -> CmdResult<Vec<media::Hit>> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        media::search(&c, person_id, threshold)
    })
    .await
}

/// "Yes, that's my kid": add this detected face as an extra reference (improves recall).
#[tauri::command]
async fn confirm_face(app: AppHandle, face_id: i64, person_id: i64) -> CmdResult<()> {
    let a = app.clone();
    blocking(move || {
        let st = a.state::<AppState>();
        let thumbs_dir = st.paths().thumbs;
        let c = st.db.lock().unwrap();
        let (emb, path, kind, b): (Vec<u8>, String, String, [f32; 4]) = c.query_row(
            "SELECT f.embedding, m.path, m.kind, f.x1, f.y1, f.x2, f.y2 FROM faces f JOIN media m ON m.id=f.media_id WHERE f.id=?1",
            [face_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?])),
        )?;
        let mut thumb: Option<String> = None;
        if kind == "image" {
            if let Ok(img) = face::load_image(Path::new(&path)) {
                let (w, h) = (img.width() as f32, img.height() as f32);
                let t = thumbs_dir.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
                if face::face_thumb(&img, &[b[0] * w, b[1] * h, b[2] * w, b[3] * h], 160).save(&t).is_ok() {
                    thumb = Some(t.to_string_lossy().to_string());
                }
            }
        }
        c.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1,?2,?3,?4,?5)",
            params![person_id, emb, thumb, format!("face:{face_id}"), db::now()],
        )?;
        c.execute("DELETE FROM rejections WHERE face_id=?1 AND person_id=?2", params![face_id, person_id])?;
        Ok(())
    })
    .await?;
    let _ = app.emit("persons-changed", ());
    Ok(())
}

/// Backwards-compatible face-only rejection command.
#[tauri::command]
async fn reject_face(app: AppHandle, face_id: i64, person_id: i64, threshold: Option<f32>) -> CmdResult<Option<media::Hit>> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        let thresh = threshold.unwrap_or_else(|| crate::threshold(&c));
        media::reject_and_rescore_media(&c, Some(face_id), None, person_id, thresh)
    })
    .await
}

/// Surgical rejection supporting `"face"`, `"ocr"`, or `"both"` hits on a single media item.
#[tauri::command]
async fn reject_hit(
    app: AppHandle,
    person_id: i64,
    face_id: Option<i64>,
    ocr_id: Option<i64>,
    threshold: Option<f32>,
) -> CmdResult<Option<media::Hit>> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        let thresh = threshold.unwrap_or_else(|| crate::threshold(&c));
        media::reject_and_rescore_media(&c, face_id, ocr_id, person_id, thresh)
    })
    .await
}

#[tauri::command]
async fn export(app: AppHandle, out_dir: String, items: Vec<media::ExportItem>, fade: Option<f64>) -> CmdResult<media::ExportReport> {
    blocking(move || media::export(&app, Path::new(&out_dir), items, fade.unwrap_or(0.0))).await
}

// ---------------- storage stats, migration & space reclamation ----------------

#[tauri::command]
async fn get_storage_stats(app: AppHandle) -> CmdResult<storage::StorageStats> {
    blocking(move || {
        let st = app.state::<AppState>();
        let paths = st.paths();
        let c = st.db.lock().unwrap();
        storage::compute_storage_stats(&st.app_data_dir, &paths.storage_root, &c)
    })
    .await
}

#[tauri::command]
async fn migrate_storage(app: AppHandle, new_root: String) -> CmdResult<storage::MigrateReport> {
    let a = app.clone();
    blocking(move || {
        let st = a.state::<AppState>();
        if st.processing.load(Ordering::SeqCst) || st.ocr_backfilling.load(Ordering::SeqCst) {
            bail!("正在后台分析照片/视频，请等待分析完成后再迁移存储目录");
        }
        let cur_paths = st.paths();
        let target_root = PathBuf::from(new_root.trim());
        if media::is_wechat_internal_path(&target_root) {
            bail!("存储目录不能设置在微信内部缓存目录中");
        }
        let mut c = st.db.lock().unwrap();
        let rep = storage::migrate_storage(
            &st.app_data_dir,
            &mut c,
            &cur_paths.storage_root,
            &target_root,
            |done, total, current| {
                let _ = a.emit(
                    "progress",
                    media::Progress {
                        stage: "export",
                        done,
                        total,
                        current: current.to_string(),
                    },
                );
            },
        )?;
        let updated_paths = Paths::with_storage_root(st.app_data_dir.clone(), target_root)?;
        *st.paths.write().unwrap() = updated_paths;
        Ok(rep)
    })
    .await
}

#[tauri::command]
async fn purge_unmatched(app: AppHandle) -> CmdResult<storage::PurgeReport> {
    blocking(move || {
        let st = app.state::<AppState>();
        if st.processing.load(Ordering::SeqCst) || st.ocr_backfilling.load(Ordering::SeqCst) {
            bail!("正在后台分析照片/视频，请等待分析完成后再清理空间");
        }
        let paths = st.paths();
        // Ensure any un-OCRed images are scanned before deciding what is unmatched if OCR models are ready.
        if download::ocr_models_ready(&paths) {
            let _ = media::backfill_missing_ocr(&st, |_| {});
        }
        let mut c = st.db.lock().unwrap();
        let person_count: i64 = c.query_row("SELECT COUNT(*) FROM persons", [], |r| r.get(0))?;
        if person_count == 0 {
            bail!("请先在「① 录入宝宝」添加宝宝档案后再清理未命中原片");
        }
        let floor = threshold(&c).min(media::PURGE_CONSERVATIVE_FACE_FLOOR);
        let keep_ids = media::collect_protected_media_ids(&c, floor)?;
        storage::purge_unmatched_files(&mut c, &paths.library, &keep_ids)
    })
    .await
}

// ---------------- 群相册 ----------------

#[tauri::command]
async fn qxc_create_login() -> CmdResult<qxc::LoginTicket> {
    qxc::create_ticket().await.map_err(err)
}

#[tauri::command]
async fn qxc_check_login(app: AppHandle, token: String) -> CmdResult<Option<String>> {
    let st = app.state::<AppState>();
    qxc::check_login(&st, &token).await.map_err(err)
}

#[tauri::command]
async fn qxc_logout(app: AppHandle) -> CmdResult<()> {
    qxc::logout(&app.state::<AppState>()).map_err(err)
}

#[tauri::command]
async fn qxc_albums(app: AppHandle) -> CmdResult<Vec<qxc::Album>> {
    let st = app.state::<AppState>();
    qxc::albums(&st).await.map_err(err)
}

#[tauri::command]
async fn qxc_sync(app: AppHandle, group_id: String) -> CmdResult<qxc::SyncReport> {
    let rep = qxc::sync(&app, &group_id).await.map_err(err)?;
    media::spawn_processing(app);
    Ok(rep)
}

// ---------------- Chrome 扩展安装入口 ----------------

#[tauri::command]
async fn install_chrome_extension(app: AppHandle) -> CmdResult<extension::InstallResult> {
    blocking(move || {
        let paths = app.state::<AppState>().paths();
        let dir = extension::extract(&paths)?;
        Ok(extension::open_install_helpers(&dir))
    })
    .await
}

#[tauri::command]
fn open_qxc_web(app: AppHandle) -> CmdResult<()> {
    app.opener().open_url("https://qxc.hi.cn/#/", None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_web_cache(app: AppHandle) -> CmdResult<()> {
    blocking(move || bridge::clear_web_cache_sync(&app.state::<AppState>())).await
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir().map_err(|e| anyhow!(e))?;
            let storage_root = storage::load_storage_root(&app_data_dir);
            let paths = Paths::with_storage_root(app_data_dir.clone(), storage_root)?;
            let conn = db::open(&paths.data.join("kid-finder.db"))?;
            // A crash mid-way may leave partial temp files behind.
            let _ = std::fs::remove_dir_all(&paths.tmp);
            std::fs::create_dir_all(&paths.tmp)?;
            let _ = extension::extract(&paths);
            app.manage(AppState {
                app_data_dir,
                paths: RwLock::new(paths),
                db: Mutex::new(conn),
                engine: Mutex::new(None),
                ocr_engine: Mutex::new(None),
                processing: AtomicBool::new(false),
                ocr_backfilling: AtomicBool::new(false),
                bridge_running: AtomicBool::new(false),
                web_cache: Mutex::new(HashMap::new()),
            });
            bridge::start(app.handle().clone());
            media::spawn_thumb_backfill(app.handle().clone());
            media::spawn_ocr_backfill(app.handle().clone());
            media::spawn_wechat_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_threshold,
            download_models,
            install_ffmpeg,
            default_aliases,
            list_persons,
            add_person,
            rename_person,
            update_person_profile,
            delete_person,
            add_refs,
            delete_ref,
            import_paths,
            discover_wechat,
            import_wechat_videos,
            set_wechat_watch,
            start_processing,
            retry_failed,
            search,
            confirm_face,
            reject_face,
            reject_hit,
            export,
            get_storage_stats,
            migrate_storage,
            purge_unmatched,
            qxc_create_login,
            qxc_check_login,
            qxc_logout,
            qxc_albums,
            qxc_sync,
            install_chrome_extension,
            open_qxc_web,
            clear_web_cache,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
