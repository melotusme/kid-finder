//! Downloading face models / ffmpeg, and locating ffmpeg.
use crate::{face, AppState, Paths};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

const MODEL_BASE: &str = "https://huggingface.co/public-data/insightface/resolve/main/models/buffalo_l";
#[cfg(windows)]
const FFMPEG_WIN_ZIP: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";

#[derive(Serialize, Clone)]
struct DlProgress<'a> {
    label: &'a str,
    done: u64,
    total: Option<u64>,
}

pub async fn download(app: &AppHandle, url: &str, dest: &Path, label: &str) -> Result<()> {
    let client = reqwest::Client::builder().user_agent("kid-finder").connect_timeout(Duration::from_secs(20)).build()?;
    let resp = client.get(url).send().await?.error_for_status().with_context(|| format!("下载失败: {url}"))?;
    let total = resp.content_length();
    if let Some(p) = dest.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    let part = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await?;
    let mut stream = resp.bytes_stream();
    let (mut done, mut last) = (0u64, Instant::now());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if last.elapsed() > Duration::from_millis(250) {
            last = Instant::now();
            let _ = app.emit("download-progress", DlProgress { label, done, total });
        }
    }
    file.flush().await?;
    drop(file);
    if let Some(t) = total {
        if done != t {
            bail!("下载不完整（{done}/{t}），请重试");
        }
    }
    tokio::fs::rename(&part, dest).await?;
    let _ = app.emit("download-progress", DlProgress { label, done, total: Some(done) });
    Ok(())
}

pub fn models_ready(paths: &Paths) -> bool {
    [face::DET_MODEL, face::REC_MODEL].iter().all(|f| {
        std::fs::metadata(paths.models.join(f)).map(|m| m.len() > 1_000_000).unwrap_or(false)
    })
}

pub async fn download_models(app: &AppHandle, paths: &Paths) -> Result<()> {
    for f in [face::DET_MODEL, face::REC_MODEL] {
        let dest = paths.models.join(f);
        if std::fs::metadata(&dest).map(|m| m.len() > 1_000_000).unwrap_or(false) {
            continue;
        }
        download(app, &format!("{MODEL_BASE}/{f}"), &dest, f).await?;
    }
    Ok(())
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn find_ffmpeg(st: &AppState) -> Option<PathBuf> {
    let local = st.paths.bin.join(exe("ffmpeg"));
    if local.is_file() {
        return Some(local);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            let p = d.join(exe("ffmpeg"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // GUI apps on macOS don't inherit the shell PATH, so check Homebrew locations directly.
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"].iter().map(PathBuf::from).find(|p| p.is_file())
}

#[cfg(windows)]
pub async fn install_ffmpeg(app: &AppHandle, paths: &Paths) -> Result<PathBuf> {
    let zip_path = paths.tmp.join("ffmpeg.zip");
    download(app, FFMPEG_WIN_ZIP, &zip_path, "ffmpeg").await?;
    let bin = paths.bin.clone();
    let zp = zip_path.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let mut z = zip::ZipArchive::new(std::fs::File::open(&zp)?)?;
        std::fs::create_dir_all(&bin)?;
        let dest = bin.join("ffmpeg.exe");
        for i in 0..z.len() {
            let mut f = z.by_index(i)?;
            if f.name().map(|n| n.ends_with("/bin/ffmpeg.exe")).unwrap_or(false) {
                let mut o = std::fs::File::create(&dest)?;
                std::io::copy(&mut f, &mut o)?;
                return Ok(dest);
            }
        }
        bail!("压缩包里没有找到 ffmpeg.exe")
    })
    .await??;
    let _ = std::fs::remove_file(zip_path);
    Ok(out)
}

#[cfg(not(windows))]
pub async fn install_ffmpeg(_app: &AppHandle, _paths: &Paths) -> Result<PathBuf> {
    bail!("macOS 请在终端执行：brew install ffmpeg，然后重启本应用")
}
