//! Custom storage root pointer (`storage.json`), disk statistics, atomic verified
//! cross-drive storage migration, and tombstone-preserving space reclamation.

use crate::media::sha256_file;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// `media.status = 3`: physical managed file was reclaimed to save disk space while
/// `sha256`, `remote_id`, `faces`, and OCR records remain intact in SQLite as tombstones.
pub const STATUS_PURGED: i64 = 3;

const STORAGE_CONFIG_FILE: &str = "storage.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageConfig {
    #[serde(default)]
    storage_root: Option<String>,
}

/// Disk & storage summary for the Settings UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStats {
    pub storage_root: String,
    pub default_root: String,
    pub is_custom: bool,
    pub library_bytes: u64,
    pub thumbs_bytes: u64,
    pub free_bytes: Option<u64>,
    pub purged_count: i64,
    pub external_count: i64,
}

/// Summary returned after an atomic storage root migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrateReport {
    pub old_root: String,
    pub new_root: String,
    pub copied_files: usize,
    pub copied_bytes: u64,
    pub updated_media_rows: usize,
    pub updated_ref_rows: usize,
    pub removed_old_files: usize,
}

/// Summary returned after reclaiming unmatched managed media files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PurgeReport {
    pub purged_count: usize,
    pub freed_bytes: u64,
    pub skipped_external: usize,
    pub skipped_kept: usize,
    pub errors: Vec<String>,
}

/// Loads the configured `storageRoot` from `<app_data_dir>/storage.json` if valid and
/// creatable; otherwise falls back to `app_data_dir`.
pub fn load_storage_root(app_data_dir: &Path) -> PathBuf {
    let cfg_path = app_data_dir.join(STORAGE_CONFIG_FILE);
    let Ok(raw) = fs::read_to_string(&cfg_path) else {
        return app_data_dir.to_path_buf();
    };
    let Ok(cfg) = serde_json::from_str::<StorageConfig>(&raw) else {
        return app_data_dir.to_path_buf();
    };
    let Some(root_str) = cfg.storage_root else {
        return app_data_dir.to_path_buf();
    };
    let trimmed = root_str.trim();
    if trimmed.is_empty() {
        return app_data_dir.to_path_buf();
    }
    let candidate = PathBuf::from(trimmed);
    if fs::create_dir_all(&candidate).is_ok() && candidate.is_dir() {
        candidate
    } else {
        app_data_dir.to_path_buf()
    }
}

/// Persists `new_root` into `<app_data_dir>/storage.json` via atomic write-and-rename.
/// Passing `None` resets the configuration back to the default `app_data_dir`.
pub fn save_storage_root(app_data_dir: &Path, new_root: Option<&Path>) -> Result<()> {
    fs::create_dir_all(app_data_dir)
        .with_context(|| format!("无法创建配置目录 {}", app_data_dir.display()))?;
    if let Some(p) = new_root {
        if p.as_os_str().is_empty() || p.to_string_lossy().trim().is_empty() {
            bail!("存储目录路径不能为空");
        }
    }
    let cfg = StorageConfig {
        storage_root: new_root.map(|p| p.to_string_lossy().to_string()),
    };
    let json = serde_json::to_vec_pretty(&cfg)?;
    let dest = app_data_dir.join(STORAGE_CONFIG_FILE);
    let tmp = app_data_dir.join(format!("{STORAGE_CONFIG_FILE}.part.{}", uuid::Uuid::new_v4()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, &dest) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Computes current managed directory sizes, free disk bytes, purged tombstone count,
/// and external in-place media count.
pub fn compute_storage_stats(
    app_data_dir: &Path,
    storage_root: &Path,
    conn: &Connection,
) -> Result<StorageStats> {
    let library_dir = storage_root.join("library");
    let thumbs_dir = storage_root.join("thumbs");
    let library_bytes = dir_size_bytes(&library_dir);
    let thumbs_bytes = dir_size_bytes(&thumbs_dir);
    let free_bytes = free_disk_bytes(storage_root);

    let purged_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE status = ?1",
        [STATUS_PURGED],
        |r| r.get(0),
    )?;

    let mut external_count: i64 = 0;
    let mut stmt = conn.prepare("SELECT path FROM media WHERE status != ?1")?;
    let rows = stmt.query_map([STATUS_PURGED], |r| r.get::<_, String>(0))?;
    for path_res in rows {
        let p = path_res?;
        if !is_path_inside(Path::new(&p), &library_dir) {
            external_count += 1;
        }
    }

    Ok(StorageStats {
        storage_root: storage_root.to_string_lossy().to_string(),
        default_root: app_data_dir.to_string_lossy().to_string(),
        is_custom: !paths_equivalent(storage_root, app_data_dir),
        library_bytes,
        thumbs_bytes,
        free_bytes,
        purged_count,
        external_count,
    })
}

/// Sums byte sizes of all regular files (excluding `.part.` temp files) under `dir`.
pub fn dir_size_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && !is_temp_part_file(e.path()))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

