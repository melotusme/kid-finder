use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let c = Connection::open(path)?;
    c.execute_batch(
        r#"
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS persons(
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            aliases TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS refs(
            id INTEGER PRIMARY KEY,
            person_id INTEGER NOT NULL REFERENCES persons(id) ON DELETE CASCADE,
            embedding BLOB NOT NULL, thumb TEXT, source TEXT, sha256 TEXT, created_at TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS media(
            id INTEGER PRIMARY KEY,
            sha256 TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL,
            path TEXT NOT NULL,
            orig_name TEXT NOT NULL,
            source TEXT NOT NULL,
            remote_id TEXT UNIQUE,
            remote_time TEXT,
            imported_at TEXT NOT NULL,
            status INTEGER NOT NULL DEFAULT 0,
            ocr_done INTEGER NOT NULL DEFAULT 0,
            error TEXT, poster TEXT, duration REAL);
        CREATE TABLE IF NOT EXISTS faces(
            id INTEGER PRIMARY KEY,
            media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            t REAL, x1 REAL, y1 REAL, x2 REAL, y2 REAL, score REAL,
            embedding BLOB NOT NULL);
        CREATE INDEX IF NOT EXISTS faces_media ON faces(media_id);
        CREATE TABLE IF NOT EXISTS rejections(
            face_id INTEGER NOT NULL, person_id INTEGER NOT NULL,
            PRIMARY KEY(face_id, person_id));
        CREATE TABLE IF NOT EXISTS ocr_texts(
            id INTEGER PRIMARY KEY,
            media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            x1 REAL NOT NULL, y1 REAL NOT NULL, x2 REAL NOT NULL, y2 REAL NOT NULL,
            text TEXT NOT NULL,
            norm_text TEXT NOT NULL,
            score REAL NOT NULL);
        CREATE INDEX IF NOT EXISTS ocr_texts_media ON ocr_texts(media_id);
        CREATE TABLE IF NOT EXISTS ocr_rejections(
            ocr_id INTEGER NOT NULL,
            person_id INTEGER NOT NULL,
            PRIMARY KEY(ocr_id, person_id));
        CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS web_scans(
            item_id TEXT PRIMARY KEY,
            scanned_at TEXT NOT NULL,
            ocr_done INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS web_faces(
            id INTEGER PRIMARY KEY,
            item_id TEXT NOT NULL REFERENCES web_scans(item_id) ON DELETE CASCADE,
            x1 REAL NOT NULL, y1 REAL NOT NULL, x2 REAL NOT NULL, y2 REAL NOT NULL,
            embedding BLOB NOT NULL);
        CREATE INDEX IF NOT EXISTS web_faces_item ON web_faces(item_id);
        CREATE TABLE IF NOT EXISTS web_ocr_texts(
            id INTEGER PRIMARY KEY,
            item_id TEXT NOT NULL REFERENCES web_scans(item_id) ON DELETE CASCADE,
            x1 REAL NOT NULL, y1 REAL NOT NULL, x2 REAL NOT NULL, y2 REAL NOT NULL,
            text TEXT NOT NULL,
            norm_text TEXT NOT NULL,
            score REAL NOT NULL);
        CREATE INDEX IF NOT EXISTS web_ocr_item ON web_ocr_texts(item_id);
        "#,
    )?;
    // Idempotent column migrations for databases created by earlier versions.
    let _ = c.execute("ALTER TABLE persons ADD COLUMN aliases TEXT NOT NULL DEFAULT ''", []);
    let _ = c.execute("ALTER TABLE refs ADD COLUMN sha256 TEXT", []);
    let _ = c.execute("CREATE INDEX IF NOT EXISTS refs_person_sha ON refs(person_id, sha256)", []);
    let _ = c.execute("ALTER TABLE media ADD COLUMN ocr_done INTEGER NOT NULL DEFAULT 0", []);
    let _ = c.execute("ALTER TABLE web_scans ADD COLUMN ocr_done INTEGER NOT NULL DEFAULT 0", []);
    Ok(c)
}

pub fn get_setting(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn set_setting(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn del_setting(c: &Connection, key: &str) -> Result<()> {
    c.execute("DELETE FROM settings WHERE key=?1", [key])?;
    Ok(())
}

pub fn emb_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_emb(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
