//! One-way import from a sqlite cache into the bitcode cache.
//!
//! The two backends are selected at compile time and never share a file, so a
//! build that switches between them stops seeing the other store's records —
//! which, for sessions whose transcripts the tool has since deleted, is the
//! only copy. This copies them across.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use super::bitcode_store::BitcodeStorage;
use super::Storage;
use crate::types::{self, Provider, UsageRecord};

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
    aggregate: bool,
    /// The `sealed` table exists.
    sealed: bool,
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
    let mut sealed = if columns.sealed {
        read_sealed(&conn)?
    } else {
        HashMap::new()
    };
    let files = read_files(&conn)?;

    let mut store = BitcodeStorage::new();
    let mut report = ImportReport {
        files: 0,
        records: 0,
        skipped_files: 0,
        range: None,
    };

    for f in &files {
        let carries_seals = sealed.get(&f.id).is_some_and(|s| !s.is_empty())
            && by_file.get(&f.id).is_some_and(|r| !r.is_empty());
        if carries_seals && store.is_legacy(&f.provider) {
            bail!(
                "the {} record cache is still in the legacy format, which cannot hold this \
                 database's rollups; run tku once to upgrade it, then import again",
                f.provider
            );
        }
    }
    // Sealed in the destination before the import; a file sealing one of these again would count it twice.
    let mut sealed_before: HashMap<String, HashSet<u64>> = HashMap::new();

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
        let seals = sealed.remove(&id).unwrap_or_default();
        if !seals.is_empty() {
            let before = sealed_before
                .entry(provider.clone())
                .or_insert_with(|| store.sealed_fingerprints(&provider));
            if seals.iter().any(|fp| before.contains(fp)) {
                eprintln!(
                    "tku: not importing {path}: it rolls up messages the record cache already rolled up"
                );
                report.skipped_files += 1;
                continue;
            }
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
            store.insert_sealed(
                &provider,
                Path::new(&path),
                mtime,
                size,
                records.clone(),
                seals,
            );
        }
    }

    if !dry_run {
        store.force_write();
        if !store.flush() {
            bail!("record cache not written; nothing was imported");
        }
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
    let sealed = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sealed')",
            [],
            |row| row.get(0),
        )
        .context("failed to read the database schema")?;
    Ok(Columns {
        cache_creation_1h: names.iter().any(|n| n == "cache_creation_1h_input_tokens"),
        fast_mode: names.iter().any(|n| n == "fast_mode"),
        aggregate: names.iter().any(|n| n == "aggregate"),
        sealed,
    })
}

