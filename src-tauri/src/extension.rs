//! Bundles the Chrome extension files inside the desktop binary, extracts them
//! to `<appDataDir>/chrome-extension`, and assists the user with one-click browser installation.
use crate::Paths;
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const MANIFEST_JSON: &[u8] = include_bytes!("../../chrome-extension/manifest.json");

const FILES: &[(&str, &[u8])] = &[
    ("manifest.json", MANIFEST_JSON),
    ("background.js", include_bytes!("../../chrome-extension/background.js")),
    ("content.js", include_bytes!("../../chrome-extension/content.js")),
    ("content.css", include_bytes!("../../chrome-extension/content.css")),
    ("popup.html", include_bytes!("../../chrome-extension/popup.html")),
    ("popup.js", include_bytes!("../../chrome-extension/popup.js")),
    ("icons/icon32.png", include_bytes!("../icons/32x32.png")),
    ("icons/icon128.png", include_bytes!("../icons/128x128.png")),
];

/// Returns the `"version"` string declared in the embedded `chrome-extension/manifest.json`.
pub fn bundled_extension_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            serde_json::from_slice::<serde_json::Value>(MANIFEST_JSON)
                .ok()
                .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(str::to_string))
                .expect("chrome-extension/manifest.json must contain a valid string 'version'")
        })
        .as_str()
}

fn parse_version_segments(ver: &str) -> Option<Vec<u32>> {
    let ver = ver.trim();
    if ver.is_empty() {
        return None;
    }
    let segs: Vec<&str> = ver.split('.').collect();
    if !(2..=4).contains(&segs.len()) {
        return None;
    }
    let mut out = Vec::with_capacity(segs.len());
    for s in segs {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push(s.parse::<u32>().ok()?);
    }
    Some(out)
}

/// Checks major-baseline compatibility between the desktop app and Chrome extension:
/// both must parse as dot-separated numeric segments (`2..=4` segments) and share the exact
/// same `(major, minor)` pair (`seg[0]` and `seg[1]`), while `patch` (`seg[2]`) and optional
/// Chrome 4th build segment (`seg[3]`) may iterate independently.
pub fn versions_compatible(app_ver: &str, ext_ver: &str) -> bool {
    let (Some(a), Some(b)) = (parse_version_segments(app_ver), parse_version_segments(ext_ver)) else {
        return false;
    };
    a[0] == b[0] && a[1] == b[1]
}

pub fn extension_dir(paths: &Paths) -> PathBuf {
    paths.data.join("chrome-extension")
}

/// Writes the bundled Chrome extension into `<appDataDir>/chrome-extension`.
pub fn extract(paths: &Paths) -> Result<PathBuf> {
    let app_ver = env!("CARGO_PKG_VERSION");
    let ext_ver = bundled_extension_version();
    anyhow::ensure!(
        versions_compatible(app_ver, ext_ver),
        "内置扩展版本 ({ext_ver}) 与桌面端版本 ({app_ver}) 大版本基线不一致"
    );
    let dir = extension_dir(paths);
    for (rel, bytes) in FILES {
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Only rewrite if missing or content changed so Chrome doesn't complain unnecessarily.
        let same = std::fs::read(&dest).map(|cur| cur == *bytes).unwrap_or(false);
        if !same {
            std::fs::write(&dest, bytes)?;
        }
    }
    Ok(dir)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub dir: String,
    pub browser: Option<String>,
    pub opened_browser: bool,
    pub opened_folder: bool,
}

/// Opens the browser's extensions page (chrome://extensions or edge://extensions)
/// and reveals the extracted `chrome-extension` folder in the OS file manager.
pub fn open_install_helpers(dir: &Path) -> InstallResult {
    let browser = open_browser_extensions_page();
    let opened_folder = reveal_folder(dir);
    InstallResult {
        dir: dir.to_string_lossy().to_string(),
        opened_browser: browser.is_some(),
        browser,
        opened_folder,
    }
}

