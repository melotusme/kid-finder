//! 群相册 (qxc.hi.cn) client: QR login, album list, incremental sync. See docs/qxc-api.md.
use crate::{db, media, AppState};
use anyhow::{anyhow, bail, Result};
use qrcode::{render::svg, QrCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

const BASE: &str = "https://cloud-release-3g472rpybcfa654e-1326935991.ap-shanghai.app.tcloudbase.com";

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent("kid-finder").timeout(std::time::Duration::from_secs(60)).build()?)
}

fn check(v: &Value) -> Result<()> {
    match v.get("code").and_then(|c| c.as_i64()) {
        Some(0) => Ok(()),
        Some(-3) => bail!("群相册登录已过期，请重新扫码登录"),
        _ => bail!("群相册接口返回错误: {}", v.get("msg").and_then(|m| m.as_str()).unwrap_or("未知")),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginTicket {
    pub token: String,
    pub qr_svg: String,
    pub expires_in_ms: i64,
}

pub async fn create_ticket() -> Result<LoginTicket> {
    let v: Value = client()?
        .post(format!("{BASE}/createWebLoginTicketForWeb"))
        .json(&json!({"env":"release","os":"web"}))
        .send().await?.json().await?;
    check(&v)?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from).ok_or_else(|| anyhow!("缺少字段 {k}"));
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let (token, sid) = (s("token")?, s("sid")?);
    let url = format!("https://qxc.hi.cn/loginqrcode.html?sid={sid}&ts={}", n("ts"));
    let qr_svg = QrCode::new(url.as_bytes()).map_err(|e| anyhow!("生成二维码失败: {e:?}"))?.render::<svg::Color>().min_dimensions(220, 220).build();
    Ok(LoginTicket { token, qr_svg, expires_in_ms: (n("expiryTime") - n("ts")).max(0) })
}

/// Returns the WeChat nickname once the user has confirmed the login on their phone.
pub async fn check_login(st: &AppState, token: &str) -> Result<Option<String>> {
    let v: Value = client()?
        .post(format!("{BASE}/checkLoginForWebV2"))
        .json(&json!({"token":token,"env":"release","os":"web"}))
        .send().await?.json().await?;
    if v.get("code").and_then(|c| c.as_i64()) == Some(-3) {
        return Ok(None);
    }
    check(&v)?;
    let d = v.get("data").and_then(|d| d.get(0)).ok_or_else(|| anyhow!("登录返回为空"))?;
    let tok = d.get("token").and_then(|x| x.as_str()).unwrap_or(token).to_string();
    let dur = d.get("expiryDuration").and_then(|x| x.as_i64()).unwrap_or(86_400_000);
    let name = d.get("wxName").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let exp = chrono::Utc::now().timestamp_millis() + dur;
    let c = st.db.lock().unwrap();
    db::set_setting(&c, "qxc_token", &tok)?;
    db::set_setting(&c, "qxc_expire", &exp.to_string())?;
    db::set_setting(&c, "qxc_name", &name)?;
    Ok(Some(name))
}

pub fn saved_token(st: &AppState) -> Option<(String, String)> {
    let c = st.db.lock().unwrap();
    let exp: i64 = db::get_setting(&c, "qxc_expire")?.parse().ok()?;
    if exp < chrono::Utc::now().timestamp_millis() + 60_000 {
        return None;
    }
    Some((db::get_setting(&c, "qxc_token")?, db::get_setting(&c, "qxc_name").unwrap_or_default()))
}

pub fn logout(st: &AppState) -> Result<()> {
    let c = st.db.lock().unwrap();
    for k in ["qxc_token", "qxc_expire", "qxc_name"] {
        db::del_setting(&c, k)?;
    }
    Ok(())
}

fn token(st: &AppState) -> Result<String> {
    saved_token(st).map(|t| t.0).ok_or_else(|| anyhow!("请先扫码登录群相册"))
}

async fn get(st: &AppState, path: &str, query: &[(&str, String)]) -> Result<Value> {
    let tok = token(st)?;
    let mut q: Vec<(&str, String)> = query.to_vec();
    q.extend([("token", tok), ("env", "release".into()), ("os", "web".into())]);
    let v: Value = client()?.get(format!("{BASE}/{path}")).query(&q).send().await?.json().await?;
    if v.get("code").and_then(|c| c.as_i64()) == Some(-3) {
        let _ = logout(st);
    }
    check(&v)?;
    Ok(v)
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub group_id: String,
    pub group_name: String,
    #[serde(default)]
    pub img_cnt: i64,
    #[serde(default)]
    pub video_cnt: i64,
    #[serde(default)]
    pub last_upload_time: Option<String>,
}

pub async fn albums(st: &AppState) -> Result<Vec<Album>> {
    let v = get(st, "getAllAlbumGroupForWebV3", &[]).await?;
    Ok(serde_json::from_value(v.get("albumList").cloned().unwrap_or(json!([])))?)
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub added: usize,
    /// already imported before (skipped, not downloaded again)
    pub duplicates: usize,
    pub errors: Vec<String>,
    pub complete: bool,
}

#[derive(Serialize, Clone)]
struct SyncProgress {
    page: i64,
    added: usize,
    duplicates: usize,
}

/// Incremental sync, newest first. Stops at the first already-known item once a full
/// pass has completed for this album; until then it skips known items and keeps going
/// so an interrupted first sync can resume.
pub async fn sync(app: &AppHandle, group_id: &str) -> Result<SyncReport> {
    let st = app.state::<AppState>();
    let done_key = format!("qxc_complete:{group_id}");
    let had_full_pass = db::get_setting(&st.db.lock().unwrap(), &done_key).is_some();
    let http = client()?;
    let mut rep = SyncReport::default();
    let mut page = 0i64;
    'outer: loop {
        let v = get(&st, "getSubFolderMediaForWebV3", &[("groupId", group_id.into()), ("subfolderId", "-1".into()), ("page", page.to_string())]).await?;
        let list = v.get("list").and_then(|l| l.as_array()).cloned().unwrap_or_default();
        for it in &list {
            let rid = it.get("_id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            let url = it.get("tempFileURL").and_then(|x| x.as_str()).unwrap_or_default();
            if rid.is_empty() || url.is_empty() {
                continue;
            }
            let known: bool = st.db.lock().unwrap()
                .query_row("SELECT 1 FROM media WHERE remote_id=?1", [&rid], |_| Ok(()))
                .is_ok();
            if known {
                rep.duplicates += 1;
                if had_full_pass {
                    break 'outer;
                }
                continue;
            }
            let kind = if it.get("fileType").and_then(|x| x.as_i64()) == Some(1) { "video" } else { "image" };
            let time = it.get("uploadTime").and_then(|x| x.as_str()).map(String::from);
            if let Err(e) = fetch_one(app, &http, &rid, url, kind, time.as_deref(), &mut rep).await {
                rep.errors.push(format!("{rid}: {e:#}"));
            }
        }
        let _ = app.emit("sync-progress", SyncProgress { page, added: rep.added, duplicates: rep.duplicates });
        if v.get("isEnd").and_then(|x| x.as_bool()).unwrap_or(true) || list.is_empty() {
            if rep.errors.is_empty() {
                db::set_setting(&st.db.lock().unwrap(), &done_key, "1")?;
            }
            rep.complete = true;
            break;
        }
        page += 1;
    }
    if had_full_pass {
        rep.complete = true;
    }
    Ok(rep)
}

async fn fetch_one(app: &AppHandle, http: &reqwest::Client, rid: &str, url: &str, kind: &str, time: Option<&str>, rep: &mut SyncReport) -> Result<()> {
    let st = app.state::<AppState>();
    let raw = url.split('?').next().unwrap_or(url);
    let name = raw.rsplit('/').next().unwrap_or("file").to_string();
    let tmp: PathBuf = st.paths().tmp.join(format!("dl_{}", uuid::Uuid::new_v4()));
    let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or(if kind == "video" { "mp4" } else { "jpg" });
    let tmp = tmp.with_extension(ext);
    let bytes = http.get(raw).send().await?.error_for_status()?.bytes().await?;
    tokio::fs::write(&tmp, &bytes).await?;
    let sha = media::sha256_file(&tmp)?;
    let existing = media::find_by_sha(&st.db.lock().unwrap(), &sha)?;
    if existing.is_some() {
        // Same file was already imported (e.g. saved from WeChat): remember the remote id so we skip it next time.
        st.db.lock().unwrap().execute("UPDATE media SET remote_id=COALESCE(remote_id, ?1) WHERE sha256=?2", [rid, &sha])?;
        let _ = tokio::fs::remove_file(&tmp).await;
        rep.duplicates += 1;
        return Ok(());
    }
    media::store_file(&st, media::NewMedia { src: &tmp, sha: &sha, kind, orig_name: &name, source: "qxc", remote_id: Some(rid), remote_time: time, move_file: true })?;
    rep.added += 1;
    Ok(())
}
