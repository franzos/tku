use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OpenFlags};

use super::bitcode_store::{CachedFile, ProviderCache};
use super::{rollup, Storage};
use crate::paths;
use crate::progress::Progress;
use crate::types::{self, Provider, UsageRecord, AGG_MARKER};

const SCHEMA_VERSION: i64 = 5;
/// Schemas below this are snapshotted and recreated; from here on they are upgraded in place.
const UPGRADE_FROM: i64 = 4;
/// `files.format` of a file written with flagged aggregates and sealed fingerprints.
const FILE_FORMAT: i64 = 2;

/// Columns [`record_from_row`] reads, in order, from `records r JOIN files f`.
const RECORD_COLUMNS: &str = "f.provider, r.session_id, r.timestamp, r.project, r.model,
     r.message_id, r.request_id, r.input_tokens, r.output_tokens,
     r.cache_creation_input_tokens, r.cache_creation_1h_input_tokens,
     r.cache_read_input_tokens, r.fast_mode, r.account_uuid, r.aggregate";

pub struct SqliteStorage {
    conn: Connection,
    rollup_days: u32,
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
        Self::with_connection(conn, super::bitcode_store::configured_rollup_days())
    }

    fn with_connection(conn: Connection, rollup_days: u32) -> Result<Self> {
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
        if version < UPGRADE_FROM {
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
                 DROP TABLE IF EXISTS files;
                 DROP TABLE IF EXISTS sealed;",
            )
            .context("Failed to drop old tables")?;
        }

        conn.execute_batch(
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
                 ON records(file_id);",
        )
        .context("Failed to initialize sqlite schema")?;
        upgrade_schema(&conn).context("Failed to upgrade sqlite schema to v5")?;

        Ok(Self { conn, rollup_days })
    }

    /// Bring every file an older build wrote to the current file format, one
    /// transaction per file so an interrupted run resumes where it stopped.
    /// A file with an unreadable row keeps its rows and loses its seals.
    pub(crate) fn upgrade_legacy_files(&mut self, progress: &Progress) -> Result<LegacyUpgrade> {
        let providers: Vec<String> = self
            .conn
            .prepare("SELECT DISTINCT provider FROM files WHERE format < ?1 ORDER BY provider")?
            .query_map(params![FILE_FORMAT], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut done = LegacyUpgrade::default();
        for provider in providers {
            let rows: Vec<(i64, String, i64, u64)> = self
                .conn
                .prepare(
                    "SELECT file_id, path, mtime_secs, size FROM files
                      WHERE provider = ?1 AND format < ?2",
                )?
                .query_map(params![provider, FILE_FORMAT], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)?.max(0) as u64,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut pc = ProviderCache::default();
            let mut ids = HashMap::new();
            for (file_id, path, mtime_secs, size) in rows {
                let Some(mut cf) = load_or_warn(&self.conn, file_id, &path, mtime_secs, size)
                else {
                    // Rows stay as stored; `drain_all` skips the ones it cannot read.
                    // Seals go: an older build never writes them.
                    self.conn
                        .execute("DELETE FROM sealed WHERE file_id = ?1", params![file_id])?;
                    self.conn.execute(
                        "UPDATE files SET format = ?2 WHERE file_id = ?1",
                        params![file_id, FILE_FORMAT],
                    )?;
                    done.unreadable += 1;
                    continue;
                };
                // An older build never writes seals; these belong to a file that had this id before.
                cf.sealed.clear();
                pc.files.insert(path.clone(), cf);
                ids.insert(path, (file_id, mtime_secs, size));
            }
            let (refreshed, rescan) = super::upgrade::upgrade_cache(&provider, &mut pc, progress);
            done.refreshed += refreshed;
            done.rescan += rescan;
            for (path, cf) in &pc.files {
                let (file_id, mtime_secs, size) = ids[path];
                let tx = self.conn.transaction()?;
                // Another process may have re-inserted it meanwhile.
                let still_legacy: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM files
                                     WHERE file_id = ?1 AND format < ?2 AND path = ?3
                                       AND mtime_secs = ?4 AND size = ?5)",
                    params![file_id, FILE_FORMAT, path, mtime_secs, size as i64],
                    |row| row.get(0),
                )?;
                if still_legacy {
                    write_file(&tx, file_id, cf, false)?;
                    done.upgraded += 1;
                }
                tx.commit()?;
            }
        }
        Ok(done)
    }

    /// Roll up every file whose newest record predates the configured age,
    /// together with the files holding copies of its messages, in one transaction.
    fn roll_up(&mut self) -> rusqlite::Result<()> {
        let cutoff = Utc::now() - Duration::days(i64::from(self.rollup_days));
        let tx = self.conn.transaction()?;
        let providers: Vec<String> = tx
            .prepare(
                "SELECT DISTINCT provider FROM files
                  WHERE rolled = 0 AND format >= ?1 AND newest_ms < ?2",
            )?
            .query_map(params![FILE_FORMAT, cutoff.timestamp_millis()], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        if providers.is_empty() {
            return Ok(());
        }
        // An older build may have re-inserted or pruned files, orphaning their seals.
        tx.execute(
            "DELETE FROM sealed WHERE file_id NOT IN (SELECT file_id FROM files)",
            [],
        )?;
        for provider in providers {
            roll_up_provider(&tx, &provider, cutoff)?;
        }
        tx.commit()
    }
}