/// Queries available free disk bytes for the filesystem hosting `path` (or its nearest existing ancestor).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let target = nearest_existing_ancestor(path)?;
    free_disk_bytes_existing(&target)
}

#[cfg(windows)]
fn free_disk_bytes_existing(target: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available_to_caller: *mut u64,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut total_free)
    };
    if ok != 0 {
        Some(free_avail)
    } else {
        None
    }
}

#[cfg(unix)]
fn free_disk_bytes_existing(target: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(target)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    avail_kb.checked_mul(1024)
}

#[cfg(not(any(windows, unix)))]
fn free_disk_bytes_existing(_target: &Path) -> Option<u64> {
    None
}

/// Safely migrates managed `library/` and `thumbs/` files from `old_root` to `new_root`:
/// 1. Validates paths (rejects empty, identical, or `new_root` nested inside `old_root/library` or `old_root/thumbs`).
/// 2. Copies every file via `<name>.part.<uuid>` + rename and verifies BOTH byte length and SHA-256 on disk before touching old files.
/// 3. Rewrites `media.path`, `media.poster`, and `refs.thumb` in a single SQLite transaction where paths start with `old_root` (leaving external in-place paths untouched).
/// 4. Persists `save_storage_root(app_data_dir, Some(new_root))`.
/// 5. Only AFTER DB commit + config write succeeds, removes the old copied files under `old_root/library` and `old_root/thumbs`.
pub fn migrate_storage(
    app_data_dir: &Path,
    conn: &mut Connection,
    old_root: &Path,
    new_root: &Path,
    on_progress: impl Fn(usize, usize, &str),
) -> Result<MigrateReport> {
    if old_root.as_os_str().is_empty() || old_root.to_string_lossy().trim().is_empty() {
        bail!("原存储目录路径不能为空");
    }
    if new_root.as_os_str().is_empty() || new_root.to_string_lossy().trim().is_empty() {
        bail!("新存储目录路径不能为空");
    }

    if paths_equivalent(old_root, new_root) {
        bail!("新存储目录不能与当前存储目录相同");
    }
    if is_path_inside(new_root, &old_root.join("library"))
        || is_path_inside(new_root, &old_root.join("thumbs"))
    {
        bail!("新存储目录不能位于当前 library 或 thumbs 子目录内部");
    }
    if is_path_inside(old_root, &new_root.join("library"))
        || is_path_inside(old_root, &new_root.join("thumbs"))
    {
        bail!("当前存储目录不能位于目标 library 或 thumbs 子目录内部");
    }

    struct CopyItem {
        src: PathBuf,
        dest: PathBuf,
        display: String,
        expected_len: u64,
    }

    let mut items: Vec<CopyItem> = Vec::new();
    for sub in ["library", "thumbs"] {
        let src_base = old_root.join(sub);
        if !src_base.is_dir() {
            continue;
        }
        let dest_base = new_root.join(sub);
        for entry in WalkDir::new(&src_base).follow_links(false).into_iter() {
            let entry = entry.with_context(|| format!("读取目录失败: {}", src_base.display()))?;
            if !entry.file_type().is_file() || is_temp_part_file(entry.path()) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&src_base)
                .map_err(|e| anyhow!("路径解析失败: {e}"))?;
            let meta = entry.metadata()?;
            items.push(CopyItem {
                src: entry.path().to_path_buf(),
                dest: dest_base.join(rel),
                display: format!("{sub}/{}", rel.to_string_lossy()),
                expected_len: meta.len(),
            });
        }
    }

    let total_bytes_needed: u64 = items.iter().map(|i| i.expected_len).sum();
    if let Some(free) = free_disk_bytes(new_root) {
        if free < total_bytes_needed {
            bail!(
                "目标磁盘剩余空间不足（需要 {} 字节，可用 {} 字节）",
                total_bytes_needed,
                free
            );
        }
    }

    fs::create_dir_all(new_root.join("library"))
        .with_context(|| format!("无法在目标位置创建目录: {}", new_root.display()))?;
    fs::create_dir_all(new_root.join("thumbs"))
        .with_context(|| format!("无法在目标位置创建目录: {}", new_root.display()))?;

    let total = items.len();
    on_progress(0, total, "");

    let mut copied_bytes = 0u64;
    for (idx, item) in items.iter().enumerate() {
        on_progress(idx, total, &item.display);
        copy_and_verify_atomic(&item.src, &item.dest, item.expected_len)?;
        copied_bytes += item.expected_len;
    }
    on_progress(total, total, "");

    // Single SQLite transaction rewriting paths under old_root -> new_root
    let (updated_media_rows, updated_ref_rows) = {
        let tx = conn.transaction()?;

        let media_rows: Vec<(i64, String, Option<String>)> = {
            let mut stmt = tx.prepare("SELECT id, path, poster FROM media")?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut media_updated = 0usize;
        for (id, path_str, poster_opt) in media_rows {
            let new_path = rewrite_path_under_root(&path_str, old_root, new_root);
            let new_poster = poster_opt
                .as_deref()
                .and_then(|p| rewrite_path_under_root(p, old_root, new_root));

            if new_path.is_some() || new_poster.is_some() {
                let final_path = new_path.as_deref().unwrap_or(&path_str);
                let final_poster = match (&new_poster, &poster_opt) {
                    (Some(np), _) => Some(np.as_str()),
                    (None, Some(op)) => Some(op.as_str()),
                    (None, None) => None,
                };
                tx.execute(
                    "UPDATE media SET path = ?2, poster = ?3 WHERE id = ?1",
                    params![id, final_path, final_poster],
                )?;
                media_updated += 1;
            }
        }

        let ref_rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, thumb FROM refs WHERE thumb IS NOT NULL AND thumb != ''")?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut refs_updated = 0usize;
        for (id, thumb_str) in ref_rows {
            if let Some(new_thumb) = rewrite_path_under_root(&thumb_str, old_root, new_root) {
                tx.execute(
                    "UPDATE refs SET thumb = ?2 WHERE id = ?1",
                    params![id, new_thumb],
                )?;
                refs_updated += 1;
            }
        }

        tx.commit()?;
        (media_updated, refs_updated)
    };

    // Persist storage.json pointer BEFORE removing any old files
    save_storage_root(app_data_dir, Some(new_root))?;

    // Cleanup old copied files & empty subdirectories under old_root/library and old_root/thumbs
    let mut removed_old_files = 0usize;
    for item in &items {
        if fs::remove_file(&item.src).is_ok() {
            removed_old_files += 1;
        }
    }
    for sub in ["library", "thumbs"] {
        prune_empty_dirs(&old_root.join(sub));
    }

    Ok(MigrateReport {
        old_root: old_root.to_string_lossy().to_string(),
        new_root: new_root.to_string_lossy().to_string(),
        copied_files: total,
        copied_bytes,
        updated_media_rows,
        updated_ref_rows,
        removed_old_files,
    })
}

