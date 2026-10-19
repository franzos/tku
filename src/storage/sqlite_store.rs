use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

use super::Storage;
use crate::paths;
use crate::types::{Provider, UsageRecord};

const SCHEMA_VERSION: i64 = 4;

pub struct SqliteStorage {
    conn: Connection,
}

impl SqliteStorage {
    pub fn open() -> Result<Self> {
        let conn = match paths::sqlite_db_file() {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                Connection::open(&path).context("Failed to open sqlite database")?
            }
            None => Connection::open_in_memory().context("Failed to open in-memory sqlite")?,
        };
        Self::with_connection(conn)
    }

    fn with_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .context("Failed to set sqlite pragmas")?;

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("Failed to query sqlite schema version")?;

        // An outdated schema is recreated rather than migrated, which is fine
        // for the parse-cache half of this store and fatal for the archive
        // half: most of these records describe sessions whose transcripts the
        // tool has already deleted, so re-scanning cannot bring them back.
        // Snapshot the database first, and say where it went.
        if version < SCHEMA_VERSION {
            if let Some(path) = paths::sqlite_db_file() {
                if records_present(&conn) {
                    let aside = path.with_extension(format!(
                        "db.quarantine-{}",
                        chrono::Utc::now().format("%Y%m%d%H%M%S")
                    ));
                    match conn.execute("VACUUM INTO ?1", params![aside.to_string_lossy()]) {
                        Ok(_) => eprintln!(
                            "tku: {} uses schema v{version}, which this build recreates. \
                             Copied it to {} first — `tku cache import-sqlite --db {}` \
                             merges those records into the bitcode cache.",
                            path.display(),
                            aside.display(),
                            aside.display()
                        ),
                        Err(e) => bail!(
                            "{} uses schema v{version} and would be recreated, but it could \
                             not be copied aside first ({e}). Refusing to discard it.",
                            path.display()
                        ),
                    }
                }
            }
            conn.execute_batch(
                "DROP TABLE IF EXISTS records;
                 DROP TABLE IF EXISTS files;",
            )
            .context("Failed to drop old tables")?;
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
                 cache_creation_1h_input_tokens INTEGER NOT NULL,
                 cache_read_input_tokens      INTEGER NOT NULL,
                 fast_mode                    INTEGER NOT NULL,
                 account_uuid                 TEXT
             );

             CREATE INDEX IF NOT EXISTS idx_records_file_id
                 ON records(file_id);

             PRAGMA user_version = {SCHEMA_VERSION};"
        ))
        .context("Failed to initialize sqlite schema")?;

        Ok(Self { conn })
    }
}

