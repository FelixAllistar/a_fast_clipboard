use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use arboard::ImageData;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const DEFAULT_RETENTION_LIMIT: i64 = 100;
const DEFAULT_HOTKEY: &str = "Ctrl+Alt+V";
const THUMBNAIL_SIZE: usize = 48;

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: i64,
    pub kind: String,
    pub text: String,
    pub image_bytes: Option<Vec<u8>>,
    pub image_encoding: String,
    pub image_width: Option<i64>,
    pub image_height: Option<i64>,
    pub thumbnail_bytes: Option<Vec<u8>>,
    pub thumbnail_width: Option<i64>,
    pub thumbnail_height: Option<i64>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub pinned: bool,
}

#[derive(Clone)]
pub struct Store {
    db_path: Arc<PathBuf>,
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let db_path = default_db_path()?;
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let store = Self {
            db_path: Arc::new(db_path),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.db_path
    }

    pub fn list(&self, query: &str, limit: i64) -> Result<Vec<Clip>> {
        self.with_conn(|conn| {
            let trimmed = query.trim();
            let mut clips = Vec::new();

            if trimmed.is_empty() {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, text_content, NULL, COALESCE(image_encoding, 'rgba'),
                            image_width, image_height,
                            thumbnail_bytes, thumbnail_width, thumbnail_height,
                            created_at, last_used_at, use_count, pinned
                     FROM clips
                     WHERE kind IN ('text', 'image')
                     ORDER BY pinned DESC, created_at DESC
                     LIMIT ?1",
                )?;

                let rows = stmt.query_map(params![limit], row_to_clip)?;
                for row in rows {
                    clips.push(row?);
                }
            } else {
                let pattern = format!("%{}%", escape_like(trimmed));
                let mut stmt = conn.prepare(
                    "SELECT id, kind, text_content, NULL, COALESCE(image_encoding, 'rgba'),
                            image_width, image_height,
                            thumbnail_bytes, thumbnail_width, thumbnail_height,
                            created_at, last_used_at, use_count, pinned
                     FROM clips
                     WHERE kind IN ('text', 'image')
                       AND text_content LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                     ORDER BY pinned DESC, created_at DESC
                     LIMIT ?2",
                )?;

                let rows = stmt.query_map(params![pattern, limit], row_to_clip)?;
                for row in rows {
                    clips.push(row?);
                }
            }

            Ok(clips)
        })
    }

    pub fn count(&self) -> Result<i64> {
        self.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM clips", [], |row| row.get(0))
                .map_err(Into::into)
        })
    }

    pub fn get(&self, id: i64) -> Result<Option<Clip>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, kind, text_content, image_bytes, COALESCE(image_encoding, 'rgba'),
                        image_width, image_height,
                        thumbnail_bytes, thumbnail_width, thumbnail_height,
                        created_at, last_used_at, use_count, pinned
                 FROM clips
                 WHERE id = ?1",
                params![id],
                row_to_clip,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn upsert_text(&self, text: &str) -> Result<Option<i64>> {
        let text = normalize_text(text);
        if text.trim().is_empty() {
            return Ok(None);
        }

        let now = now_millis();
        let hash = content_hash("text", &text);
        let retention_limit = self.retention_limit()?;

        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO clips
                    (kind, text_content, image_bytes, image_encoding, image_width, image_height,
                     thumbnail_bytes, thumbnail_width, thumbnail_height,
                     content_hash, created_at, last_used_at, use_count, pinned)
                 VALUES
                    ('text', ?1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?2, ?3, NULL, 0, 0)
                 ON CONFLICT(content_hash) DO UPDATE SET
                    text_content = excluded.text_content,
                    kind = excluded.kind,
                    image_bytes = NULL,
                    image_encoding = NULL,
                    image_width = NULL,
                    image_height = NULL,
                    thumbnail_bytes = NULL,
                    thumbnail_width = NULL,
                    thumbnail_height = NULL,
                    created_at = excluded.created_at",
                params![text, hash, now],
            )?;

            let id = conn.query_row(
                "SELECT id FROM clips WHERE content_hash = ?1",
                params![hash],
                |row| row.get(0),
            )?;

            prune_conn(conn, retention_limit)?;
            Ok(Some(id))
        })
    }

    pub fn upsert_image(&self, image: &ImageData<'_>) -> Result<Option<i64>> {
        if image.width == 0 || image.height == 0 || image.bytes.is_empty() {
            return Ok(None);
        }

        let now = now_millis();
        let preview = format!("[Image {}x{}]", image.width, image.height);
        let bytes = image.bytes.as_ref();
        let hash = content_hash_image(image.width, image.height, bytes);
        let compressed_bytes = lz4_flex::compress_prepend_size(bytes);
        let thumbnail = build_thumbnail_rgba(image.width, image.height, bytes);
        let retention_limit = self.retention_limit()?;

        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO clips
                    (kind, text_content, image_bytes, image_encoding, image_width, image_height,
                     thumbnail_bytes, thumbnail_width, thumbnail_height,
                     content_hash, created_at, last_used_at, use_count, pinned)
                 VALUES
                    ('image', ?1, ?2, 'lz4-rgba', ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 0, 0)
                 ON CONFLICT(content_hash) DO UPDATE SET
                    text_content = excluded.text_content,
                    kind = excluded.kind,
                    image_bytes = excluded.image_bytes,
                    image_encoding = excluded.image_encoding,
                    image_width = excluded.image_width,
                    image_height = excluded.image_height,
                    thumbnail_bytes = excluded.thumbnail_bytes,
                    thumbnail_width = excluded.thumbnail_width,
                    thumbnail_height = excluded.thumbnail_height,
                    created_at = excluded.created_at",
                params![
                    preview,
                    compressed_bytes,
                    image.width as i64,
                    image.height as i64,
                    thumbnail.as_ref().map(|item| item.0.as_slice()),
                    thumbnail.as_ref().map(|item| item.1),
                    thumbnail.as_ref().map(|item| item.2),
                    hash,
                    now
                ],
            )?;

            let id = conn.query_row(
                "SELECT id FROM clips WHERE content_hash = ?1",
                params![hash],
                |row| row.get(0),
            )?;

            prune_conn(conn, retention_limit)?;
            Ok(Some(id))
        })
    }

    pub fn mark_used(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE clips
                 SET last_used_at = ?1, use_count = use_count + 1
                 WHERE id = ?2",
                params![now_millis(), id],
            )?;
            Ok(())
        })
    }

    pub fn toggle_pin(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE clips SET pinned = NOT pinned WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn clear_unpinned(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM clips WHERE pinned = 0", [])?;
            Ok(())
        })
    }

    pub fn retention_limit(&self) -> Result<i64> {
        let raw = self
            .get_setting("retention_limit")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_RETENTION_LIMIT);

        Ok(raw.clamp(10, 10_000))
    }

    pub fn set_retention_limit(&self, value: i64) -> Result<()> {
        let value = value.clamp(10, 10_000);
        self.set_setting("retention_limit", &value.to_string())?;
        self.with_conn(|conn| prune_conn(conn, value))
    }

    pub fn hotkey(&self) -> Result<String> {
        Ok(self
            .get_setting("hotkey")?
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HOTKEY.to_string()))
    }

    pub fn set_hotkey(&self, value: &str) -> Result<()> {
        self.set_setting("hotkey", value.trim())
    }

    pub fn paste_on_select(&self) -> Result<bool> {
        Ok(self
            .get_setting("paste_on_select")?
            .map(|value| value != "false")
            .unwrap_or(true))
    }

    pub fn set_paste_on_select(&self, value: bool) -> Result<()> {
        self.set_setting("paste_on_select", if value { "true" } else { "false" })
    }

    pub fn onboarding_seen(&self) -> Result<bool> {
        Ok(self
            .get_setting("onboarding_seen")?
            .map(|value| value == "true")
            .unwrap_or(false))
    }

    pub fn set_onboarding_seen(&self, value: bool) -> Result<()> {
        self.set_setting("onboarding_seen", if value { "true" } else { "false" })
    }

    pub fn start_with_windows_preference(&self) -> Result<Option<bool>> {
        Ok(self
            .get_setting("start_with_windows")?
            .map(|value| value != "false"))
    }

    pub fn set_start_with_windows_preference(&self, value: bool) -> Result<()> {
        self.set_setting("start_with_windows", if value { "true" } else { "false" })
    }

    fn initialize(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;

                CREATE TABLE IF NOT EXISTS clips (
                    id INTEGER PRIMARY KEY,
                    kind TEXT NOT NULL,
                    text_content TEXT NOT NULL,
                    image_bytes BLOB,
                    image_encoding TEXT,
                    image_width INTEGER,
                    image_height INTEGER,
                    thumbnail_bytes BLOB,
                    thumbnail_width INTEGER,
                    thumbnail_height INTEGER,
                    content_hash TEXT NOT NULL UNIQUE,
                    created_at INTEGER NOT NULL,
                    last_used_at INTEGER,
                    use_count INTEGER NOT NULL DEFAULT 0,
                    pinned INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_clips_created
                    ON clips(created_at DESC);

                CREATE INDEX IF NOT EXISTS idx_clips_pinned_created
                    ON clips(pinned DESC, created_at DESC);

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                ",
            )?;
            ensure_column(conn, "clips", "image_bytes", "BLOB")?;
            ensure_column(conn, "clips", "image_encoding", "TEXT")?;
            ensure_column(conn, "clips", "image_width", "INTEGER")?;
            ensure_column(conn, "clips", "image_height", "INTEGER")?;
            ensure_column(conn, "clips", "thumbnail_bytes", "BLOB")?;
            ensure_column(conn, "clips", "thumbnail_width", "INTEGER")?;
            ensure_column(conn, "clips", "thumbnail_height", "INTEGER")?;
            Ok(())
        })
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings(key, value)
                 VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = Connection::open(&*self.db_path)
            .with_context(|| format!("failed to open {}", self.db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(2))
            .context("failed to set SQLite busy timeout")?;
        f(&conn)
    }
}

