//! Zero-ban-risk WeChat local file discovery, legacy 3.9 `.dat` XOR decoding,
//! and stable watch-folder scanning.
//!
//! Security & anti-ban invariants:
//! - Strictly read-only file-system metadata & plaintext media inspection.
//! - NEVER reads or attaches to any running `Weixin` / `WeChat` process memory.
//! - NEVER reads encrypted SQLCipher chat databases (`db_storage/*.db`, `Msg/*.db`).
//! - Explicitly skips Weixin 4.x `V1`/`V2` AES-hybrid `.dat` containers instead of guessing keys.

use crate::media::{kind_of, VIDEO_EXT};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

/// Weixin 4.0 early encrypted `.dat` magic header (`07 08 56 31 08 07`).
pub const WECHAT_V1_MAGIC: &[u8; 6] = b"\x07\x08V1\x08\x07";
/// Weixin 4.x hybrid AES+XOR encrypted `.dat` magic header (`07 08 56 32 08 07`).
pub const WECHAT_V2_MAGIC: &[u8; 6] = b"\x07\x08V2\x08\x07";

const NON_ACCOUNT_DIRS: &[&str] = &[
    "all_users",
    "all users",
    "applet",
    "backup",
    "wmpf",
    "config",
    "xplugin",
    "radium",
    "locales",
    "crashpad",
    "kvcomm",
];

/// Summary of a discovered local WeChat account media directory (without reading chat text).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WechatSource {
    /// Account directory name (e.g. `wxid_xxx`).
    pub account_id: String,
    /// Client architecture generation: `"4.x"` (`xwechat_files`) or `"3.9"` (`WeChat Files`).
    pub generation: String,
    /// Root path of this account directory.
    pub root_dir: String,
    /// Plaintext video directory (`msg/video` on 4.x or `FileStorage/Video` on 3.9).
    pub video_dir: Option<String>,
    /// Plaintext file attachment directory (`msg/file` on 4.x or `FileStorage/File` on 3.9).
    pub file_dir: Option<String>,
    /// Legacy WeChat 3.9 single-byte XOR image directory (`FileStorage/Image`).
    pub legacy_image_dir: Option<String>,
    /// Weixin 4.x encrypted attachment root (`msg/attach`), counted for UI hints only.
    pub attach_dir: Option<String>,
    /// Number of downloaded plaintext video files (`.mp4`, `.mov`, etc.) in `video_dir`.
    pub video_count: usize,
    /// Number of supported plaintext image/video files in `file_dir`.
    pub file_media_count: usize,
    /// Number of legacy 3.9 `.dat` images in `legacy_image_dir` (excluding `*_t.dat` when base exists).
    pub legacy_dat_count: usize,
    /// Total Weixin 4.x `.dat` attachments under `msg/attach` (including `*_t.dat` thumbnails).
    pub v2_dat_count: usize,
    /// Downloaded non-thumbnail `.dat` attachments under `msg/attach` (excluding `*_t.dat`).
    pub v2_full_dat_count: usize,
}

/// Read-only auto-discovery of local WeChat 4.x (`xwechat_files`) and legacy 3.9 (`WeChat Files`)
/// account directories on Windows and macOS.
pub fn discover_wechat_sources() -> Vec<WechatSource> {
    let roots = candidate_wechat_parent_dirs();
    discover_wechat_sources_from_roots(&roots)
}

/// Discovers valid WeChat account directories underneath the given parent root directories
/// (such as `.../xwechat_files` and `.../WeChat Files`), deduplicating canonical paths.
pub fn discover_wechat_sources_from_roots(roots: &[PathBuf]) -> Vec<WechatSource> {
    let mut seen = HashSet::new();
    let mut sources = Vec::new();

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|e| {
                let ft = e.file_type().ok()?;
                if ft.is_dir() {
                    Some(e.path())
                } else {
                    None
                }
            })
            .collect();
        subdirs.sort();

        for dir in subdirs {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_non_account_dir(name) {
                continue;
            }
            let dedup_key = canonical_dedup_key(&dir);
            if !seen.insert(dedup_key) {
                continue;
            }
            if let Some(src) = inspect_wechat_account_dir(&dir) {
                sources.push(src);
            }
        }
    }

    sources
}