/// Sealed fingerprints per file id, sorted.
fn read_sealed(conn: &Connection) -> Result<HashMap<i64, Vec<u64>>> {
    let mut stmt = conn
        .prepare("SELECT file_id, fp FROM sealed ORDER BY file_id, fp")
        .context("failed to read the sealed table")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.cast_unsigned()))
        })
        .context("failed to read the sealed table")?;
    let mut out: HashMap<i64, Vec<u64>> = HashMap::new();
    for (file_id, fp) in rows.filter_map(|r| r.ok()) {
        out.entry(file_id).or_default().push(fp);
    }
    Ok(out)
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
    let aggregate = if columns.aggregate {
        "r.aggregate"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT r.file_id, f.provider, r.session_id, r.timestamp, r.project, r.model,
                r.message_id, r.request_id, r.input_tokens, r.output_tokens,
                r.cache_creation_input_tokens, {cache_1h}, r.cache_read_input_tokens,
                {fast}, r.account_uuid, {aggregate}
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
                    aggregate: row.get::<_, i64>(15)? != 0,
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
        // Rows an older build wrote carry the aggregate only in the id.
        types::adopt_legacy_aggregate(&mut record);
        out.entry(file_id).or_default().push(record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_writes_a_live_file() {
        super::super::with_scratch_home("import-live", || {
            let db =
                std::env::temp_dir().join(format!("tku-import-live-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&db);
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (file_id INTEGER PRIMARY KEY, provider TEXT, path TEXT,
                                     mtime_secs INTEGER, size INTEGER);
                 CREATE TABLE records (file_id INTEGER, session_id TEXT, timestamp TEXT,
                                       project TEXT, model TEXT, message_id TEXT, request_id TEXT,
                                       input_tokens INTEGER, output_tokens INTEGER,
                                       cache_creation_input_tokens INTEGER,
                                       cache_read_input_tokens INTEGER, account_uuid TEXT);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files VALUES (1, 'claude', '/live.jsonl', ?1, 10)",
                [Utc::now().timestamp()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO records VALUES (1, 's', '2020-01-01T01:00:00Z', 'p', 'opus',
                                             'm', 'r', 1, 2, 3, 4, NULL)",
                [],
            )
            .unwrap();
            drop(conn);

            let report = import_sqlite(&db, false).unwrap();
            std::fs::remove_file(&db).unwrap();
            assert_eq!((report.files, report.records), (1, 1));
            assert!(BitcodeStorage::new().contains("claude", Path::new("/live.jsonl")));
        });
    }

    /// Output tokens a report counts.
    fn reported(records: Vec<UsageRecord>) -> u64 {
        crate::dedup::dedup(records)
            .iter()
            .map(|r| r.output_tokens)
            .sum()
    }

    fn msg(id: &str, ts: &str, out: u64) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s1".into(),
            timestamp: ts.parse().unwrap(),
            project: "p".into(),
            model: "opus".into(),
            message_id: id.into(),
            request_id: format!("req-{id}"),
            input_tokens: 1,
            output_tokens: out,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: Some("acct".into()),
            aggregate: false,
        }
    }

    #[test]
    fn import_of_a_v4_database_counts_each_message_once() {
        super::super::with_scratch_home("import-v4", || {
            let db = crate::paths::data_dir().unwrap().join("v4.db");
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(super::super::sqlite_store::V4_SCHEMA)
                .unwrap();
            // Recent days, so the import's flush keeps the rows as stored.
            let day = |n: i64| (Utc::now() - chrono::Duration::days(n)).date_naive();
            let (d1, d2, d3) = (day(3), day(2), day(1));
            conn.execute_batch(&format!(
                "INSERT INTO files VALUES (1, 'claude', '/a.jsonl', 10, 1), (2, 'claude', '/b.jsonl', 10, 1);
                 INSERT INTO records VALUES
                     (1, 's1', '{d1}T01:00:00Z', 'p', 'opus', 'm1', 'req-m1', 1, 6, 0, 0, 0, 0, 'acct'),
                     (1, 's1', '{d1}T01:00:01Z', 'p', 'opus', 'm1', 'req-m1', 1, 6, 0, 0, 0, 0, 'acct'),
                     (1, 's1', '{d1}T01:00:02Z', 'p', 'opus', 'm1', 'req-m1', 1, 410, 0, 0, 0, 0, 'acct'),
                     (1, 's1', '{d1}T02:00:00Z', 'p', 'opus', 'm2', 'req-m2', 1, 100, 0, 0, 0, 0, 'acct'),
                     (1, 's1', '{d3}T01:00:00Z', 'p', 'opus',
                      'tku-agg:s1:{d3}:opus:acct:false', 'tku-agg', 1, 50, 0, 0, 0, 0, 'acct'),
                     (2, 's1', '{d1}T02:00:00Z', 'p', 'opus', 'm2', 'req-m2', 1, 100, 0, 0, 0, 0, 'acct'),
                     (2, 's1', '{d2}T01:00:00Z', 'p', 'opus', 'm3', 'req-m3', 1, 7, 0, 0, 0, 0, 'acct');"
            ))
            .unwrap();
            drop(conn);

            let report = import_sqlite(&db, false).unwrap();
            assert_eq!(report.files, 2);
            let records = BitcodeStorage::new().drain_all();
            let stored: Vec<(&str, u64, bool)> = records
                .iter()
                .map(|r| (r.message_id.as_str(), r.output_tokens, r.aggregate))
                .collect();
            let agg = &records[2];
            assert_eq!(agg.message_id, types::aggregate_id(agg));
            assert_eq!(
                stored,
                [
                    ("m1", 410, false),
                    ("m2", 100, false),
                    (agg.message_id.as_str(), 50, true),
                    ("m2", 100, false),
                    ("m3", 7, false),
                ]
            );
            assert_eq!(reported(records), 410 + 100 + 50 + 7);
        });
    }

    #[test]
    fn import_of_a_v5_database_keeps_its_sealed_sets() {
        super::super::with_scratch_home("import-v5", || {
            let mut sqlite = super::super::sqlite_store::SqliteStorage::open().unwrap();
            sqlite.insert(
                "claude",
                Path::new("/a.jsonl"),
                10,
                1,
                vec![
                    msg("m1", "2020-01-01T01:00:00Z", 410),
                    msg("m2", "2020-01-01T02:00:00Z", 100),
                ],
            );
            sqlite.insert(
                "claude",
                Path::new("/b.jsonl"),
                10,
                1,
                vec![
                    msg("m2", "2020-01-01T02:00:00Z", 100),
                    msg("m3", "2020-01-02T01:00:00Z", 7),
                ],
            );
            assert!(sqlite.flush());
            let expected = reported(sqlite.drain_all());
            assert_eq!(expected, 517);
            drop(sqlite);

            let db = crate::paths::sqlite_db_file().unwrap();
            let report = import_sqlite(&db, false).unwrap();
            assert_eq!(report.files, 2);
            let records = BitcodeStorage::new().drain_all();
            assert!(records.iter().all(|r| r.aggregate));
            assert_eq!(reported(records), expected);
        });
    }

    #[test]
    fn an_import_sealing_a_message_the_store_already_sealed_is_skipped() {
        super::super::with_scratch_home("import-double-seal", || {
            let m = || msg("m1", "2020-01-01T01:00:00Z", 100);
            let mut bitcode = BitcodeStorage::new();
            bitcode.insert("claude", Path::new("/b.jsonl"), 10, 1, vec![m()]);
            bitcode.force_write();
            assert!(bitcode.flush());
            assert_eq!(BitcodeStorage::new().sealed_fingerprints("claude").len(), 1);

            let mut sqlite = super::super::sqlite_store::SqliteStorage::open().unwrap();
            sqlite.insert("claude", Path::new("/a.jsonl"), 10, 1, vec![m()]);
            assert!(sqlite.flush());
            drop(sqlite);

            let db = crate::paths::sqlite_db_file().unwrap();
            let report = import_sqlite(&db, false).unwrap();
            assert_eq!((report.files, report.skipped_files), (0, 1));
            let mut bitcode = BitcodeStorage::new();
            assert!(!bitcode.contains("claude", Path::new("/a.jsonl")));
            assert_eq!(reported(bitcode.drain_all()), 100);
        });
    }

    #[test]
    fn import_of_seals_into_a_legacy_store_fails() {
        super::super::with_scratch_home("import-legacy-dest", || {
            let mut pc = super::super::bitcode_store::ProviderCache {
                legacy: true,
                ..Default::default()
            };
            pc.files.insert(
                "/x.jsonl".into(),
                super::super::bitcode_store::CachedFile {
                    mtime_secs: 10,
                    size: 1,
                    records: vec![msg("m9", "2020-01-05T01:00:00Z", 1)],
                    sealed: Vec::new(),
                },
            );
            let path = crate::paths::bitcode_cache_file("claude").unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let legacy = super::super::legacy::encode(&pc).unwrap();
            std::fs::write(&path, &legacy).unwrap();

            let mut sqlite = super::super::sqlite_store::SqliteStorage::open().unwrap();
            sqlite.insert(
                "claude",
                Path::new("/a.jsonl"),
                10,
                1,
                vec![msg("m1", "2020-01-01T01:00:00Z", 100)],
            );
            assert!(sqlite.flush());
            drop(sqlite);

            let db = crate::paths::sqlite_db_file().unwrap();
            let Err(err) = import_sqlite(&db, false) else {
                panic!("import into a legacy store succeeded");
            };
            assert!(err.to_string().contains("legacy format"), "{err:#}");
            assert_eq!(std::fs::read(&path).unwrap(), legacy);
        });
    }
}
