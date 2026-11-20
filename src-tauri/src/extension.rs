//! Bundles the Chrome extension files inside the desktop binary, extracts them
//! to `<appDataDir>/chrome-extension`, and assists the user with one-click browser installation.
use crate::Paths;
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const FILES: &[(&str, &[u8])] = &[
    ("manifest.json", include_bytes!("../../chrome-extension/manifest.json")),
    ("background.js", include_bytes!("../../chrome-extension/background.js")),
    ("content.js", include_bytes!("../../chrome-extension/content.js")),
    ("content.css", include_bytes!("../../chrome-extension/content.css")),
    ("popup.html", include_bytes!("../../chrome-extension/popup.html")),
    ("popup.js", include_bytes!("../../chrome-extension/popup.js")),
    ("icons/icon32.png", include_bytes!("../icons/32x32.png")),
    ("icons/icon128.png", include_bytes!("../icons/128x128.png")),
];

pub fn extension_dir(paths: &Paths) -> PathBuf {
    paths.data.join("chrome-extension")
}

/// Writes the bundled Chrome extension into `<appDataDir>/chrome-extension`.
pub fn extract(paths: &Paths) -> Result<PathBuf> {
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
}