/// Inspects a single candidate WeChat account directory (`4.x` or `3.9`) in strictly read-only mode.
pub fn inspect_wechat_account_dir(account_dir: &Path) -> Option<WechatSource> {
    if !account_dir.is_dir() {
        return None;
    }
    let account_id = account_dir.file_name()?.to_str()?.to_string();
    if is_non_account_dir(&account_id) {
        return None;
    }

    // Detect 4.x (`msg/...` or `db_storage`) vs legacy 3.9 (`FileStorage/...` or `Msg`)
    let msg_dir = account_dir.join("msg");
    let file_storage_dir = account_dir.join("FileStorage");
    let is_v4 = msg_dir.is_dir() || account_dir.join("db_storage").is_dir();
    let is_v39 = file_storage_dir.is_dir() || (!is_v4 && account_dir.join("Msg").is_dir());

    if !is_v4 && !is_v39 {
        return None;
    }

    let generation = if is_v4 { "4.x" } else { "3.9" }.to_string();

    let v4_video = msg_dir.join("video");
    let v39_video = file_storage_dir.join("Video");
    let video_path = if v4_video.is_dir() {
        Some(v4_video)
    } else if v39_video.is_dir() {
        Some(v39_video)
    } else {
        None
    };

    let v4_file = msg_dir.join("file");
    let v39_file = file_storage_dir.join("File");
    let file_path = if v4_file.is_dir() {
        Some(v4_file)
    } else if v39_file.is_dir() {
        Some(v39_file)
    } else {
        None
    };

    let v39_img = file_storage_dir.join("Image");
    let legacy_image_path = if v39_img.is_dir() { Some(v39_img) } else { None };

    let v4_attach = msg_dir.join("attach");
    let attach_path = if v4_attach.is_dir() { Some(v4_attach) } else { None };

    let video_count = video_path.as_deref().map(count_video_files).unwrap_or(0);
    let file_media_count = file_path.as_deref().map(count_supported_media_files).unwrap_or(0);
    let legacy_dat_count = legacy_image_path
        .as_deref()
        .map(count_legacy_dat_files)
        .unwrap_or(0);
    let (v2_dat_count, v2_full_dat_count) = attach_path
        .as_deref()
        .map(count_attach_dat_files)
        .unwrap_or((0, 0));

    Some(WechatSource {
        account_id,
        generation,
        root_dir: account_dir.to_string_lossy().to_string(),
        video_dir: video_path.map(|p| p.to_string_lossy().to_string()),
        file_dir: file_path.map(|p| p.to_string_lossy().to_string()),
        legacy_image_dir: legacy_image_path.map(|p| p.to_string_lossy().to_string()),
        attach_dir: attach_path.map(|p| p.to_string_lossy().to_string()),
        video_count,
        file_media_count,
        legacy_dat_count,
        v2_dat_count,
        v2_full_dat_count,
    })
}

