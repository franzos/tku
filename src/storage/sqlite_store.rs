use std::collections::HashSet;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::{params, Connection};

use super::Storage;
use crate::types::UsageRecord;

const SCHEMA_VERSION: i64 = 2;

pub struct SqliteStorage {
    conn: Connection,
}

fn db_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "tku").map(|d| d.cache_dir().join("records.db"))
}

impl SqliteStorage {
    pub fn open() -> Self {
        let conn = match db_path() {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                Connection::open(&path).expect("Failed to open sqlite database")
            }
            None => Connection::open_in_memory().expect("Failed to open in-memory sqlite"),
        };

        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .expect("Failed to set sqlite pragmas");

        // Migrate if schema is outdated (this is a cache — safe to drop and recreate)
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);

        if version < SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS records;
                 DROP TABLE IF EXISTS files;",
            )
            .expect("Failed to drop old tables");
        }

        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS files (
                 file_id    INTEGER PRIMARY KEY,
                 provider   TEXT NOT NULL,
                 path       TEXT NOT NULL,
                 mtime_secs INTEGER NOT NULL,
                 size       INTEGER NOT NULL,
                 UNIQUE (provider, path)
             );

             CREATE TABLE IF NOT EXISTS records (
                 file_id                      INTEGER NOT NULL REFERENCES files(file_id),
                 session_id                   TEXT NOT NULL,
                 timestamp                    TEXT NOT NULL,
                 project                      TEXT NOT NULL,
                 model                        TEXT NOT NULL,
                 message_id                   TEXT NOT NULL,
                 request_id                   TEXT NOT NULL,
                 input_tokens                 INTEGER NOT NULL,
                 output_tokens                INTEGER NOT NULL,
                 cache_creation_input_tokens  INTEGER NOT NULL,
                 cache_read_input_tokens      INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_records_file_id
                 ON records(file_id);

             PRAGMA user_version = {SCHEMA_VERSION};"
        ))
        .expect("Failed to initialize sqlite schema");

        Self { conn }
    }
}

impl Storage for SqliteStorage {
    fn is_cached(&mut self, provider: &str, file_path: &Path, mtime: i64, size: u64) -> bool {
        let key = file_path.to_string_lossy().to_string();
        self.conn
            .query_row(
                "SELECT 1 FROM files
                  WHERE provider = ?1 AND path = ?2
                    AND mtime_secs = ?3 AND size = ?4",
                params![provider, key, mtime, size as i64],
                |_| Ok(true),
            )
            .unwrap_or(false)
    }

    fn insert(
        &mut self,
        provider: &str,
        file_path: &Path,
        mtime: i64,
        size: u64,
        records: Vec<UsageRecord>,
    ) {
        let key = file_path.to_string_lossy().to_string();

        let tx = match self.conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("tku: sqlite transaction failed: {e}");
                return;
            }
        };

        // Delete old records for this file (if it existed before)
        if let Err(e) = tx.execute(
            "DELETE FROM records WHERE file_id IN
                (SELECT file_id FROM files WHERE provider = ?1 AND path = ?2)",
            params![provider, key],
        ) {
            eprintln!("tku: sqlite delete records failed: {e}");
        }

        // Upsert the file entry
        if let Err(e) = tx.execute(
            "INSERT OR REPLACE INTO files (provider, path, mtime_secs, size)
             VALUES (?1, ?2, ?3, ?4)",
            params![provider, key, mtime, size as i64],
        ) {
            eprintln!("tku: sqlite insert file failed: {e}");
            return;
        }
        let file_id = tx.last_insert_rowid();

        for r in &records {
            if let Err(e) = tx.execute(
                "INSERT INTO records (
                    file_id, session_id, timestamp, project, model,
                    message_id, request_id, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    file_id,
                    r.session_id,
                    r.timestamp.to_rfc3339(),
                    r.project,
                    r.model,
                    r.message_id,
                    r.request_id,
                    r.input_tokens as i64,
                    r.output_tokens as i64,
                    r.cache_creation_input_tokens as i64,
                    r.cache_read_input_tokens as i64,
                ],
            ) {
                eprintln!("tku: sqlite insert record failed: {e}");
            }
        }

        if let Err(e) = tx.commit() {
            eprintln!("tku: sqlite commit failed: {e}");
        }
    }

    fn prune(&mut self, provider: &str, existing: &[PathBuf]) {
        let known: HashSet<String> = existing
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let paths: Vec<String> = self
            .conn
            .prepare("SELECT path FROM files WHERE provider = ?1")
            .and_then(|mut stmt| {
                stmt.query_map(params![provider], |row| row.get(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        if let Ok(tx) = self.conn.transaction() {
            for path in &paths {
                if !known.contains(path) {
                    if let Err(e) = tx.execute(
                        "DELETE FROM records WHERE file_id IN
                            (SELECT file_id FROM files WHERE provider = ?1 AND path = ?2)",
                        params![provider, path],
                    ) {
                        eprintln!("tku: sqlite prune records failed: {e}");
                    }
                    if let Err(e) = tx.execute(
                        "DELETE FROM files WHERE provider = ?1 AND path = ?2",
                        params![provider, path],
                    ) {
                        eprintln!("tku: sqlite prune file failed: {e}");
                    }
                }
            }
            if let Err(e) = tx.commit() {
                eprintln!("tku: sqlite prune commit failed: {e}");
            }
        }
    }

    fn flush(&self) {
        // WAL mode — writes are already persisted
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        let mut stmt = match self.conn.prepare(
            "SELECT f.provider, r.session_id, r.timestamp, r.project, r.model,
                    r.message_id, r.request_id, r.input_tokens, r.output_tokens,
                    r.cache_creation_input_tokens, r.cache_read_input_tokens
               FROM records r
               JOIN files f ON r.file_id = f.file_id",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tku: sqlite drain_all query failed: {e}");
                return Vec::new();
            }
        };

        stmt.query_map([], |row| {
            let ts_str: String = row.get(2)?;
            let timestamp = ts_str.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(UsageRecord {
                provider: row.get(0)?,
                session_id: row.get(1)?,
                timestamp,
                project: row.get(3)?,
                model: row.get(4)?,
                message_id: row.get(5)?,
                request_id: row.get(6)?,
                input_tokens: row.get::<_, i64>(7)? as u64,
                output_tokens: row.get::<_, i64>(8)? as u64,
                cache_creation_input_tokens: row.get::<_, i64>(9)? as u64,
                cache_read_input_tokens: row.get::<_, i64>(10)? as u64,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}