/// Reclaims disk space by deleting physical files of analyzed (`status = 1`) managed media
/// whose `id` is NOT in `keep_media_ids` AND whose `path` resides strictly inside `managed_library_dir`.
///
/// - External files outside `managed_library_dir` are NEVER deleted or altered.
/// - Updates `media.status = 3` (`STATUS_PURGED`) while preserving `sha256`, `remote_id`,
///   `faces`, and any OCR rows in SQLite so future imports/syncs still deduplicate in <1ms.
pub fn purge_unmatched_files(
    conn: &mut Connection,
    managed_library_dir: &Path,
    keep_media_ids: &HashSet<i64>,
) -> Result<PurgeReport> {
    if managed_library_dir.as_os_str().is_empty()
        || managed_library_dir.to_string_lossy().trim().is_empty()
    {
        bail!("受管媒体库目录不能为空");
    }

    let managed_thumbs_dir = managed_library_dir.parent().map(|p| p.join("thumbs"));

    let candidates: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, path, poster FROM media WHERE status = 1 ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut rep = PurgeReport::default();
    let mut purged_ids: Vec<i64> = Vec::new();

    for (id, path_str, poster_opt) in candidates {
        if keep_media_ids.contains(&id) {
            rep.skipped_kept += 1;
            continue;
        }

        let media_path = Path::new(&path_str);
        if !is_path_inside(media_path, managed_library_dir) {
            rep.skipped_external += 1;
            continue;
        }

        let mut item_freed = 0u64;
        match fs::metadata(media_path) {
            Ok(meta) => {
                let len = meta.len();
                if let Err(e) = fs::remove_file(media_path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        rep.errors.push(format!("{}: {e}", media_path.display()));
                        continue;
                    }
                } else {
                    item_freed += len;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                rep.errors.push(format!("{}: {e}", media_path.display()));
                continue;
            }
        }

        // Remove managed poster thumbnail if present inside library/ or sibling thumbs/
        if let Some(ref poster_str) = poster_opt {
            if !poster_str.is_empty() {
                let poster_path = Path::new(poster_str);
                let poster_is_managed = is_path_inside(poster_path, managed_library_dir)
                    || managed_thumbs_dir
                        .as_deref()
                        .map(|td| is_path_inside(poster_path, td))
                        .unwrap_or(false);
                if poster_is_managed {
                    if let Ok(meta) = fs::metadata(poster_path) {
                        let len = meta.len();
                        if fs::remove_file(poster_path).is_ok() {
                            item_freed += len;
                        }
                    }
                }
            }
        }

        rep.freed_bytes += item_freed;
        purged_ids.push(id);
    }

    if !purged_ids.is_empty() {
        let tx = conn.transaction()?;
        for id in &purged_ids {
            tx.execute(
                "UPDATE media SET status = ?2, poster = NULL WHERE id = ?1",
                params![id, STATUS_PURGED],
            )?;
        }
        tx.commit()?;
        rep.purged_count = purged_ids.len();
    }

    prune_empty_dirs(managed_library_dir);
    Ok(rep)
}