#[cfg(windows)]
fn open_browser_extensions_page() -> Option<String> {
    let mut candidates: Vec<(&str, PathBuf, &str)> = vec![];
    for env_key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(env_key) {
            let b = PathBuf::from(base);
            candidates.push(("Chrome", b.join(r"Google\Chrome\Application\chrome.exe"), "chrome://extensions/"));
        }
    }
    for env_key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(env_key) {
            let b = PathBuf::from(base);
            candidates.push(("Edge", b.join(r"Microsoft\Edge\Application\msedge.exe"), "edge://extensions/"));
        }
    }
    for (name, exe, url) in candidates {
        if exe.is_file() && Command::new(&exe).arg(url).spawn().is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn open_browser_extensions_page() -> Option<String> {
    for (name, app, url) in [
        ("Chrome", "Google Chrome", "chrome://extensions/"),
        ("Edge", "Microsoft Edge", "edge://extensions/"),
    ] {
        let app_path = format!("/Applications/{app}.app");
        if Path::new(&app_path).exists() && Command::new("open").args(["-a", app, url]).spawn().is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn open_browser_extensions_page() -> Option<String> {
    for (name, bin, url) in [
        ("Chrome", "google-chrome", "chrome://extensions/"),
        ("Chromium", "chromium", "chrome://extensions/"),
        ("Edge", "microsoft-edge", "edge://extensions/"),
    ] {
        if Command::new(bin).arg(url).spawn().is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(windows)]
fn reveal_folder(dir: &Path) -> bool {
    // `/select,<dir>` opens the parent folder with `chrome-extension` highlighted so
    // the user can drag that exact folder straight onto chrome://extensions/.
    let arg = format!("/select,{}", dir.to_string_lossy());
    Command::new("explorer.exe").arg(arg).spawn().is_ok()
}

#[cfg(target_os = "macos")]
fn reveal_folder(dir: &Path) -> bool {
    Command::new("open").arg("-R").arg(dir).spawn().is_ok()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn reveal_folder(dir: &Path) -> bool {
    let target = dir.parent().unwrap_or(dir);
    Command::new("xdg-open").arg(target).spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_writes_all_extension_files() {
        let tmp = std::env::temp_dir().join(format!("kid_finder_ext_test_{}", uuid::Uuid::new_v4()));
        let paths = crate::Paths::new(tmp.clone()).unwrap();
        let dir = extract(&paths).unwrap();
        for (rel, expected) in FILES {
            let p = dir.join(rel);
            assert!(p.is_file(), "missing {rel}");
            assert_eq!(std::fs::read(&p).unwrap(), *expected);
        }
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["manifest_version"], 3);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn desktop_and_extension_versions_stay_aligned() {
        let cargo_ver = env!("CARGO_PKG_VERSION");
        let pkg_json: serde_json::Value =
            serde_json::from_str(include_str!("../../package.json")).expect("valid package.json");
        let tauri_conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("valid tauri.conf.json");

        let pkg_ver = pkg_json["version"].as_str().expect("package.json version");
        let tauri_ver = tauri_conf["version"].as_str().expect("tauri.conf.json version");
        assert_eq!(
            pkg_ver, cargo_ver,
            "package.json version ({pkg_ver}) must match src-tauri/Cargo.toml ({cargo_ver})"
        );
        assert_eq!(
            tauri_ver, cargo_ver,
            "src-tauri/tauri.conf.json version ({tauri_ver}) must match src-tauri/Cargo.toml ({cargo_ver})"
        );

        let ext_ver = bundled_extension_version();
        assert!(
            parse_version_segments(ext_ver).is_some(),
            "chrome-extension/manifest.json version ({ext_ver}) must be 2..=4 dot-separated numeric segments"
        );
        assert!(
            versions_compatible(cargo_ver, ext_ver),
            "Desktop version ({cargo_ver}) and Chrome extension version ({ext_ver}) have incompatible major.minor baselines; keep X.Y aligned when bumping major/feature versions"
        );
    }

    #[test]
    fn versions_compatible_enforces_major_minor_and_allows_patch_or_build() {
        assert!(versions_compatible("0.1.3", "0.1.3"));
        assert!(versions_compatible("0.1.3", "0.1.5"));
        assert!(versions_compatible("0.1.4", "0.1.3.2"));
        assert!(versions_compatible("1.2.0", "1.2.4"));

        assert!(!versions_compatible("0.2.0", "0.1.9"));
        assert!(!versions_compatible("1.0.0", "0.1.3"));
        assert!(!versions_compatible("2.0.0", "1.0.0"));

        assert!(!versions_compatible("", "0.1.3"));
        assert!(!versions_compatible("0", "0.1.3"));
        assert!(!versions_compatible("0.1.3", "0.1.x"));
        assert!(!versions_compatible("0.1.3-beta.1", "0.1.3"));
        assert!(!versions_compatible("0.1.3", "0.1.3.4.5"));
    }
}