/// Collects all non-empty plaintext media files from a discovered `WechatSource`:
/// - `video_dir`: video files only (`.mp4`, `.mov`, etc., skipping `.jpg` cover thumbnails in `msg/video`).
/// - `file_dir`: both supported image and video attachments (`msg/file` or `FileStorage/File`).
pub fn collect_plaintext_media_files(source: &WechatSource) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(ref vd) = source.video_dir {
        let p = Path::new(vd);
        if p.is_dir() {
            for entry in WalkDir::new(p).follow_links(false).into_iter().flatten() {
                if entry.file_type().is_file() && is_video_ext(entry.path()) {
                    if entry.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                        out.push(entry.into_path());
                    }
                }
            }
        }
    }
    if let Some(ref fd) = source.file_dir {
        let p = Path::new(fd);
        if p.is_dir() {
            for entry in WalkDir::new(p).follow_links(false).into_iter().flatten() {
                if entry.file_type().is_file() && kind_of(entry.path()).is_some() {
                    if entry.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                        out.push(entry.into_path());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Decodes a legacy WeChat 3.9 single-byte XOR `.dat` image buffer in memory.
///
/// - Explicitly returns `None` if `bytes` starts with Weixin 4.x `V1` (`\x07\x08V1\x08\x07`)
///   or `V2` (`\x07\x08V2\x08\x07`) encrypted container headers, or if `bytes.len() < 64`.
/// - Detects the single-byte XOR key against known image magic headers:
///   - JPEG: `FF D8 FF` -> `"jpg"`
///   - PNG: `89 50 4E 47` -> `"png"`
///   - GIF: `47 49 46 38` -> `"gif"`
///   - WebP: `52 49 46 46` (`RIFF`) + `bytes[8..12]` `57 45 42 50` (`WEBP`) -> `"webp"`
pub fn decode_legacy_dat(bytes: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    if bytes.len() < 64 {
        return None;
    }
    if bytes.starts_with(WECHAT_V2_MAGIC) || bytes.starts_with(WECHAT_V1_MAGIC) {
        return None;
    }

    let (ext, key) = detect_xor_magic(bytes)?;
    let decoded: Vec<u8> = if key == 0 {
        bytes.to_vec()
    } else {
        bytes.iter().map(|b| b ^ key).collect()
    };
    if decoded.len() < 64 {
        return None;
    }
    Some((ext, decoded))
}

fn detect_xor_magic(bytes: &[u8]) -> Option<(&'static str, u8)> {
    // PNG: 89 50 4E 47
    {
        let k = bytes[0] ^ 0x89;
        if (bytes[1] ^ k) == 0x50 && (bytes[2] ^ k) == 0x4E && (bytes[3] ^ k) == 0x47 {
            return Some(("png", k));
        }
    }
    // GIF: 47 49 46 38
    {
        let k = bytes[0] ^ 0x47;
        if (bytes[1] ^ k) == 0x49 && (bytes[2] ^ k) == 0x46 && (bytes[3] ^ k) == 0x38 {
            return Some(("gif", k));
        }
    }
    // WebP: RIFF (52 49 46 46) .... WEBP (57 45 42 50)
    {
        let k = bytes[0] ^ 0x52;
        if (bytes[1] ^ k) == 0x49
            && (bytes[2] ^ k) == 0x46
            && (bytes[3] ^ k) == 0x46
            && (bytes[8] ^ k) == 0x57
            && (bytes[9] ^ k) == 0x45
            && (bytes[10] ^ k) == 0x42
            && (bytes[11] ^ k) == 0x50
        {
            return Some(("webp", k));
        }
    }
    // JPEG: FF D8 FF
    {
        let k = bytes[0] ^ 0xFF;
        if (bytes[1] ^ k) == 0xD8 && (bytes[2] ^ k) == 0xFF {
            return Some(("jpg", k));
        }
    }
    None
}

/// Walks a user-configured "微信中转监听文件夹" and returns stable files ready for ingestion:
/// - Supported image/video files (`IMAGE_EXT` / `VIDEO_EXT`), plus legacy `.dat` files
///   (excluding `*_t.dat` thumbnails when a corresponding base `.dat` or `*_h.dat` exists,
///   and skipping Weixin 4.x `V1`/`V2` encrypted `.dat` containers).
/// - Requires `file_size > 0` and `modified()` timestamp at least `min_age` old so files
///   still being written by WeChat "Save As" or drag-export are never read mid-write.
pub fn collect_stable_watch_files(watch_dir: &Path, min_age: Duration) -> Vec<PathBuf> {
    if !watch_dir.is_dir() {
        return Vec::new();
    }
    let now = SystemTime::now();
    let mut out = Vec::new();

    for entry in WalkDir::new(watch_dir).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_media = kind_of(path).is_some();
        let is_dat = !is_media && has_dat_ext(path);
        if !is_media && !is_dat {
            continue;
        }

        if is_dat {
            if is_redundant_thumb_dat(path) || has_v1_or_v2_magic(path) {
                continue;
            }
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() == 0 {
            continue;
        }
        if !min_age.is_zero() {
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age < min_age {
                continue;
            }
        }

        out.push(entry.into_path());
    }

    out.sort();
    out
}

fn is_non_account_dir(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    lower.is_empty() || lower.starts_with('.') || NON_ACCOUNT_DIRS.contains(&lower.as_str())
}

fn canonical_dedup_key(p: &Path) -> String {
    let resolved = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    #[cfg(windows)]
    {
        resolved
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        resolved.to_string_lossy().to_string()
    }
}

fn is_video_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn has_dat_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("dat"))
        .unwrap_or(false)
}

fn is_thumb_dat_stem(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| {
            let lower = s.to_ascii_lowercase();
            lower.ends_with("_t") && lower.len() > 2
        })
        .unwrap_or(false)
}

/// Returns `true` if `path` is `<stem>_t.dat` and `<stem>.dat` (or `<stem>_h.dat`) exists in the same folder.
fn is_redundant_thumb_dat(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = stem.to_ascii_lowercase();
    if !lower.ends_with("_t") || lower.len() <= 2 {
        return false;
    }
    let base = &stem[..stem.len() - 2];
    let Some(parent) = path.parent() else {
        return false;
    };
    parent.join(format!("{base}.dat")).is_file()
        || parent.join(format!("{base}.DAT")).is_file()
        || parent.join(format!("{base}_h.dat")).is_file()
        || parent.join(format!("{base}_h.DAT")).is_file()
}

fn has_v1_or_v2_magic(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 6];
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    &header == WECHAT_V2_MAGIC || &header == WECHAT_V1_MAGIC
}