/// What [`SqliteStorage::upgrade_legacy_files`] did, in files.
#[derive(Default)]
pub(crate) struct LegacyUpgrade {
    pub(crate) upgraded: usize,
    /// Re-parsed from an unchanged source.
    pub(crate) refreshed: usize,
    /// Source changed, so the rescan replaces them.
    pub(crate) rescan: usize,
    /// Holding a row this build cannot read, so marked upgraded with its rows as stored.
    pub(crate) unreadable: usize,
}

/// Whether the database at `path` holds files an older build wrote, or
/// predates the `files.format` column. Opened read-only; a missing or
/// unreadable database is not.
pub(crate) fn has_legacy_files(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    match has_column(&conn, "files", "format") {
        Ok(true) => conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM files WHERE format < ?1)",
                params![FILE_FORMAT],
                |row| row.get(0),
            )
            .unwrap_or(false),
        Ok(false) => has_column(&conn, "files", "file_id").unwrap_or(false),
        Err(_) => false,
    }
}

/// Copy the database at `path` to the new file `to`, read-only, and sync it.
pub(crate) fn backup(path: &Path, to: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} read-only", path.display()))?;
    conn.execute("VACUUM INTO ?1", params![to.to_string_lossy()])
        .with_context(|| format!("copy {} to {}", path.display(), to.display()))?;
    std::fs::File::open(to)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("sync {}", to.display()))?;
    crate::atomic_write::sync_dir(to.parent().unwrap_or(Path::new(".")))?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )
}

/// [`load_file`], or `None` after a warning when it fails.
fn load_or_warn(
    conn: &Connection,
    file_id: i64,
    path: &str,
    mtime_secs: i64,
    size: u64,
) -> Option<CachedFile> {
    match load_file(conn, file_id, mtime_secs, size) {
        Ok(cf) => Some(cf),
        Err(e) => {
            eprintln!("tku: sqlite file {path} not readable ({e}); left as it is");
            None
        }
    }
}

/// A file's rows and sealed fingerprints. Fails on any unreadable row, so a
/// caller rewriting the file never drops one.
fn load_file(
    conn: &Connection,
    file_id: i64,
    mtime_secs: i64,
    size: u64,
) -> rusqlite::Result<CachedFile> {
    let records = conn
        .prepare_cached(&format!(
            "SELECT {RECORD_COLUMNS}
               FROM records r
               JOIN files f ON r.file_id = f.file_id
              WHERE r.file_id = ?1
              ORDER BY r.rowid"
        ))?
        .query_map(params![file_id], record_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let sealed = conn
        .prepare_cached("SELECT fp FROM sealed WHERE file_id = ?1 ORDER BY fp")?
        .query_map(params![file_id], |row| row.get::<_, i64>(0))?
        .map(|fp| fp.map(i64::cast_unsigned))
        .collect::<rusqlite::Result<_>>()?;
    Ok(CachedFile {
        mtime_secs,
        size,
        records,
        sealed,
    })
}

/// Replace a file's rows and sealed fingerprints with `cf`'s, in the current format.
fn write_file(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    cf: &CachedFile,
    rolled: bool,
) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM records WHERE file_id = ?1", params![file_id])?;
    tx.execute("DELETE FROM sealed WHERE file_id = ?1", params![file_id])?;
    insert_rows(tx, file_id, &cf.records)?;
    let mut seal = tx.prepare_cached("INSERT INTO sealed (file_id, fp) VALUES (?1, ?2)")?;
    for &fp in &cf.sealed {
        seal.execute(params![file_id, fp_to_sql(fp)])?;
    }
    let newest_ms = cf
        .records
        .iter()
        .map(|r| r.timestamp.timestamp_millis())
        .max();
    tx.execute(
        "UPDATE files SET format = ?2, newest_ms = ?3, rolled = ?4 WHERE file_id = ?1",
        params![file_id, FILE_FORMAT, newest_ms, rolled],
    )?;
    Ok(())
}

fn insert_rows(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    records: &[UsageRecord],
) -> rusqlite::Result<()> {
    // `prepare_cached` keeps the compiled statement for the next file on this connection.
    let mut stmt = tx.prepare_cached(
        "INSERT INTO records (
            file_id, session_id, timestamp, project, model,
            message_id, request_id, input_tokens, output_tokens,
            cache_creation_input_tokens, cache_creation_1h_input_tokens,
            cache_read_input_tokens, fast_mode,
            account_uuid, aggregate
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
    )?;
    for r in records {
        stmt.execute(params![
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
            r.aggregate,
        ])?;
    }
    Ok(())
}

