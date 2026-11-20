mod bridge;
mod db;
mod download;
mod extension;
mod face;
mod media;
mod qxc;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

pub struct Paths {
    pub data: PathBuf,
    pub models: PathBuf,
    pub library: PathBuf,
    pub thumbs: PathBuf,
    pub bin: PathBuf,
    pub tmp: PathBuf,
}

impl Paths {
    fn new(data: PathBuf) -> Result<Self> {
        let p = Self {
            models: data.join("models"),
            library: data.join("library"),
            thumbs: data.join("thumbs"),
            bin: data.join("bin"),
            tmp: data.join("tmp"),
            data,
        };
        for d in [&p.data, &p.models, &p.library, &p.thumbs, &p.bin, &p.tmp] {
            std::fs::create_dir_all(d)?;
        }
        Ok(p)
    }
}

pub struct AppState {
    pub paths: Paths,
    pub db: Mutex<Connection>,
    pub engine: Mutex<Option<face::FaceEngine>>,
    pub processing: AtomicBool,
    pub bridge_running: AtomicBool,
    pub web_cache: Mutex<HashMap<String, bridge::CachedFaces>>,
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
    models_ready: bool,
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
    bridge_running: bool,
    bridge_port: u16,
    extension_dir: String,
}

#[tauri::command]
async fn get_status(app: AppHandle) -> CmdResult<Status> {
    blocking(move || {
        let st = app.state::<AppState>();
        let ffmpeg = download::find_ffmpeg(&st).map(|p| p.to_string_lossy().to_string());
        let qxc_user = qxc::saved_token(&st).map(|t| t.1);
        let c = st.db.lock().unwrap();
        let n = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        Ok(Status {
            models_ready: download::models_ready(&st.paths),
            ffmpeg,
            processing: st.processing.load(Ordering::SeqCst),
            total: n("SELECT COUNT(*) FROM media")?,
            pending: n("SELECT COUNT(*) FROM media WHERE status=0")?,
            failed: n("SELECT COUNT(*) FROM media WHERE status=2")?,
            images: n("SELECT COUNT(*) FROM media WHERE kind='image'")?,
            videos: n("SELECT COUNT(*) FROM media WHERE kind='video'")?,
            qxc_user,
            threshold: threshold(&c),
            data_dir: st.paths.data.to_string_lossy().to_string(),
            bridge_running: st.bridge_running.load(Ordering::SeqCst),
            bridge_port: bridge::BRIDGE_PORT,
            extension_dir: extension::extension_dir(&st.paths).to_string_lossy().to_string(),
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
    let st = app.state::<AppState>();
    download::download_models(&app, &st.paths).await.map_err(err)
}

#[tauri::command]
async fn install_ffmpeg(app: AppHandle) -> CmdResult<String> {
    let st = app.state::<AppState>();
    download::install_ffmpeg(&app, &st.paths).await.map(|p| p.to_string_lossy().to_string()).map_err(err)
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
    refs: Vec<RefInfo>,
}

#[tauri::command]
async fn list_persons(app: AppHandle) -> CmdResult<Vec<Person>> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        let mut ps: Vec<Person> = c
            .prepare("SELECT id, name FROM persons ORDER BY id")?
            .query_map([], |r| Ok(Person { id: r.get(0)?, name: r.get(1)?, refs: vec![] }))?
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
async fn add_person(app: AppHandle, name: String) -> CmdResult<i64> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        c.execute("INSERT INTO persons(name, created_at) VALUES(?1, ?2)", params![name.trim(), db::now()])?;
        Ok(c.last_insert_rowid())
    })
    .await
}

#[tauri::command]
async fn rename_person(app: AppHandle, id: i64, name: String) -> CmdResult<()> {
    blocking(move || {
        app.state::<AppState>().db.lock().unwrap().execute("UPDATE persons SET name=?2 WHERE id=?1", params![id, name.trim()])?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn delete_person(app: AppHandle, id: i64) -> CmdResult<()> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        c.execute("DELETE FROM rejections WHERE person_id=?1", [id])?;
        c.execute("DELETE FROM persons WHERE id=?1", [id])?;
        Ok(())
    })
    .await
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
    blocking(move || {
        let st = app.state::<AppState>();
        media::ensure_engine(&st)?;
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
                let thumb = st.paths.thumbs.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
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
    .await
}

#[tauri::command]
async fn delete_ref(app: AppHandle, id: i64) -> CmdResult<()> {
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        if let Ok(Some(t)) = c.query_row("SELECT thumb FROM refs WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)) {
            let _ = std::fs::remove_file(t);
        }
        c.execute("DELETE FROM refs WHERE id=?1", [id])?;
        Ok(())
    })
    .await
}

// ---------------- import / analysis ----------------

#[tauri::command]
async fn import_paths(app: AppHandle, paths: Vec<String>) -> CmdResult<media::ImportReport> {
    let a = app.clone();
    let rep = blocking(move || media::import_paths(&a, paths)).await?;
    media::spawn_processing(app);
    Ok(rep)
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
    blocking(move || {
        let st = app.state::<AppState>();
        let c = st.db.lock().unwrap();
        let (emb, path, kind, b): (Vec<u8>, String, String, [f32; 4]) = c.query_row(
            "SELECT f.embedding, m.path, m.kind, f.x1, f.y1, f.x2, f.y2 FROM faces f JOIN media m ON m.id=f.media_id WHERE f.id=?1",
            [face_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?])),
        )?;
        let mut thumb: Option<String> = None;
        if kind == "image" {
            if let Ok(img) = face::load_image(std::path::Path::new(&path)) {
                let (w, h) = (img.width() as f32, img.height() as f32);
                let t = st.paths.thumbs.join(format!("ref_{}.jpg", uuid::Uuid::new_v4()));
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
    .await
}

/// "Not my kid": hide this face from future results for that person.
#[tauri::command]
async fn reject_face(app: AppHandle, face_id: i64, person_id: i64) -> CmdResult<()> {
    blocking(move || {
        app.state::<AppState>().db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO rejections(face_id, person_id) VALUES(?1,?2)",
            params![face_id, person_id],
        )?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn export(app: AppHandle, out_dir: String, items: Vec<media::ExportItem>, fade: Option<f64>) -> CmdResult<media::ExportReport> {
    blocking(move || media::export(&app, std::path::Path::new(&out_dir), items, fade.unwrap_or(0.0))).await
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
        let st = app.state::<AppState>();
        let dir = extension::extract(&st.paths)?;
        Ok(extension::open_install_helpers(&dir))
    })
    .await
}

#[tauri::command]
fn open_qxc_web(app: AppHandle) -> CmdResult<()> {
    app.opener().open_url("https://qxc.hi.cn/#/", None::<&str>).map_err(|e| e.to_string())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let data = app.path().app_data_dir().map_err(|e| anyhow!(e))?;
            let paths = Paths::new(data)?;
            let conn = db::open(&paths.data.join("kid-finder.db"))?;
            // A crash mid-way may leave partial temp files behind.
            let _ = std::fs::remove_dir_all(&paths.tmp);
            std::fs::create_dir_all(&paths.tmp)?;
            let _ = extension::extract(&paths);
            app.manage(AppState {
                paths,
                db: Mutex::new(conn),
                engine: Mutex::new(None),
                processing: AtomicBool::new(false),
                bridge_running: AtomicBool::new(false),
                web_cache: Mutex::new(HashMap::new()),
            });
            bridge::start(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_threshold,
            download_models,
            install_ffmpeg,
            list_persons,
            add_person,
            rename_person,
            delete_person,
            add_refs,
            delete_ref,
            import_paths,
            start_processing,
            retry_failed,
            search,
            confirm_face,
            reject_face,
            export,
            qxc_create_login,
            qxc_check_login,
            qxc_logout,
            qxc_albums,
            qxc_sync,
            install_chrome_extension,
            open_qxc_web,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