fn count_video_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && is_video_ext(e.path()))
        .count()
}

fn count_supported_media_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && kind_of(e.path()).is_some())
        .count()
}

fn count_legacy_dat_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| {
            e.file_type().is_file()
                && has_dat_ext(e.path())
                && !is_redundant_thumb_dat(e.path())
        })
        .count()
}

fn count_attach_dat_files(dir: &Path) -> (usize, usize) {
    let mut total = 0usize;
    let mut full = 0usize;
    for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if entry.file_type().is_file() && has_dat_ext(entry.path()) {
            total += 1;
            if !is_thumb_dat_stem(entry.path()) {
                full += 1;
            }
        }
    }
    (total, full)
}

#[cfg(windows)]
fn read_small_config_text(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(512);
    f.take(4096).read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    // Handle UTF-16LE BOM if present, otherwise UTF-8 (stripping BOM / NULs).
    if buf.starts_with(&[0xFF, 0xFE]) && buf.len() >= 4 {
        let u16s: Vec<u16> = buf[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    let s = String::from_utf8_lossy(&buf)
        .trim_start_matches('\u{feff}')
        .replace('\0', "");
    Some(s)
}

#[cfg(any(windows, test))]
fn parse_ini_configured_roots(
    ini_text: &str,
    user_home: Option<&Path>,
    sub_folder: &str,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw_line in ini_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('[') || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        // Skip key=value lines that aren't storage paths (such as pluginUpdateExePath=...)
        if line.contains('=') {
            continue;
        }
        if line.eq_ignore_ascii_case("MyDocument:") {
            if let Some(home) = user_home {
                out.push(home.join("Documents").join(sub_folder));
                out.push(home.join(sub_folder));
            }
        } else {
            let base = PathBuf::from(line);
            out.push(base.join(sub_folder));
            out.push(base);
        }
    }
    out
}

fn candidate_wechat_parent_dirs() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    #[cfg(windows)]
    {
        let user_profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
            user_profile.as_ref().map(|u| u.join("AppData").join("Roaming"))
        });

        // 1. Weixin 4.x config: %APPDATA%\Tencent\xwechat\config\*.ini
        if let Some(ref ad) = appdata {
            let xw_cfg = ad.join("Tencent").join("xwechat").join("config");
            if let Ok(entries) = fs::read_dir(&xw_cfg) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let is_ini = p
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("ini"))
                        .unwrap_or(false);
                    if is_ini {
                        if let Some(txt) = read_small_config_text(&p) {
                            roots.extend(parse_ini_configured_roots(
                                &txt,
                                user_profile.as_deref(),
                                "xwechat_files",
                            ));
                        }
                    }
                }
            }

            // 2. Legacy WeChat 3.9 config: %APPDATA%\Tencent\WeChat\All Users\config\3ebffe94.ini
            let legacy_ini = ad
                .join("Tencent")
                .join("WeChat")
                .join("All Users")
                .join("config")
                .join("3ebffe94.ini");
            if let Some(txt) = read_small_config_text(&legacy_ini) {
                roots.extend(parse_ini_configured_roots(
                    &txt,
                    user_profile.as_deref(),
                    "WeChat Files",
                ));
            }
        }

        // 3. Default Windows paths for Weixin 4.x and legacy WeChat 3.9
        if let Some(ref home) = user_profile {
            roots.push(home.join("xwechat_files"));
            roots.push(home.join("Documents").join("xwechat_files"));
            roots.push(home.join("Documents").join("WeChat Files"));
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            // macOS Weixin 4.x sandbox container path
            roots.push(
                home.join("Library")
                    .join("Containers")
                    .join("com.tencent.xinWeChat")
                    .join("Data")
                    .join("Documents")
                    .join("xwechat_files"),
            );
            roots.push(home.join("xwechat_files"));
            roots.push(home.join("Documents").join("WeChat Files"));
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    fn xor_encode(plain: &[u8], key: u8) -> Vec<u8> {
        plain.iter().map(|b| b ^ key).collect()
    }

    #[test]
    fn decode_legacy_dat_decodes_jpeg_png_webp_gif_and_rejects_v1_v2() {
        // 1. JPEG (XOR key 0xB1, matching real WeChat 3.9 sample header 4E 69 4E 51)
        let mut plain_jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        plain_jpg.resize(96, 0x42);
        let enc_jpg = xor_encode(&plain_jpg, 0xB1);
        assert_eq!(&enc_jpg[..4], &[0x4E, 0x69, 0x4E, 0x51]);
        let (ext, dec_jpg) = decode_legacy_dat(&enc_jpg).expect("should decode XOR JPEG");
        assert_eq!(ext, "jpg");
        assert_eq!(dec_jpg, plain_jpg);

        // 2. PNG (XOR key 0x5A)
        let mut plain_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        plain_png.resize(128, 0x7E);
        let enc_png = xor_encode(&plain_png, 0x5A);
        let (ext, dec_png) = decode_legacy_dat(&enc_png).expect("should decode XOR PNG");
        assert_eq!(ext, "png");
        assert_eq!(dec_png, plain_png);

        // 3. WebP (RIFF....WEBP, XOR key 0xA3)
        let mut plain_webp = vec![
            0x52, 0x49, 0x46, 0x46, // RIFF
            0x40, 0x00, 0x00, 0x00, // chunk size
            0x57, 0x45, 0x42, 0x50, // WEBP
            0x56, 0x50, 0x38, 0x20, // VP8
        ];
        plain_webp.resize(80, 0x11);
        let enc_webp = xor_encode(&plain_webp, 0xA3);
        let (ext, dec_webp) = decode_legacy_dat(&enc_webp).expect("should decode XOR WebP");
        assert_eq!(ext, "webp");
        assert_eq!(dec_webp, plain_webp);

        // 4. GIF (GIF89a, XOR key 0x3C)
        let mut plain_gif = b"GIF89a".to_vec();
        plain_gif.resize(72, 0x99);
        let enc_gif = xor_encode(&plain_gif, 0x3C);
        let (ext, dec_gif) = decode_legacy_dat(&enc_gif).expect("should decode XOR GIF");
        assert_eq!(ext, "gif");
        assert_eq!(dec_gif, plain_gif);

        // 5. Reject Weixin 4.2 V2 encrypted container (`\x07\x08V2\x08\x07`)
        let mut v2_dat = b"\x07\x08V2\x08\x07\x00\x04\x00\x00".to_vec();
        v2_dat.resize(256, 0xAB);
        assert!(decode_legacy_dat(&v2_dat).is_none());

        // 6. Reject Weixin 4.0 V1 encrypted container (`\x07\x08V1\x08\x07`)
        let mut v1_dat = b"\x07\x08V1\x08\x07\x00\x04\x00\x00".to_vec();
        v1_dat.resize(256, 0xCD);
        assert!(decode_legacy_dat(&v1_dat).is_none());

        // 7. Reject buffers shorter than 64 bytes
        assert!(decode_legacy_dat(&enc_jpg[..63]).is_none());
    }

    #[test]
    fn collect_stable_watch_files_skips_mid_write_and_redundant_thumb_dats() {
        let dir = std::env::temp_dir().join(format!("kid_finder_watch_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("sub")).unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(10);
        let write_with_mtime = |p: &Path, bytes: &[u8], mtime: Option<SystemTime>| {
            fs::write(p, bytes).unwrap();
            if let Some(t) = mtime {
                let f = fs::File::options().write(true).open(p).unwrap();
                f.set_times(FileTimes::new().set_modified(t)).unwrap();
            }
        };

        // Stable valid media files (10s old)
        let stable_jpg = dir.join("stable.jpg");
        let stable_mp4 = dir.join("sub").join("clip.mp4");
        write_with_mtime(&stable_jpg, b"valid-jpg-bytes", Some(old_time));
        write_with_mtime(&stable_mp4, b"valid-mp4-bytes", Some(old_time));

        // Legacy .dat pair: base `abc.dat` + thumbnail `abc_t.dat` -> `abc_t.dat` must be skipped
        let mut legacy_jpg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        legacy_jpg.resize(80, 0x55);
        let xor_dat = xor_encode(&legacy_jpg, 0xB1);
        let base_dat = dir.join("abc.dat");
        let thumb_dat = dir.join("abc_t.dat");
        write_with_mtime(&base_dat, &xor_dat, Some(old_time));
        write_with_mtime(&thumb_dat, &xor_dat, Some(old_time));

        // Weixin 4.x V2 encrypted .dat -> must be skipped
        let mut v2_bytes = b"\x07\x08V2\x08\x07".to_vec();
        v2_bytes.resize(80, 0x12);
        let v2_dat = dir.join("v2_encrypted.dat");
        write_with_mtime(&v2_dat, &v2_bytes, Some(old_time));

        // Brand-new mid-write file (modified right now, younger than min_age = 3s)
        let mid_write_jpg = dir.join("writing_now.jpg");
        write_with_mtime(&mid_write_jpg, b"partial-bytes", None);

        // Empty 0-byte file (even if old) -> must be skipped
        let empty_png = dir.join("empty.png");
        write_with_mtime(&empty_png, b"", Some(old_time));

        // Unsupported file (.txt / .part) -> must be skipped
        let part_file = dir.join("downloading.jpg.part");
        write_with_mtime(&part_file, b"temp", Some(old_time));

        let got = collect_stable_watch_files(&dir, Duration::from_secs(3));
        let mut expected = vec![base_dat, stable_jpg, stable_mp4];
        expected.sort();
        assert_eq!(got, expected);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_sources_from_roots_handles_v4_and_v39_without_exposing_chat_text() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_wx_disc_{}", uuid::Uuid::new_v4()));
        let xw_root = tmp.join("xwechat_files");
        let legacy_root = tmp.join("WeChat Files");

        // Non-account system directories that must be ignored
        fs::create_dir_all(xw_root.join("all_users")).unwrap();
        fs::create_dir_all(xw_root.join("Backup")).unwrap();
        fs::create_dir_all(legacy_root.join("All Users")).unwrap();
        fs::create_dir_all(legacy_root.join("Applet")).unwrap();

        // Synthetic Weixin 4.x account
        let acc4 = xw_root.join("wxid_baby_4x");
        fs::create_dir_all(acc4.join("msg/video/2026-11")).unwrap();
        fs::create_dir_all(acc4.join("msg/file/2026-11")).unwrap();
        fs::create_dir_all(acc4.join("msg/attach/chatmd5/2026-11/Img")).unwrap();
        fs::write(acc4.join("msg/video/2026-11/v1.mp4"), b"mp4-data").unwrap();
        fs::write(acc4.join("msg/video/2026-11/v1_thumb.jpg"), b"cover-jpg").unwrap();
        fs::write(acc4.join("msg/file/2026-11/craft.jpg"), b"jpg-data").unwrap();
        fs::write(acc4.join("msg/attach/chatmd5/2026-11/Img/a_t.dat"), b"t1").unwrap();
        fs::write(acc4.join("msg/attach/chatmd5/2026-11/Img/a.dat"), b"f1").unwrap();
        fs::write(acc4.join("msg/attach/chatmd5/2026-11/Img/b_t.dat"), b"t2").unwrap();

        // Synthetic Legacy WeChat 3.9 account
        let acc39 = legacy_root.join("wxid_baby_39");
        fs::create_dir_all(acc39.join("FileStorage/Video/2025-01")).unwrap();
        fs::create_dir_all(acc39.join("FileStorage/Image/2025-01")).unwrap();
        fs::write(acc39.join("FileStorage/Video/2025-01/old.mp4"), b"mp4").unwrap();
        fs::write(acc39.join("FileStorage/Image/2025-01/p1.dat"), b"d1").unwrap();
        fs::write(acc39.join("FileStorage/Image/2025-01/p1_t.dat"), b"t1").unwrap();
        fs::write(acc39.join("FileStorage/Image/2025-01/p2_t.dat"), b"t2").unwrap();

        let found = discover_wechat_sources_from_roots(&[xw_root.clone(), legacy_root, xw_root]);
        assert_eq!(found.len(), 2);

        let s4 = found.iter().find(|s| s.account_id == "wxid_baby_4x").unwrap();
        assert_eq!(s4.generation, "4.x");
        assert_eq!(s4.video_count, 1); // cover .jpg inside msg/video is excluded
        assert_eq!(s4.file_media_count, 1);
        assert_eq!(s4.v2_dat_count, 3);
        assert_eq!(s4.v2_full_dat_count, 1);
        assert_eq!(collect_plaintext_media_files(s4).len(), 2);

        let s39 = found.iter().find(|s| s.account_id == "wxid_baby_39").unwrap();
        assert_eq!(s39.generation, "3.9");
        assert_eq!(s39.video_count, 1);
        assert_eq!(s39.legacy_dat_count, 2); // p1_t.dat skipped because p1.dat exists; p2_t.dat counted

        // Also test INI root parsing (`MyDocument:` and custom path)
        let home = tmp.join("user_home");
        let roots_mydoc = parse_ini_configured_roots("MyDocument:\r\n", Some(&home), "WeChat Files");
        assert!(roots_mydoc.contains(&home.join("Documents").join("WeChat Files")));
        let roots_custom = parse_ini_configured_roots("D:\\WeixinStore\n[Update]\nfoo=bar", Some(&home), "xwechat_files");
        assert_eq!(roots_custom.len(), 2);

        let _ = fs::remove_dir_all(&tmp);
    }
}