/// Whether the old schema actually holds records worth preserving. A missing
/// table means a fresh database, which needs no snapshot.
fn records_present(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM records LIMIT 1", [], |_| Ok(()))
        .is_ok()
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

        // Prepare the per-row insert once and reuse it across `records`.
        // `prepare_cached` additionally keeps the compiled statement around for
        // the next `insert()` call on this connection — matters when many
        // provider files land in the same process run.
        match tx.prepare_cached(
            "INSERT INTO records (
                file_id, session_id, timestamp, project, model,
                message_id, request_id, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_creation_1h_input_tokens,
                cache_read_input_tokens, fast_mode,
                account_uuid
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        ) {
            Ok(mut stmt) => {
                for r in &records {
                    if let Err(e) = stmt.execute(params![
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
                        r.cache_creation_1h_input_tokens as i64,
                        r.cache_read_input_tokens as i64,
                        r.fast_mode,
                        r.account_uuid,
                    ]) {
                        eprintln!("tku: sqlite insert record failed: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("tku: sqlite prepare_cached failed: {e}");
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

    fn flush(&mut self) -> bool {
        // WAL mode — writes are already persisted
        true
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        let mut stmt = match self.conn.prepare(
            "SELECT f.provider, r.session_id, r.timestamp, r.project, r.model,
                    r.message_id, r.request_id, r.input_tokens, r.output_tokens,
                    r.cache_creation_input_tokens, r.cache_creation_1h_input_tokens,
                    r.cache_read_input_tokens, r.fast_mode,
                    r.account_uuid
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
            let provider_str: String = row.get(0)?;
            let provider = Provider::from_str(&provider_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    )),
                )
            })?;
            Ok(UsageRecord {
                provider,
                session_id: row.get(1)?,
                timestamp,
                project: row.get(3)?,
                model: row.get(4)?,
                message_id: row.get(5)?,
                request_id: row.get(6)?,
                input_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                cache_creation_input_tokens: row.get::<_, i64>(9)?.max(0) as u64,
                cache_creation_1h_input_tokens: row.get::<_, i64>(10)?.max(0) as u64,
                cache_read_input_tokens: row.get::<_, i64>(11)?.max(0) as u64,
                fast_mode: row.get::<_, bool>(12)?,
                account_uuid: row.get::<_, Option<String>>(13)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    fn rekey_account(
        &mut self,
        old: &str,
        new: &str,
        old_dir: Option<&Path>,
        new_dir: Option<&Path>,
    ) {
        let provider = Provider::Claude.as_str();
        let tx = match self.conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("tku: sqlite transaction failed: {e}");
                return;
            }
        };

        // The aggregate id must be rewritten first: it is selected by the old tag.
        if let Err(e) = tx.execute(
            "UPDATE records
                SET message_id = replace(message_id, ':' || ?1 || ':', ':' || ?2 || ':')
              WHERE message_id LIKE 'tku-agg:%' AND account_uuid = ?1
                AND file_id IN (SELECT file_id FROM files WHERE provider = ?3)",
            params![old, new, provider],
        ) {
            eprintln!("tku: sqlite rekey message ids failed: {e}");
            return;
        }
        if let Err(e) = tx.execute(
            "UPDATE records SET account_uuid = ?2
              WHERE account_uuid = ?1
                AND file_id IN (SELECT file_id FROM files WHERE provider = ?3)",
            params![old, new, provider],
        ) {
            eprintln!("tku: sqlite rekey records failed: {e}");
            return;
        }

        if let (Some(old_dir), Some(new_dir)) = (old_dir, new_dir) {
            let files: Vec<(i64, String)> = match tx
                .prepare("SELECT file_id, path FROM files WHERE provider = ?1")
                .and_then(|mut stmt| {
                    stmt.query_map(params![provider], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect()
                }) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("tku: sqlite rekey file query failed: {e}");
                    return;
                }
            };
            for (file_id, path) in files {
                let Some(to) = super::remap_path(&path, old_dir, new_dir) else {
                    continue;
                };
                // A target already cached keeps its entry; this one stays put.
                if let Err(e) = tx.execute(
                    "UPDATE OR IGNORE files SET path = ?1 WHERE file_id = ?2",
                    params![to, file_id],
                ) {
                    eprintln!("tku: sqlite rekey file path failed: {e}");
                    return;
                }
            }
        }

        if let Err(e) = tx.commit() {
            eprintln!("tku: sqlite rekey commit failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SqliteStorage {
        SqliteStorage::with_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn rec(ts: &str, out: u64, account: Option<&str>) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s1".into(),
            timestamp: ts.parse().unwrap(),
            project: "p".into(),
            model: "opus".into(),
            message_id: format!("msg-{ts}-{out}"),
            request_id: format!("req-{ts}-{out}"),
            input_tokens: 1,
            output_tokens: out,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: account.map(str::to_string),
        }
    }

    fn agg(day: &str, account: &str) -> UsageRecord {
        UsageRecord {
            message_id: format!("tku-agg:s1:{day}:opus:{account}:false"),
            request_id: "tku-agg".into(),
            ..rec(&format!("{day}T01:00:00Z"), 1, Some(account))
        }
    }

    fn paths(s: &SqliteStorage) -> Vec<String> {
        let mut stmt = s
            .conn
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn rekey_account_rewrites_tags_and_aggregate_message_ids() {
        super::super::with_scratch_home("sqlite-rekey-agg", || {
            let mut s = store();
            s.insert(
                "claude",
                Path::new("/proj/a.jsonl"),
                0,
                0,
                vec![agg("2020-01-01", "old"), agg("2020-01-02", "old")],
            );

            s.rekey_account("old", "new", None, None);

            let records = s.drain_all();
            assert_eq!(records.len(), 2);
            for r in &records {
                assert_eq!(r.account_uuid.as_deref(), Some("new"));
                assert!(r.message_id.ends_with(":new:false"), "{}", r.message_id);
            }
        });
    }

    #[test]
    fn rekey_account_leaves_other_accounts_and_untagged_records() {
        super::super::with_scratch_home("sqlite-rekey-others", || {
            let mut s = store();
            s.insert(
                "claude",
                Path::new("/proj/a.jsonl"),
                0,
                0,
                vec![
                    agg("2020-01-01", "other"),
                    rec("2020-01-02T01:00:00Z", 2, Some("old")),
                    rec("2020-01-02T02:00:00Z", 3, None),
                ],
            );
            s.insert(
                "codex",
                Path::new("/codex/a.jsonl"),
                0,
                0,
                vec![rec("2020-01-03T01:00:00Z", 4, Some("old"))],
            );

            s.rekey_account("old", "new", None, None);

            let mut records = s.drain_all();
            records.sort_by_key(|r| r.output_tokens);
            assert_eq!(records[0].account_uuid.as_deref(), Some("other"));
            assert_eq!(
                records[0].message_id,
                "tku-agg:s1:2020-01-01:opus:other:false"
            );
            assert_eq!(records[1].account_uuid.as_deref(), Some("new"));
            assert_eq!(records[1].message_id, "msg-2020-01-02T01:00:00Z-2");
            assert_eq!(records[2].account_uuid, None);
            assert_eq!(records[3].account_uuid.as_deref(), Some("old"));
        });
    }

    #[test]
    fn rekey_account_remaps_file_paths_under_old_dir() {
        super::super::with_scratch_home("sqlite-rekey-paths", || {
            let mut s = store();
            for key in [
                "/old/proj/a.jsonl",
                "/old/proj/c.jsonl",
                "/new/proj/c.jsonl",
                "/oldish/b.jsonl",
            ] {
                s.insert(
                    "claude",
                    Path::new(key),
                    0,
                    0,
                    vec![rec("2020-01-01T01:00:00Z", 1, None)],
                );
            }

            s.rekey_account(
                "old",
                "new",
                Some(Path::new("/old")),
                Some(Path::new("/new")),
            );

            assert_eq!(
                paths(&s),
                [
                    "/new/proj/a.jsonl",
                    "/new/proj/c.jsonl",
                    "/old/proj/c.jsonl",
                    "/oldish/b.jsonl"
                ]
            );
        });
    }
}
