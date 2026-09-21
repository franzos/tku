//! One-way import from a sqlite cache into the bitcode cache.
//!
//! The two backends are selected at compile time and never share a file, so a
//! build that switches between them stops seeing the other store's records —
//! which, for sessions whose transcripts the tool has since deleted, is the
//! only copy. This copies them across.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use super::bitcode_store::BitcodeStorage;
use super::Storage;
use crate::types::{Provider, UsageRecord};

pub struct ImportReport {
    pub files: usize,
    pub records: usize,
    pub skipped_files: usize,
    pub range: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Columns added after the first schema; absent from older databases, which
/// are exactly the ones worth importing.
struct Columns {
    cache_creation_1h: bool,
    fast_mode: bool,
}

/// One row of the `files` table: id, provider, path, mtime, size.
struct CachedFileRow {
    id: i64,
    provider: String,
    path: String,
    mtime: i64,
    size: u64,
}

pub fn import_sqlite(from: &Path, dry_run: bool) -> Result<ImportReport> {
    if !from.exists() {
        bail!("no sqlite cache at {}", from.display());
    }

    // Read-only, and deliberately not through `SqliteStorage::open`: that runs
    // a migration which drops both tables when the schema predates the current
    // version, destroying the very records we are here to rescue.
    let conn = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {} read-only", from.display()))?;

    let columns = detect_columns(&conn)?;
    let by_file = read_records(&conn, &columns)?;
    let files = read_files(&conn)?;

    let mut store = BitcodeStorage::new();
    let mut report = ImportReport {
        files: 0,
        records: 0,
        skipped_files: 0,
        range: None,
    };

    for CachedFileRow {
        id,
        provider,
        path,
        mtime,
        size,
    } in files
    {
        let Some(records) = by_file.get(&id) else {
            continue;
        };
        if records.is_empty() {
            continue;
        }
        // A path the bitcode cache already knows wins: its entry either matches
        // a live file or is a later parse of one, both fresher than this.
        if store.contains(&provider, Path::new(&path)) {
            report.skipped_files += 1;
            continue;
        }
        for r in records {
            report.range = Some(match report.range {
                Some((lo, hi)) => (lo.min(r.timestamp), hi.max(r.timestamp)),
                None => (r.timestamp, r.timestamp),
            });
        }
        report.files += 1;
        report.records += records.len();
        if !dry_run {
            store.insert(&provider, Path::new(&path), mtime, size, records.clone());
        }
    }

    if !dry_run {
        store.flush();
    }
    Ok(report)
}

fn detect_columns(conn: &Connection) -> Result<Columns> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('records')")
        .context("failed to read the records schema")?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .context("failed to read the records schema")?
        .filter_map(|r| r.ok())
        .collect();
    if names.is_empty() {
        bail!("no `records` table — is this a tku sqlite cache?");
    }
    Ok(Columns {
        cache_creation_1h: names.iter().any(|n| n == "cache_creation_1h_input_tokens"),
        fast_mode: names.iter().any(|n| n == "fast_mode"),
    })
}

fn read_files(conn: &Connection) -> Result<Vec<CachedFileRow>> {
    let mut stmt = conn
        .prepare("SELECT file_id, provider, path, mtime_secs, size FROM files")
        .context("failed to read the files table")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CachedFileRow {
                id: row.get(0)?,
                provider: row.get(1)?,
                path: row.get(2)?,
                mtime: row.get(3)?,
                size: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .context("failed to read the files table")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn read_records(conn: &Connection, columns: &Columns) -> Result<HashMap<i64, Vec<UsageRecord>>> {
    let cache_1h = if columns.cache_creation_1h {
        "r.cache_creation_1h_input_tokens"
    } else {
        "0"
    };
    let fast = if columns.fast_mode {
        "r.fast_mode"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT r.file_id, f.provider, r.session_id, r.timestamp, r.project, r.model,
                r.message_id, r.request_id, r.input_tokens, r.output_tokens,
                r.cache_creation_input_tokens, {cache_1h}, r.cache_read_input_tokens,
                {fast}, r.account_uuid
           FROM records r
           JOIN files f ON r.file_id = f.file_id"
    );

    let mut stmt = conn.prepare(&sql).context("failed to read records")?;
    let rows = stmt
        .query_map([], |row| {
            let provider_str: String = row.get(1)?;
            let ts: String = row.get(3)?;
            Ok((
                row.get::<_, i64>(0)?,
                provider_str,
                ts,
                UsageRecord {
                    // Placeholder; replaced below once provider/timestamp parse.
                    provider: Provider::Claude,
                    session_id: row.get(2)?,
                    timestamp: Utc::now(),
                    project: row.get(4)?,
                    model: row.get(5)?,
                    message_id: row.get(6)?,
                    request_id: row.get(7)?,
                    input_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(9)?.max(0) as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(10)?.max(0) as u64,
                    cache_creation_1h_input_tokens: row.get::<_, i64>(11)?.max(0) as u64,
                    cache_read_input_tokens: row.get::<_, i64>(12)?.max(0) as u64,
                    fast_mode: row.get::<_, i64>(13)? != 0,
                    account_uuid: row.get::<_, Option<String>>(14)?,
                },
            ))
        })
        .context("failed to read records")?;

    let mut out: HashMap<i64, Vec<UsageRecord>> = HashMap::new();
    for row in rows.filter_map(|r| r.ok()) {
        let (file_id, provider_str, ts, mut record) = row;
        let (Ok(provider), Ok(timestamp)) = (
            Provider::from_str(&provider_str),
            ts.parse::<DateTime<Utc>>(),
        ) else {
            continue;
        };
        record.provider = provider;
        record.timestamp = timestamp;
        out.entry(file_id).or_default().push(record);
    }
    Ok(out)
}