/// Checks whether `child` is equal to or nested inside `parent` using normalized path components
/// (case-insensitive on Windows, preventing `/library_extra` false prefix matches).
pub fn is_path_inside(child: &Path, parent: &Path) -> bool {
    strip_root_prefix(child, parent).is_some()
}

fn copy_and_verify_atomic(src: &Path, dest: &Path, expected_len: u64) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("无效的目标文件路径: {}", dest.display()))?;
    fs::create_dir_all(parent)?;

    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("migrating");
    let part = parent.join(format!("{file_name}.part.{}", uuid::Uuid::new_v4()));

    let copy_res: Result<(u64, String)> = (|| {
        let mut reader = fs::File::open(src)
            .with_context(|| format!("读取源文件失败: {}", src.display()))?;
        let part_file = fs::File::create(&part)
            .with_context(|| format!("创建临时目标文件失败: {}", part.display()))?;
        let mut writer = BufWriter::new(part_file);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n])?;
            written += n as u64;
        }
        writer.flush()?;
        let inner = writer
            .into_inner()
            .map_err(|e| anyhow!("刷新文件缓冲区失败: {e}"))?;
        inner.sync_all()?;
        Ok((written, hex::encode(hasher.finalize())))
    })();

    let (src_len, src_sha) = match copy_res {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };

    if src_len != expected_len {
        let _ = fs::remove_file(&part);
        bail!(
            "源文件长度在迁移过程中发生变化: {} ({src_len} != {expected_len})",
            src.display()
        );
    }

    if let Err(e) = fs::rename(&part, dest) {
        let _ = fs::remove_file(&part);
        return Err(e).with_context(|| format!("重命名目标文件失败: {}", dest.display()));
    }

    // Re-read from disk to verify persisted length & SHA-256 integrity
    let dest_meta = fs::metadata(dest)
        .with_context(|| format!("无法校验目标文件元数据: {}", dest.display()))?;
    if dest_meta.len() != src_len {
        let _ = fs::remove_file(dest);
        bail!(
            "目标文件大小校验失败: {} ({} != {})",
            dest.display(),
            dest_meta.len(),
            src_len
        );
    }
    let dest_sha = sha256_file(dest)
        .with_context(|| format!("计算目标文件 SHA-256 失败: {}", dest.display()))?;
    if dest_sha != src_sha {
        let _ = fs::remove_file(dest);
        bail!(
            "目标文件 SHA-256 校验不一致: {} ({dest_sha} != {src_sha})",
            dest.display()
        );
    }

    Ok(())
}