fn row_to_clip(row: &rusqlite::Row<'_>) -> rusqlite::Result<Clip> {
    let pinned: i64 = row.get(13)?;
    Ok(Clip {
        id: row.get(0)?,
        kind: row.get(1)?,
        text: row.get(2)?,
        image_bytes: row.get(3)?,
        image_encoding: row.get(4)?,
        image_width: row.get(5)?,
        image_height: row.get(6)?,
        thumbnail_bytes: row.get(7)?,
        thumbnail_width: row.get(8)?,
        thumbnail_height: row.get(9)?,
        created_at: row.get(10)?,
        last_used_at: row.get(11)?,
        use_count: row.get(12)?,
        pinned: pinned != 0,
    })
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;

    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }

    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn prune_conn(conn: &Connection, retention_limit: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM clips
         WHERE pinned = 0
           AND id NOT IN (
             SELECT id
             FROM clips
             WHERE pinned = 0
             ORDER BY created_at DESC
             LIMIT ?1
           )",
        params![retention_limit],
    )?;
    Ok(())
}

fn default_db_path() -> Result<PathBuf> {
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local_app_data)
            .join("AFastClipboard")
            .join("clips.sqlite3"));
    }

    let current_dir = env::current_dir().context("failed to resolve current directory")?;
    Ok(current_dir.join("clips.sqlite3"))
}