/// [`rollup::rollup`] over one provider's unrolled old files. Besides them it
/// loads, whole, every file holding a copy of one of their messages, and the
/// sealed fingerprints of every file sealing a message any of those hold, so
/// the rollup sees each copy and owner it would see in the bitcode store.
fn roll_up_provider(
    tx: &rusqlite::Transaction<'_>,
    provider: &str,
    cutoff: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let cutoff_ms = cutoff.timestamp_millis();
    let candidates: Vec<i64> = tx
        .prepare(
            "SELECT file_id FROM files
              WHERE provider = ?1 AND rolled = 0 AND format >= ?2 AND newest_ms < ?3",
        )?
        .query_map(params![provider, FILE_FORMAT, cutoff_ms], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let peers: Vec<i64> = tx
        .prepare(
            "SELECT DISTINCT p.file_id
               FROM files cf
               JOIN records c ON c.file_id = cf.file_id
               JOIN records p ON p.message_id = c.message_id AND p.request_id = c.request_id
               JOIN files pf ON pf.file_id = p.file_id
              WHERE cf.provider = ?1 AND cf.rolled = 0 AND cf.format >= ?2 AND cf.newest_ms < ?3
                AND c.aggregate = 0 AND c.request_id != ?4
                AND (c.message_id != '' OR c.request_id != '')
                AND p.file_id != c.file_id AND pf.provider = ?1",
        )?
        .query_map(
            params![provider, FILE_FORMAT, cutoff_ms, AGG_MARKER],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;

    let mut files: HashMap<String, CachedFile> = HashMap::new();
    let mut ids: HashMap<String, i64> = HashMap::new();
    let mut loaded: HashSet<i64> = HashSet::new();
    let mut file_row =
        tx.prepare_cached("SELECT path, mtime_secs, size FROM files WHERE file_id = ?1")?;
    for &file_id in candidates.iter().chain(&peers) {
        if loaded.contains(&file_id) {
            continue;
        }
        let (path, mtime_secs, size): (String, i64, i64) = file_row
            .query_row(params![file_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        let Some(cf) = load_or_warn(tx, file_id, &path, mtime_secs, size.max(0) as u64) else {
            // Not retried on every flush; a re-insert resets it.
            if candidates.contains(&file_id) {
                tx.execute(
                    "UPDATE files SET rolled = 1 WHERE file_id = ?1",
                    params![file_id],
                )?;
            }
            continue;
        };
        files.insert(path.clone(), cf);
        ids.insert(path, file_id);
        loaded.insert(file_id);
    }

    let fps: HashSet<u64> = files
        .values()
        .flat_map(|cf| &cf.records)
        .filter(|r| rollup::keyed(r))
        .map(rollup::fingerprint)
        .collect();
    let mut owner = tx.prepare_cached(
        "SELECT s.file_id, f.path FROM sealed s JOIN files f ON f.file_id = s.file_id
          WHERE s.fp = ?1 AND f.provider = ?2",
    )?;
    for fp in fps {
        let owners: Vec<(i64, String)> = owner
            .query_map(params![fp_to_sql(fp), provider], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (file_id, path) in owners {
            if !loaded.contains(&file_id) {
                // Records stay empty, so the rollup reads its seals and never rolls it.
                files
                    .entry(path)
                    .or_insert_with(|| CachedFile {
                        mtime_secs: 0,
                        size: 0,
                        records: Vec::new(),
                        sealed: Vec::new(),
                    })
                    .sealed
                    .push(fp);
            }
        }
    }

    let candidates: HashSet<i64> = candidates
        .into_iter()
        .filter(|id| loaded.contains(id))
        .collect();
    let mut unroll = tx.prepare_cached("UPDATE files SET rolled = 0 WHERE file_id = ?1")?;
    for (path, _) in rollup::rollup(&mut files, cutoff) {
        let Some(&file_id) = ids.get(&path) else {
            continue;
        };
        if candidates.contains(&file_id) {
            write_file(tx, file_id, &files[&path], true)?;
        } else {
            // Its own peers were not loaded; a later flush rolls it as a candidate.
            unroll.execute(params![file_id])?;
        }
    }
    // Whatever the rollup left unchanged is already rolled.
    let mut mark = tx.prepare_cached("UPDATE files SET rolled = 1 WHERE file_id = ?1")?;
    for file_id in candidates {
        mark.execute(params![file_id])?;
    }
    Ok(())
}

/// Bring a v4 schema to v5 in place. Every step is idempotent, because an older
/// binary resets `user_version` to 4 while keeping the added columns.
fn upgrade_schema(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for (table, column, decl) in [
        ("records", "aggregate", "INTEGER NOT NULL DEFAULT 0"),
        ("files", "format", "INTEGER NOT NULL DEFAULT 1"),
        ("files", "newest_ms", "INTEGER"),
        ("files", "rolled", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !has_column(&tx, table, column)? {
            tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
        }
    }
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS sealed (
             file_id INTEGER NOT NULL,
             fp      INTEGER NOT NULL,
             PRIMARY KEY (file_id, fp)
         ) WITHOUT ROWID;

         CREATE INDEX IF NOT EXISTS idx_sealed_fp ON sealed(fp);

         CREATE INDEX IF NOT EXISTS idx_records_message
             ON records(message_id, request_id);

         PRAGMA user_version = {SCHEMA_VERSION};"
    ))?;
    tx.commit()
}

/// `sealed.fp` holds the u64 fingerprint bit-cast to sqlite's i64.
fn fp_to_sql(fp: u64) -> i64 {
    fp.cast_signed()
}

/// Give account `old`'s aggregate rows of `provider` the [`types::aggregate_id`] for account `new`.
fn rekey_aggregates(
    tx: &rusqlite::Transaction<'_>,
    old: &str,
    new: &str,
    provider: &str,
) -> rusqlite::Result<()> {
    let rows: Vec<(i64, UsageRecord)> = tx
        .prepare(&format!(
            "SELECT {RECORD_COLUMNS}, r.rowid
               FROM records r
               JOIN files f ON r.file_id = f.file_id
              WHERE (r.aggregate = 1 OR r.request_id = ?1)
                AND r.account_uuid = ?2 AND f.provider = ?3"
        ))?
        .query_map(params![AGG_MARKER, old, provider], |row| {
            Ok((row.get(15)?, record_from_row(row)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut update = tx.prepare(
        "UPDATE records SET message_id = ?1, request_id = ?2, aggregate = 1 WHERE rowid = ?3",
    )?;
    for (rowid, mut r) in rows {
        if !r.aggregate {
            continue;
        }
        r.account_uuid = Some(new.to_string());
        update.execute(params![types::aggregate_id(&r), AGG_MARKER, rowid])?;
    }
    Ok(())
}

/// A record from [`RECORD_COLUMNS`]. A row an
/// older build wrote with a `tku-agg:` id is read as a flagged aggregate.
fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    let ts_str: String = row.get(2)?;
    let timestamp = ts_str.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
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
    let mut r = UsageRecord {
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
        aggregate: row.get::<_, bool>(14)?,
    };
    if !r.aggregate && r.request_id == AGG_MARKER {
        types::adopt_legacy_aggregate(&mut r);
    }
    Ok(r)
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
        let records = crate::dedup::dedup(records);

        let tx = match self.conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("tku: sqlite transaction failed: {e}");
                return;
            }
        };

        // Delete old records and sealed fingerprints for this file (if it existed before)
        for table in ["records", "sealed"] {
            if let Err(e) = tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE file_id IN
                        (SELECT file_id FROM files WHERE provider = ?1 AND path = ?2)"
                ),
                params![provider, key],
            ) {
                eprintln!("tku: sqlite delete {table} failed: {e}");
            }
        }

        let newest_ms = records.iter().map(|r| r.timestamp.timestamp_millis()).max();
        if let Err(e) = tx.execute(
            "INSERT OR REPLACE INTO files (provider, path, mtime_secs, size, format, newest_ms, rolled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![provider, key, mtime, size as i64, FILE_FORMAT, newest_ms],
        ) {
            eprintln!("tku: sqlite insert file failed: {e}");
            return;
        }
        let file_id = tx.last_insert_rowid();
        // A reused file_id can still carry sealed rows of a file an older build deleted.
        if let Err(e) = tx.execute("DELETE FROM sealed WHERE file_id = ?1", params![file_id]) {
            eprintln!("tku: sqlite delete sealed failed: {e}");
        }

        if let Err(e) = insert_rows(&tx, file_id, &records) {
            eprintln!("tku: sqlite insert record failed: {e}");
            return;
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
                    for table in ["records", "sealed"] {
                        if let Err(e) = tx.execute(
                            &format!(
                                "DELETE FROM {table} WHERE file_id IN
                                    (SELECT file_id FROM files WHERE provider = ?1 AND path = ?2)"
                            ),
                            params![provider, path],
                        ) {
                            eprintln!("tku: sqlite prune {table} failed: {e}");
                        }
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
        // WAL mode: inserts are already persisted.
        if let Err(e) = self.roll_up() {
            eprintln!("tku: sqlite rollup failed: {e}");
            return false;
        }
        true
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        let sealed_in: HashMap<i64, i64> = match self
            .conn
            .prepare("SELECT s.fp, s.file_id FROM sealed s JOIN files f ON s.file_id = f.file_id")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            }) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("tku: sqlite sealed query failed: {e}");
                return Vec::new();
            }
        };

        let mut stmt = match self.conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS}, r.file_id
               FROM records r
               JOIN files f ON r.file_id = f.file_id
              ORDER BY f.provider, f.path, r.rowid"
        )) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tku: sqlite drain_all query failed: {e}");
                return Vec::new();
            }
        };

        stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(15)?, record_from_row(row)?))
        })
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter(|(file_id, r)| {
                    sealed_in.is_empty()
                        || r.aggregate
                        || (r.message_id.is_empty() && r.request_id.is_empty())
                        || sealed_in
                            .get(&fp_to_sql(crate::dedup::stable_fingerprint(
                                r.provider,
                                &r.message_id,
                                &r.request_id,
                            )))
                            .is_none_or(|owner| owner == file_id)
                })
                .map(|(_, r)| r)
                .collect()
        })
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

        // Aggregate ids must be rewritten first: they are selected by the old tag.
        if let Err(e) = rekey_aggregates(&tx, old, new, provider) {
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

/// The schema-4 tables, as the build before schema 5 created them.
#[cfg(test)]
pub(crate) const V4_SCHEMA: &str = "
    CREATE TABLE files (
        file_id    INTEGER PRIMARY KEY,
        provider   TEXT NOT NULL,
        path       TEXT NOT NULL,
        mtime_secs INTEGER NOT NULL,
        size       INTEGER NOT NULL,
        UNIQUE (provider, path)
    );
    CREATE TABLE records (
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
    CREATE INDEX idx_records_file_id ON records(file_id);
    PRAGMA user_version = 4;";

#[cfg(test)]
mod tests {
    use super::*;

    /// A rollup age no test record reaches.
    const NEVER: u32 = 100_000;

    fn store() -> SqliteStorage {
        SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), NEVER).unwrap()
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
            aggregate: false,
        }
    }

    fn agg(day: &str, account: &str) -> UsageRecord {
        let mut r = rec(&format!("{day}T01:00:00Z"), 1, Some(account));
        r.aggregate = true;
        r.message_id = types::aggregate_id(&r);
        r.request_id = AGG_MARKER.into();
        r
    }

    /// An aggregate as an older build wrote it, with a `:`-joined id.
    fn legacy_agg(day: &str, account: &str) -> UsageRecord {
        UsageRecord {
            message_id: format!("tku-agg:s1:{day}:opus:{account}:false"),
            request_id: AGG_MARKER.into(),
            ..rec(&format!("{day}T01:00:00Z"), 1, Some(account))
        }
    }

    fn count(s: &SqliteStorage, sql: &str) -> i64 {
        s.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn file_id(s: &SqliteStorage, path: &str) -> i64 {
        s.conn
            .query_row(
                "SELECT file_id FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn seal(s: &SqliteStorage, file_id: i64, r: &UsageRecord) {
        let fp = crate::dedup::stable_fingerprint(r.provider, &r.message_id, &r.request_id);
        s.conn
            .execute(
                "INSERT INTO sealed (file_id, fp) VALUES (?1, ?2)",
                params![file_id, fp_to_sql(fp)],
            )
            .unwrap();
    }

    /// A schema-4 database as the previous build created it, with one plain row
    /// and one legacy aggregate row.
    fn v4_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V4_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO files VALUES (1, 'claude', '/proj/a.jsonl', 10, 20);
             INSERT INTO records VALUES
                 (1, 's1', '2020-01-01T01:00:00+00:00', 'p', 'opus', 'msg-1', 'req-1',
                  1, 2, 0, 0, 0, 0, 'acct'),
                 (1, 's1', '2020-01-02T01:00:00+00:00', 'p', 'opus',
                  'tku-agg:s1:2020-01-02:opus:acct:false', 'tku-agg', 1, 3, 0, 0, 0, 0, 'acct');",
        )
        .unwrap();
        conn
    }

    fn assert_v5(conn: &Connection) {
        for (table, column) in [
            ("records", "aggregate"),
            ("files", "format"),
            ("files", "newest_ms"),
            ("files", "rolled"),
            ("sealed", "fp"),
        ] {
            assert!(has_column(conn, table, column).unwrap(), "{table}.{column}");
        }
        for index in ["idx_sealed_fp", "idx_records_message"] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "{index}");
        }
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn v4_database_is_upgraded_in_place() {
        let s = SqliteStorage::with_connection(v4_conn(), NEVER).unwrap();
        assert_v5(&s.conn);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM records"), 2);
        assert_eq!(count(&s, "SELECT format FROM files"), 1);

        let mut s = s;
        let mut records = s.drain_all();
        records.sort_by_key(|r| r.timestamp);
        assert_eq!(records.len(), 2);
        assert!(!records[0].aggregate);
        assert_eq!(records[0].message_id, "msg-1");
        assert!(records[1].aggregate);
        assert_eq!(records[1].message_id, types::aggregate_id(&records[1]));
    }

    #[test]
    fn interrupted_upgrade_reruns() {
        let conn = v4_conn();
        conn.execute_batch("ALTER TABLE records ADD COLUMN aggregate INTEGER NOT NULL DEFAULT 0;")
            .unwrap();
        let s = SqliteStorage::with_connection(conn, NEVER).unwrap();
        assert_v5(&s.conn);

        // An older build resets the version but keeps the columns.
        s.conn.pragma_update(None, "user_version", 4).unwrap();
        let s = SqliteStorage::with_connection(s.conn, NEVER).unwrap();
        assert_v5(&s.conn);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM records"), 2);
    }

    #[test]
    fn insert_writes_format_and_newest_ms() {
        let mut s = store();
        s.insert(
            "claude",
            Path::new("/proj/a.jsonl"),
            0,
            0,
            vec![
                rec("2020-01-02T00:00:00.250Z", 1, None),
                rec("2020-01-01T00:00:00Z", 2, None),
            ],
        );
        s.insert("claude", Path::new("/proj/empty.jsonl"), 0, 0, Vec::new());
        let row: (i64, Option<i64>, i64) = s
            .conn
            .query_row(
                "SELECT format, newest_ms, rolled FROM files WHERE path = '/proj/a.jsonl'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let newest: chrono::DateTime<chrono::Utc> = "2020-01-02T00:00:00.250Z".parse().unwrap();
        assert_eq!(row, (FILE_FORMAT, Some(newest.timestamp_millis()), 0));
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM files WHERE path = '/proj/empty.jsonl' AND newest_ms IS NULL"
            ),
            1
        );
    }

    #[test]
    fn reinsert_drops_the_files_sealed_rows() {
        let mut s = store();
        let r = rec("2020-01-01T00:00:00Z", 1, None);
        s.insert("claude", Path::new("/proj/a.jsonl"), 0, 0, vec![r.clone()]);
        s.insert("claude", Path::new("/proj/b.jsonl"), 0, 0, vec![r.clone()]);
        seal(&s, file_id(&s, "/proj/a.jsonl"), &r);
        seal(
            &s,
            file_id(&s, "/proj/b.jsonl"),
            &rec("2020-01-02T00:00:00Z", 1, None),
        );

        s.insert("claude", Path::new("/proj/a.jsonl"), 1, 0, vec![r]);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sealed"), 1);
        assert_eq!(
            count(&s, "SELECT file_id FROM sealed"),
            file_id(&s, "/proj/b.jsonl")
        );
    }

    #[test]
    fn orphaned_sealed_rows_are_removed() {
        let mut s = store();
        let r = rec("2020-01-01T00:00:00Z", 1, None);
        s.insert("claude", Path::new("/proj/a.jsonl"), 0, 0, vec![r.clone()]);
        let a = file_id(&s, "/proj/a.jsonl");
        seal(&s, a, &r);
        seal(&s, a + 1, &r);
        seal(&s, a + 2, &r);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sealed"), 3);

        // The next file takes over `a + 1`, which must not inherit its seals.
        s.insert("claude", Path::new("/proj/b.jsonl"), 0, 0, vec![r.clone()]);
        assert_eq!(file_id(&s, "/proj/b.jsonl"), a + 1);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sealed"), 2);

        // Without a file to roll up, the flush leaves them.
        assert!(s.flush());
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sealed"), 2);

        s.rollup_days = 30;
        assert!(s.flush());
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sealed"), 1);
        assert_eq!(count(&s, "SELECT file_id FROM sealed"), a);
    }

    #[test]
    fn drain_all_drops_records_sealed_in_another_file() {
        let mut s = store();
        let shared = rec("2020-01-01T00:00:00Z", 1, None);
        let keyless = UsageRecord {
            message_id: String::new(),
            request_id: String::new(),
            ..rec("2020-01-01T02:00:00Z", 3, None)
        };
        let mut sealed_agg = agg("2020-01-01", "acct");
        sealed_agg.output_tokens = 4;
        s.insert(
            "claude",
            Path::new("/proj/a.jsonl"),
            0,
            0,
            vec![shared.clone()],
        );
        s.insert(
            "claude",
            Path::new("/proj/b.jsonl"),
            0,
            0,
            vec![
                shared.clone(),
                rec("2020-01-01T01:00:00Z", 2, None),
                keyless.clone(),
                sealed_agg.clone(),
            ],
        );
        let a = file_id(&s, "/proj/a.jsonl");
        seal(&s, a, &shared);
        seal(&s, a, &keyless);
        seal(&s, a, &sealed_agg);

        let outs: Vec<u64> = s.drain_all().iter().map(|r| r.output_tokens).collect();
        assert_eq!(outs, [1, 2, 3, 4]);
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
    fn insert_max_merges_duplicates_within_a_file() {
        let mut s = store();
        let line = |out| UsageRecord {
            message_id: "msg-1".into(),
            request_id: "req-1".into(),
            ..rec("2020-01-01T00:00:00Z", out, None)
        };
        s.insert(
            "claude",
            Path::new("/proj/a.jsonl"),
            0,
            0,
            vec![line(6), line(6), line(410)],
        );
        let records = s.drain_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].output_tokens, 410);
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
                vec![agg("2020-01-01", "old"), legacy_agg("2020-01-02", "old")],
            );

            s.rekey_account("old", "new", None, None);

            let mut records = s.drain_all();
            records.sort_by_key(|r| r.timestamp);
            let ids: Vec<&str> = records.iter().map(|r| r.message_id.as_str()).collect();
            assert_eq!(
                ids,
                [
                    "tku-agg:s1\u{1f}2020-01-01\u{1f}p\u{1f}opus\u{1f}new\u{1f}false",
                    "tku-agg:s1\u{1f}2020-01-02\u{1f}p\u{1f}opus\u{1f}new\u{1f}false",
                ]
            );
            for r in &records {
                assert_eq!(r.account_uuid.as_deref(), Some("new"));
                assert_eq!(r.request_id, AGG_MARKER);
                assert!(r.aggregate);
            }
            assert_eq!(
                count(&s, "SELECT COUNT(*) FROM records WHERE aggregate = 1"),
                2
            );
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
                types::aggregate_id(&agg("2020-01-01", "other"))
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

    fn msg(id: &str, ts: &str, out: u64) -> UsageRecord {
        UsageRecord {
            message_id: id.into(),
            request_id: format!("req-{id}"),
            ..rec(ts, out, None)
        }
    }

    /// Every file's rolled flag and newest_ms, rows and seals, in order.
    fn dump(s: &SqliteStorage) -> Vec<String> {
        let query = |sql: &str| -> Vec<String> {
            let mut stmt = s.conn.prepare(sql).unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let mut out = query(
            "SELECT path || ' rolled=' || rolled || ' newest=' || newest_ms FROM files ORDER BY path",
        );
        out.extend(query(
            "SELECT f.path || ' ' || r.message_id || ' ' || r.timestamp || ' ' || r.output_tokens
                    || ' agg=' || r.aggregate
               FROM records r JOIN files f ON f.file_id = r.file_id ORDER BY f.path, r.rowid",
        ));
        out.extend(query(
            "SELECT f.path || ' sealed ' || s.fp
               FROM sealed s JOIN files f ON f.file_id = s.file_id ORDER BY f.path, s.fp",
        ));
        out
    }

    fn rolled(s: &SqliteStorage, path: &str) -> bool {
        s.conn
            .query_row(
                "SELECT rolled FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn sealed_fps(s: &SqliteStorage, path: &str) -> Vec<u64> {
        let mut stmt = s
            .conn
            .prepare("SELECT fp FROM sealed WHERE file_id = ?1 ORDER BY fp")
            .unwrap();
        let mut fps: Vec<u64> = stmt
            .query_map(params![file_id(s, path)], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|fp| fp.unwrap().cast_unsigned())
            .collect();
        fps.sort_unstable();
        fps
    }

    fn fp(r: &UsageRecord) -> u64 {
        crate::dedup::stable_fingerprint(r.provider, &r.message_id, &r.request_id)
    }

    #[test]
    fn flush_rolls_up_old_files_once() {
        let mut s =
            SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), 30).unwrap();
        let recent = (Utc::now() - Duration::days(1)).to_rfc3339();
        let old = "2020-01-01T01:00:00Z";
        s.insert(
            "claude",
            Path::new("/a.jsonl"),
            0,
            0,
            vec![
                msg("m1", old, 6),
                msg("m1", "2020-01-01T01:00:01Z", 410),
                msg("m2", "2020-01-01T02:00:00Z", 100),
            ],
        );
        s.insert(
            "claude",
            Path::new("/b.jsonl"),
            0,
            0,
            vec![
                msg("m2", "2020-01-01T02:00:00Z", 100),
                msg("m3", "2020-01-02T01:00:00Z", 7),
            ],
        );
        // A live file holding the most complete copy of an old message.
        s.insert(
            "claude",
            Path::new("/live.jsonl"),
            0,
            0,
            vec![msg("m1", old, 500), msg("m9", &recent, 1)],
        );
        assert!(s.flush());

        assert!(rolled(&s, "/a.jsonl") && rolled(&s, "/b.jsonl"));
        assert!(!rolled(&s, "/live.jsonl"));
        let mut expected_a = vec![fp(&msg("m1", old, 0)), fp(&msg("m2", old, 0))];
        expected_a.sort_unstable();
        assert_eq!(sealed_fps(&s, "/a.jsonl"), expected_a);
        assert_eq!(sealed_fps(&s, "/b.jsonl"), [fp(&msg("m3", old, 0))]);
        assert!(sealed_fps(&s, "/live.jsonl").is_empty());
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM records r JOIN files f ON f.file_id = r.file_id
                  WHERE f.path = '/a.jsonl' AND r.aggregate = 0"
            ),
            0
        );

        let before = dump(&s);
        assert!(s.flush());
        assert_eq!(dump(&s), before);

        let outs: Vec<u64> = s.drain_all().iter().map(|r| r.output_tokens).collect();
        // a: one aggregate of m1 (raised to the live copy) and m2; b: m3's aggregate,
        // its raw m2 hidden; live: m9, its sealed m1 copy hidden.
        assert_eq!(outs, [600, 7, 1]);
    }

    #[test]
    fn a_file_left_unchanged_by_the_rollup_is_marked_rolled() {
        let mut s =
            SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), 30).unwrap();
        let m1 = msg("m1", "2020-01-01T01:00:00Z", 5);
        s.insert("claude", Path::new("/a.jsonl"), 0, 0, vec![m1.clone()]);
        assert!(s.flush());
        s.insert("claude", Path::new("/b.jsonl"), 0, 0, vec![m1.clone()]);
        s.insert(
            "claude",
            Path::new("/c.jsonl"),
            0,
            0,
            vec![agg("2020-01-03", "acct")],
        );
        let before = dump(&s);

        assert!(s.flush());
        assert!(rolled(&s, "/b.jsonl") && rolled(&s, "/c.jsonl"));
        let changed: Vec<String> = dump(&s)
            .into_iter()
            .filter(|line| !before.contains(line))
            .collect();
        assert_eq!(
            changed,
            [
                format!(
                    "/b.jsonl rolled=1 newest={}",
                    m1.timestamp.timestamp_millis()
                ),
                format!(
                    "/c.jsonl rolled=1 newest={}",
                    agg("2020-01-03", "acct").timestamp.timestamp_millis()
                ),
            ]
        );

        let outs: Vec<u64> = s.drain_all().iter().map(|r| r.output_tokens).collect();
        assert_eq!(outs, [5, 1]);
    }

    #[test]
    fn a_reinserted_rolled_file_is_rolled_again() {
        let mut s =
            SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), 30).unwrap();
        let m1 = msg("m1", "2020-01-01T01:00:00Z", 5);
        s.insert("claude", Path::new("/a.jsonl"), 0, 0, vec![m1.clone()]);
        assert!(s.flush());
        s.insert("claude", Path::new("/a.jsonl"), 1, 0, vec![m1.clone()]);
        assert!(!rolled(&s, "/a.jsonl"));

        assert!(s.flush());
        assert!(rolled(&s, "/a.jsonl"));
        assert_eq!(sealed_fps(&s, "/a.jsonl"), [fp(&m1)]);
        let records = s.drain_all();
        assert_eq!(records.len(), 1);
        assert!(records[0].aggregate);
    }

    /// A file whose rows name a provider this build does not know.
    fn insert_unreadable(s: &SqliteStorage, path: &str, format: i64) -> i64 {
        s.conn
            .execute(
                "INSERT INTO files (provider, path, mtime_secs, size, format, newest_ms, rolled)
                 VALUES ('gone-provider', ?1, 0, 0, ?2, 0, 0)",
                params![path, format],
            )
            .unwrap();
        let id = s.conn.last_insert_rowid();
        s.conn
            .execute(
                "INSERT INTO records VALUES (?1, 's1', '2020-01-01T01:00:00+00:00', 'p', 'opus',
                                             'm1', 'r1', 1, 2, 0, 0, 0, 0, NULL, 0)",
                params![id],
            )
            .unwrap();
        id
    }

    #[test]
    fn an_unreadable_file_is_left_out_of_the_rollup() {
        let mut s =
            SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), 30).unwrap();
        s.insert(
            "claude",
            Path::new("/good.jsonl"),
            0,
            0,
            vec![msg("m1", "2020-01-01T01:00:00Z", 5)],
        );
        let bad = insert_unreadable(&s, "/bad.jsonl", FILE_FORMAT);
        let before = raw_rows(&s, bad);

        assert!(s.flush());
        assert!(rolled(&s, "/good.jsonl"));
        // Rolled, so the next flush does not select it again.
        assert!(rolled(&s, "/bad.jsonl"));
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM files WHERE rolled = 0 AND newest_ms IS NOT NULL"
            ),
            0
        );
        assert!(s.flush());
        assert_eq!(raw_rows(&s, bad), before);
    }

    /// Every column of a file's rows, as stored.
    fn raw_rows(s: &SqliteStorage, file_id: i64) -> Vec<Vec<rusqlite::types::Value>> {
        let mut stmt = s
            .conn
            .prepare("SELECT * FROM records WHERE file_id = ?1 ORDER BY rowid")
            .unwrap();
        let n = stmt.column_count();
        stmt.query_map(params![file_id], |row| {
            (0..n).map(|i| row.get(i)).collect::<rusqlite::Result<_>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[test]
    fn legacy_upgrade_skips_unreadable_files_and_drops_stale_seals() {
        super::super::with_scratch_home("sqlite-upgrade-skip", || {
            let mut s = store();
            let m1 = msg("m1", "2020-01-01T01:00:00Z", 5);
            s.insert(
                "claude",
                Path::new("/a.jsonl"),
                0,
                0,
                vec![m1.clone(), msg("m1", "2020-01-01T01:00:01Z", 9)],
            );
            let a = file_id(&s, "/a.jsonl");
            // An older build re-inserted the file under the id of a file it had rolled up.
            s.conn
                .execute("UPDATE files SET format = 1 WHERE file_id = ?1", params![a])
                .unwrap();
            seal(&s, a, &m1);
            let bad = insert_unreadable(&s, "/bad.jsonl", 1);
            seal(&s, bad, &m1);
            let before = raw_rows(&s, bad);

            let done = s.upgrade_legacy_files(&Progress::new(false)).unwrap();
            assert_eq!((done.upgraded, done.unreadable), (1, 1));
            assert!(sealed_fps(&s, "/a.jsonl").is_empty());
            assert!(sealed_fps(&s, "/bad.jsonl").is_empty());
            assert_eq!(
                count(&s, "SELECT format FROM files WHERE path = '/a.jsonl'"),
                FILE_FORMAT
            );
            // Nothing is left for the migration to do.
            assert_eq!(count(&s, "SELECT COUNT(*) FROM files WHERE format < 2"), 0);
            assert_eq!(raw_rows(&s, bad), before);
            let outs: Vec<u64> = s
                .drain_all()
                .iter()
                .filter(|r| r.provider == Provider::Claude)
                .map(|r| r.output_tokens)
                .collect();
            assert_eq!(outs, [9]);
        });
    }

    #[test]
    fn a_changed_peer_is_rolled_later_with_its_own_peers() {
        let mut s =
            SqliteStorage::with_connection(Connection::open_in_memory().unwrap(), 30).unwrap();
        let recent = (Utc::now() - Duration::days(1)).to_rfc3339();
        let old = "2020-01-01T01:00:00Z";
        // An old file marked rolled whose m5 is sealed nowhere.
        s.insert(
            "claude",
            Path::new("/p.jsonl"),
            0,
            0,
            vec![msg("m1", old, 3), msg("m5", "2020-01-01T02:00:00Z", 6)],
        );
        s.conn
            .execute("UPDATE files SET rolled = 1 WHERE path = '/p.jsonl'", [])
            .unwrap();
        // The most complete copy of m5, not sharing a message with the candidate.
        s.insert(
            "claude",
            Path::new("/q.jsonl"),
            0,
            0,
            vec![
                msg("m5", "2020-01-01T02:00:00Z", 410),
                msg("m9", &recent, 1),
            ],
        );
        s.insert(
            "claude",
            Path::new("/c.jsonl"),
            0,
            0,
            vec![msg("m1", old, 3)],
        );

        assert!(s.flush());
        assert!(!rolled(&s, "/p.jsonl"));
        assert!(sealed_fps(&s, "/p.jsonl").is_empty());
        assert_eq!(sealed_fps(&s, "/c.jsonl"), [fp(&msg("m1", old, 0))]);

        assert!(s.flush());
        assert!(rolled(&s, "/p.jsonl"));
        assert_eq!(sealed_fps(&s, "/p.jsonl"), [fp(&msg("m5", old, 0))]);
        let outs: Vec<u64> = s.drain_all().iter().map(|r| r.output_tokens).collect();
        // c: m1; p: m5 raised to q's copy, its raw m1 hidden; q: m9, its m5 hidden.
        assert_eq!(outs, [3, 410, 1]);
    }
}