fn rewrite_path_under_root(raw_path: &str, old_root: &Path, new_root: &Path) -> Option<String> {
    let rel = strip_root_prefix(Path::new(raw_path), old_root)?;
    let joined = if rel.as_os_str().is_empty() {
        new_root.to_path_buf()
    } else {
        new_root.join(rel)
    };
    Some(joined.to_string_lossy().to_string())
}

fn strip_root_prefix(path: &Path, root: &Path) -> Option<PathBuf> {
    let norm_path = normalize_path_best_effort(path);
    let norm_root = normalize_path_best_effort(root);

    let p_comps: Vec<Component<'_>> = norm_path.components().collect();
    let r_comps: Vec<Component<'_>> = norm_root.components().collect();

    if r_comps.is_empty() || p_comps.len() < r_comps.len() {
        return None;
    }

    for (pc, rc) in p_comps.iter().zip(r_comps.iter()) {
        if !components_eq(pc, rc) {
            return None;
        }
    }

    let mut rel = PathBuf::new();
    for c in &p_comps[r_comps.len()..] {
        if let Component::Normal(os) = c {
            rel.push(os);
        }
    }
    Some(rel)
}

fn components_eq(a: &Component<'_>, b: &Component<'_>) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    let na = normalize_path_best_effort(a);
    let nb = normalize_path_best_effort(b);
    let ac: Vec<Component<'_>> = na.components().collect();
    let bc: Vec<Component<'_>> = nb.components().collect();
    ac.len() == bc.len() && ac.iter().zip(bc.iter()).all(|(x, y)| components_eq(x, y))
}

fn normalize_path_best_effort(path: &Path) -> PathBuf {
    // Canonicalize the longest existing prefix so symlinks / 8.3 short names on Windows/macOS resolve identically.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = lexical_clean(path);
    loop {
        if cur.exists() {
            if let Ok(canon) = fs::canonicalize(&cur) {
                let mut base = strip_verbatim_prefix(canon);
                for part in tail.into_iter().rev() {
                    base.push(part);
                }
                return lexical_clean(&base);
            }
            break;
        }
        match (cur.file_name().map(|f| f.to_os_string()), cur.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() && parent != cur => {
                tail.push(name);
                cur = parent;
            }
            _ => break,
        }
    }
    strip_verbatim_prefix(lexical_clean(path))
}

fn lexical_clean(path: &Path) -> PathBuf {
    #[cfg(windows)]
    let path_buf = PathBuf::from(path.to_string_lossy().replace('/', r"\"));
    #[cfg(not(windows))]
    let path_buf = path.to_path_buf();

    let mut out = PathBuf::new();
    for comp in path_buf.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        if cur.exists() {
            return Some(cur);
        }
        let parent = cur.parent()?.to_path_buf();
        if parent.as_os_str().is_empty() || parent == cur {
            return None;
        }
        cur = parent;
    }
}

fn is_temp_part_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.contains(".part."))
        .unwrap_or(false)
}