fn normalize_text(text: &str) -> String {
    text.trim_matches(|c| c == '\u{0}').to_string()
}

fn content_hash(kind: &str, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(text.as_bytes());

    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn content_hash_image(width: usize, height: usize, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"image");
    hasher.update([0]);
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hasher.update(bytes);

    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn build_thumbnail_rgba(width: usize, height: usize, bytes: &[u8]) -> Option<(Vec<u8>, i64, i64)> {
    if width == 0 || height == 0 || bytes.len() < width.checked_mul(height)?.checked_mul(4)? {
        return None;
    }

    let scale = THUMBNAIL_SIZE as f64 / width.max(height) as f64;
    let target_width = ((width as f64 * scale).round() as usize).clamp(1, THUMBNAIL_SIZE);
    let target_height = ((height as f64 * scale).round() as usize).clamp(1, THUMBNAIL_SIZE);
    let offset_x = (THUMBNAIL_SIZE - target_width) / 2;
    let offset_y = (THUMBNAIL_SIZE - target_height) / 2;
    let mut output = vec![0; THUMBNAIL_SIZE * THUMBNAIL_SIZE * 4];

    for y in 0..target_height {
        for x in 0..target_width {
            let source_x = x * width / target_width;
            let source_y = y * height / target_height;
            let source_index = (source_y * width + source_x) * 4;
            let target_index = ((offset_y + y) * THUMBNAIL_SIZE + offset_x + x) * 4;
            output[target_index..target_index + 4]
                .copy_from_slice(&bytes[source_index..source_index + 4]);
        }
    }

    Some((output, THUMBNAIL_SIZE as i64, THUMBNAIL_SIZE as i64))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn now_millis() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    elapsed.as_millis() as i64
}