fn prune_empty_dirs(root: &Path) {
    if !root.is_dir() {
        return;
    }
    let mut dirs: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    // Deepest subdirectories first
    dirs.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn storage_json_roundtrip_and_fallback_to_default_app_data() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_cfg_test_{}", uuid::Uuid::new_v4()));
        let app_data = tmp.join("app_data");
        let custom = tmp.join("custom_drive").join("kid_store");
        fs::create_dir_all(&app_data).unwrap();

        // Default when storage.json does not exist
        assert_eq!(load_storage_root(&app_data), app_data);

        // Save custom storageRoot -> load creates & returns it
        save_storage_root(&app_data, Some(&custom)).unwrap();
        assert_eq!(load_storage_root(&app_data), custom);
        assert!(custom.is_dir());

        // Reset back to default via None
        save_storage_root(&app_data, None).unwrap();
        assert_eq!(load_storage_root(&app_data), app_data);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn migrate_storage_verifies_sha256_rewrites_managed_paths_keeps_external_and_cleans_old() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_mig_test_{}", uuid::Uuid::new_v4()));
        let app_data = tmp.join("app_data");
        let old_root = app_data.clone();
        let new_root = tmp.join("external_ssd").join("kid-finder-data");
        let wechat_ext_dir = tmp.join("xwechat_files").join("wxid_123").join("msg").join("video");

        fs::create_dir_all(old_root.join("library/photos/aa")).unwrap();
        fs::create_dir_all(old_root.join("library/videos/bb")).unwrap();
        fs::create_dir_all(old_root.join("thumbs")).unwrap();
        fs::create_dir_all(&wechat_ext_dir).unwrap();

        let mut conn = db::open(&app_data.join("kid-finder.db")).unwrap();

        // Reject invalid destinations (empty, identical, nested inside library/thumbs)
        assert!(migrate_storage(&app_data, &mut conn, &old_root, Path::new(""), |_, _, _| {}).is_err());
        assert!(migrate_storage(&app_data, &mut conn, &old_root, &old_root, |_, _, _| {}).is_err());
        assert!(migrate_storage(
            &app_data,
            &mut conn,
            &old_root,
            &old_root.join("library/nested"),
            |_, _, _| {}
        )
        .is_err());
        assert!(migrate_storage(
            &app_data,
            &mut conn,
            &old_root,
            &old_root.join("thumbs/nested"),
            |_, _, _| {}
        )
        .is_err());

        // Create managed files + thumbnails + external WeChat video
        let img_bytes = b"managed-photo-content-1234567890";
        let vid_bytes = b"managed-video-content-0987654321";
        let poster_bytes = b"poster-jpeg-bytes";
        let ext_poster_bytes = b"external-video-poster-bytes";
        let ref_thumb_bytes = b"person-ref-face-thumb";
        let ext_vid_bytes = b"in-place-wechat-video-stream";

        let old_img = old_root.join("library/photos/aa/aa111.jpg");
        let old_vid = old_root.join("library/videos/bb/bb222.mp4");
        let old_poster = old_root.join("thumbs/img_1.jpg");
        let old_ext_poster = old_root.join("thumbs/poster_3.jpg");
        let old_ref_thumb = old_root.join("thumbs/ref_abc.jpg");
        let ext_vid = wechat_ext_dir.join("2026-11-class.mp4");

        fs::write(&old_img, img_bytes).unwrap();
        fs::write(&old_vid, vid_bytes).unwrap();
        fs::write(&old_poster, poster_bytes).unwrap();
        fs::write(&old_ext_poster, ext_poster_bytes).unwrap();
        fs::write(&old_ref_thumb, ref_thumb_bytes).unwrap();
        fs::write(&ext_vid, ext_vid_bytes).unwrap();

        let expected_img_sha = sha256_file(&old_img).unwrap();
        let expected_vid_sha = sha256_file(&old_vid).unwrap();

        conn.execute(
            "INSERT INTO persons(name, created_at) VALUES('小宝', '2026-11-21 10:00:00')",
            [],
        )
        .unwrap();
        let pid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO refs(person_id, embedding, thumb, source, created_at) VALUES(?1, ?2, ?3, 'local', '2026-11-21 10:00:00')",
            params![pid, db::emb_to_blob(&[1.0f32, 0.0f32]), old_ref_thumb.to_string_lossy()],
        )
        .unwrap();
        let ref_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status,poster) VALUES(?1,'image',?2,'a.jpg','local','2026-11-21 10:01:00',1,?3)",
            params![expected_img_sha, old_img.to_string_lossy(), old_poster.to_string_lossy()],
        )
        .unwrap();
        let mid_img = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status,poster) VALUES(?1,'video',?2,'b.mp4','local','2026-11-21 10:02:00',1,NULL)",
            params![expected_vid_sha, old_vid.to_string_lossy()],
        )
        .unwrap();
        let mid_vid = conn.last_insert_rowid();

        // External in-place WeChat video whose poster IS stored in managed `thumbs/`
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status,poster) VALUES('sha_ext_wx','video',?1,'2026-11-class.mp4','wechat','2026-11-21 10:03:00',1,?2)",
            params![ext_vid.to_string_lossy(), old_ext_poster.to_string_lossy()],
        )
        .unwrap();
        let mid_ext = conn.last_insert_rowid();

        let progress_calls = std::cell::Cell::new(0usize);
        let rep = migrate_storage(&app_data, &mut conn, &old_root, &new_root, |_, _, _| {
            progress_calls.set(progress_calls.get() + 1);
        })
        .unwrap();

        assert_eq!(rep.copied_files, 5);
        assert_eq!(rep.removed_old_files, 5);
        assert_eq!(rep.updated_media_rows, 3); // 2 managed media + 1 external media's managed poster
        assert_eq!(rep.updated_ref_rows, 1);
        assert!(progress_calls.get() >= 6);

        // Verify SHA-256 integrity of migrated files
        let new_img = new_root.join("library/photos/aa/aa111.jpg");
        let new_vid = new_root.join("library/videos/bb/bb222.mp4");
        let new_poster = new_root.join("thumbs/img_1.jpg");
        let new_ext_poster = new_root.join("thumbs/poster_3.jpg");
        let new_ref_thumb = new_root.join("thumbs/ref_abc.jpg");

        assert_eq!(sha256_file(&new_img).unwrap(), expected_img_sha);
        assert_eq!(sha256_file(&new_vid).unwrap(), expected_vid_sha);
        assert!(new_poster.is_file());
        assert!(new_ext_poster.is_file());
        assert!(new_ref_thumb.is_file());

        // Old copied files removed, whilekid-finder.db & storage.json in app_data remain intact
        assert!(!old_img.exists());
        assert!(!old_vid.exists());
        assert!(!old_poster.exists());
        assert!(!old_ref_thumb.exists());
        assert!(app_data.join("kid-finder.db").is_file());
        assert_eq!(load_storage_root(&app_data), new_root);

        // Verify SQLite rewrites & preservation of external WeChat video path
        let (db_img_path, db_img_poster): (String, Option<String>) = conn
            .query_row("SELECT path, poster FROM media WHERE id=?1", [mid_img], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(PathBuf::from(&db_img_path), new_img);
        assert_eq!(PathBuf::from(db_img_poster.unwrap()), new_poster);

        let db_vid_path: String = conn
            .query_row("SELECT path FROM media WHERE id=?1", [mid_vid], |r| r.get(0))
            .unwrap();
        assert_eq!(PathBuf::from(&db_vid_path), new_vid);

        let (db_ext_path, db_ext_poster): (String, Option<String>) = conn
            .query_row("SELECT path, poster FROM media WHERE id=?1", [mid_ext], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(PathBuf::from(&db_ext_path), ext_vid); // External path untouched!
        assert!(ext_vid.is_file());
        assert_eq!(PathBuf::from(db_ext_poster.unwrap()), new_ext_poster);

        let db_ref_thumb: String = conn
            .query_row("SELECT thumb FROM refs WHERE id=?1", [ref_id], |r| r.get(0))
            .unwrap();
        assert_eq!(PathBuf::from(&db_ref_thumb), new_ref_thumb);

        // Verify StorageStats on migrated root
        let stats = compute_storage_stats(&app_data, &new_root, &conn).unwrap();
        assert!(stats.is_custom);
        assert_eq!(stats.external_count, 1);
        assert_eq!(stats.purged_count, 0);
        assert_eq!(stats.library_bytes, (img_bytes.len() + vid_bytes.len()) as u64);
        assert!(stats.free_bytes.is_some());

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn purge_unmatched_deletes_unmatched_managed_protects_kept_and_external_and_preserves_dedup() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_purge_test_{}", uuid::Uuid::new_v4()));
        let library_dir = tmp.join("library");
        let thumbs_dir = tmp.join("thumbs");
        let external_dir = tmp.join("user_pictures");
        fs::create_dir_all(library_dir.join("photos/11")).unwrap();
        fs::create_dir_all(library_dir.join("photos/22")).unwrap();
        fs::create_dir_all(library_dir.join("photos/33")).unwrap();
        fs::create_dir_all(&thumbs_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let mut conn = db::open(&tmp.join("kid-finder.db")).unwrap();

        // 1. Matched managed photo (in keep_media_ids -> MUST stay on disk & status=1)
        let kept_file = library_dir.join("photos/11/kept.jpg");
        let kept_poster = thumbs_dir.join("img_1.jpg");
        fs::write(&kept_file, vec![0xAA; 1024]).unwrap();
        fs::write(&kept_poster, vec![0xBB; 128]).unwrap();
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,remote_id,imported_at,status,poster) VALUES('sha_kept','image',?1,'kept.jpg','qxc','rem_kept','2026-11-21 11:00:00',1,?2)",
            params![kept_file.to_string_lossy(), kept_poster.to_string_lossy()],
        )
        .unwrap();
        let mid_kept = conn.last_insert_rowid();

        // 2. Unmatched managed photo (NOT in keep_media_ids, status=1 -> physical file + poster deleted, status=3, sha256/remote_id/faces kept!)
        let purge_file = library_dir.join("photos/22/unmatched.jpg");
        let purge_poster = thumbs_dir.join("img_2.jpg");
        fs::write(&purge_file, vec![0xCC; 4096]).unwrap();
        fs::write(&purge_poster, vec![0xDD; 256]).unwrap();
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,remote_id,imported_at,status,poster) VALUES('sha_unmatched','image',?1,'other_kid.jpg','qxc','rem_unmatched','2026-11-21 11:01:00',1,?2)",
            params![purge_file.to_string_lossy(), purge_poster.to_string_lossy()],
        )
        .unwrap();
        let mid_purge = conn.last_insert_rowid();
        // Attach a face embedding to verify faces survive tombstone purge
        conn.execute(
            "INSERT INTO faces(media_id,t,x1,y1,x2,y2,score,embedding) VALUES(?1,NULL,0.1,0.1,0.5,0.5,0.92,?2)",
            params![mid_purge, db::emb_to_blob(&[0.0f32, 1.0f32])],
        )
        .unwrap();

        // 3. Unmatched external WeChat/user photo (outside library/ -> NEVER deleted!)
        let ext_file = external_dir.join("family_original.jpg");
        fs::write(&ext_file, vec![0xEE; 2048]).unwrap();
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('sha_external','image',?1,'family_original.jpg','wechat','2026-11-21 11:02:00',1)",
            params![ext_file.to_string_lossy()],
        )
        .unwrap();
        let mid_ext = conn.last_insert_rowid();

        // 4. Pending unanalyzed managed photo (`status = 0` -> NEVER purged before analysis completes!)
        let pending_file = library_dir.join("photos/33/pending.jpg");
        fs::write(&pending_file, vec![0xFF; 512]).unwrap();
        conn.execute(
            "INSERT INTO media(sha256,kind,path,orig_name,source,imported_at,status) VALUES('sha_pending','image',?1,'pending.jpg','local','2026-11-21 11:03:00',0)",
            params![pending_file.to_string_lossy()],
        )
        .unwrap();

        let mut keep = HashSet::new();
        keep.insert(mid_kept);

        let rep = purge_unmatched_files(&mut conn, &library_dir, &keep).unwrap();
        assert_eq!(rep.purged_count, 1);
        assert_eq!(rep.freed_bytes, 4096 + 256);
        assert_eq!(rep.skipped_kept, 1);
        assert_eq!(rep.skipped_external, 1);
        assert!(rep.errors.is_empty());

        // Physical file checks
        assert!(kept_file.is_file());
        assert!(kept_poster.is_file());
        assert!(!purge_file.exists());
        assert!(!purge_poster.exists());
        assert!(ext_file.is_file()); // External file strictly protected!
        assert!(pending_file.is_file()); // Pending status=0 file strictly protected!

        // Tombstone & <1ms deduplication preservation checks
        let (status, remote_id, poster): (i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, remote_id, poster FROM media WHERE id = ?1",
                [mid_purge],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, STATUS_PURGED);
        assert_eq!(remote_id.as_deref(), Some("rem_unmatched"));
        assert!(poster.is_none());

        // `media::find_by_sha` still deduplicates purged sha256!
        let dedup = crate::media::find_by_sha(&conn, "sha_unmatched").unwrap();
        assert_eq!(
            dedup,
            Some(("other_kid.jpg".to_string(), "2026-11-21 11:01:00".to_string()))
        );

        // `faces` rows remain intact for future sibling/second-child searches!
        let remaining_faces: i64 = conn
            .query_row("SELECT COUNT(*) FROM faces WHERE media_id = ?1", [mid_purge], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_faces, 1);

        // External media status stays 1
        let ext_status: i64 = conn
            .query_row("SELECT status FROM media WHERE id = ?1", [mid_ext], |r| r.get(0))
            .unwrap();
        assert_eq!(ext_status, 1);

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    }
}
