use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files_with, DiscoveredFile,
    HomeFallback, Provider as ProviderDriver, XdgBase,
};
use crate::storage::Storage;
use crate::types::{Provider, UsageRecord};

pub struct OpenCodeProvider;

impl ProviderDriver for OpenCodeProvider {
    fn id(&self) -> Provider {
        Provider::OpenCode
    }

    fn root_dirs(&self) -> Vec<PathBuf> {
        compute_roots()
    }

    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
        prune: bool,
    ) {
        let roots = compute_roots();

        // Single walk per storage root. We want the session/*.json to seed the
        // sessionID → project map, and the message/*.json as parse targets;
        // classify by parent dir name rather than walking both subtrees twice.
        let (session_files, message_files) = discover_session_and_message(&roots);
        let session_projects = build_session_projects(&session_files);

        // Parse SQLite db(s), collect all message IDs for dedup against JSON files
        let (sqlite_records, sqlite_db_paths) = collect_sqlite_records(&roots, &session_projects);
        let sqlite_ids: HashSet<String> = sqlite_records
            .iter()
            .map(|r| r.message_id.clone())
            .collect();

        // Insert SQLite records into storage (file-level caching via db path)
        #[cfg(feature = "sqlite")]
        for db_path in &sqlite_db_paths {
            if let Some(df) = super::discovered_file(db_path) {
                if !storage.is_cached(self.name(), db_path, df.mtime, df.size) {
                    let db_records = sqlite_records.clone();
                    storage.insert(self.name(), db_path, df.mtime, df.size, db_records);
                }
            }
        }

        let _ = &sqlite_db_paths; // suppress unused warning without sqlite

        #[allow(unused_mut)]
        let mut files = message_files;

        // Include db paths in the file list so prune doesn't remove them
        #[cfg(feature = "sqlite")]
        for db_path in &sqlite_db_paths {
            if let Some(df) = super::discovered_file(db_path) {
                files.push(df);
            }
        }

        discover_and_parse_with(self.name(), files, storage, progress, prune, |path| {
            // Skip db files in the parse phase — they're handled above
            if path.extension().is_some_and(|ext| ext == "db") {
                return Vec::new();
            }
            let records = parse_message_file(path, &session_projects);
            if sqlite_ids.is_empty() {
                records
            } else {
                records
                    .into_iter()
                    .filter(|r| !sqlite_ids.contains(&r.message_id))
                    .collect()
            }
        });
    }
}

/// Walk each storage root once and split `*.json` files into `session/` and
/// `message/` buckets by parent directory name.
fn discover_session_and_message(roots: &[PathBuf]) -> (Vec<DiscoveredFile>, Vec<DiscoveredFile>) {
    // We walk via discover_files_with so the size/mtime metadata pickup and the
    // follow_links(false) policy stay in one place.
    let mut session = Vec::new();
    let mut message = Vec::new();
    let all = discover_files_with(roots, |p| p.extension().is_some_and(|e| e == "json"));
    for df in all {
        match classify_storage_file(&df.path) {
            Some(StorageKind::Session) => session.push(df),
            Some(StorageKind::Message) => message.push(df),
            None => {}
        }
    }
    (session, message)
}

enum StorageKind {
    Session,
    Message,
}

/// Classify `.../<root>/session/...json` vs `.../<root>/message/...json`.
/// The opencode layout nests one level deep under each bucket (session IDs
/// live in subdirs), so we walk up ancestors until we hit `session` or
/// `message`.
fn classify_storage_file(path: &Path) -> Option<StorageKind> {
    for ancestor in path.ancestors() {
        match ancestor.file_name().and_then(|n| n.to_str()) {
            Some("session") => return Some(StorageKind::Session),
            Some("message") => return Some(StorageKind::Message),
            _ => {}
        }
    }
    None
}

fn build_session_projects(session_files: &[DiscoveredFile]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for f in session_files {
        if let Some((session_id, project)) = parse_session_file(&f.path) {
            map.insert(session_id, project);
        }
    }
    map
}

/// Always parse SQLite dbs to get records + paths (for dedup and prune).
#[cfg(feature = "sqlite")]
fn collect_sqlite_records(
    roots: &[PathBuf],
    session_projects: &HashMap<String, String>,
) -> (Vec<UsageRecord>, Vec<PathBuf>) {
    let mut all_records = Vec::new();
    let mut db_paths = Vec::new();

    for root in roots {
        let db_path = match root.parent() {
            Some(p) => p.join("opencode.db"),
            None => continue,
        };
        if !db_path.exists() {
            continue;
        }

        let records = parse_sqlite_db(&db_path, session_projects);
        if !records.is_empty() {
            all_records.extend(records);
            db_paths.push(db_path);
        }
    }

    (all_records, db_paths)
}

#[cfg(not(feature = "sqlite"))]
fn collect_sqlite_records(
    _roots: &[PathBuf],
    _session_projects: &HashMap<String, String>,
) -> (Vec<UsageRecord>, Vec<PathBuf>) {
    (Vec::new(), Vec::new())
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("OPENCODE_DATA_DIR"),
        &["storage"],
        &[HomeFallback {
            base: XdgBase::Data,
            subpaths: &["opencode", "storage"],
        }],
    )
}

fn parse_session_file(path: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;

    let session_id = parsed.get("id")?.as_str()?.to_string();

    // Use directory basename as project name, fall back to projectID
    let project = parsed
        .get("directory")
        .and_then(|v| v.as_str())
        .and_then(|d| d.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .or_else(|| parsed.get("projectID").and_then(|v| v.as_str()))
        .unwrap_or("opencode")
        .to_string();

    Some((session_id, project))
}

fn parse_message_file(path: &Path, session_projects: &HashMap<String, String>) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    match extract_record(&parsed, session_projects) {
        Some(record) => vec![record],
        None => Vec::new(),
    }
}

/// Parse a single opencode.db SQLite database (OpenCode 1.2+).
#[cfg(feature = "sqlite")]
fn parse_sqlite_db(db_path: &Path, session_projects: &HashMap<String, String>) -> Vec<UsageRecord> {
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut stmt = match conn.prepare(
        "SELECT id, session_id, data FROM message \
         WHERE json_extract(data, '$.role') = 'assistant' \
         AND json_extract(data, '$.tokens') IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = match stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let session_id: String = row.get(1)?;
        let data: String = row.get(2)?;
        Ok((id, session_id, data))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut records = Vec::new();

    for row in rows {
        let (id, session_id, data) = match row {
            Ok(r) => r,
            Err(_) => continue,
        };

        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(mut record) = extract_record_from_data(&parsed, &id, &session_id) {
            record.project = session_projects
                .get(&session_id)
                .cloned()
                .unwrap_or_else(|| "opencode".to_string());
            records.push(record);
        }
    }

    records
}

/// Extract a UsageRecord from SQLite row data (shared logic with JSON path).
#[cfg(feature = "sqlite")]
fn extract_record_from_data(
    parsed: &serde_json::Value,
    message_id: &str,
    session_id: &str,
) -> Option<UsageRecord> {
    let model = parsed.get("modelID").and_then(|v| v.as_str())?.to_string();

    let time = parsed.get("time")?;
    let created_ms = time.get("created").and_then(|v| v.as_i64())?;
    let timestamp: DateTime<Utc> = Utc.timestamp_millis_opt(created_ms).single()?;

    let tokens = parsed.get("tokens")?;
    let input = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);

    let cache_read = tokens
        .get("cache")
        .and_then(|c| c.get("read"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_write = tokens
        .get("cache")
        .and_then(|c| c.get("write"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if input == 0 && output == 0 {
        return None;
    }

    Some(UsageRecord {
        provider: Provider::OpenCode,
        session_id: session_id.to_string(),
        timestamp,
        project: String::new(), // filled in by caller
        model,
        message_id: message_id.to_string(),
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cache_read,
        account_uuid: None,
    })
}

fn extract_record(
    parsed: &serde_json::Value,
    session_projects: &HashMap<String, String>,
) -> Option<UsageRecord> {
    // Required fields
    let message_id = parsed.get("id")?.as_str()?.to_string();
    let _provider_id = parsed.get("providerID").and_then(|v| v.as_str())?;
    let model = parsed.get("modelID").and_then(|v| v.as_str())?.to_string();

    let session_id = parsed
        .get("sessionID")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Timestamp from time.created (milliseconds)
    let time = parsed.get("time")?;
    let created_ms = time.get("created").and_then(|v| v.as_i64())?;
    let timestamp: DateTime<Utc> = Utc.timestamp_millis_opt(created_ms).single()?;

    // Tokens
    let tokens = parsed.get("tokens")?;
    let input = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);

    let cache_read = tokens
        .get("cache")
        .and_then(|c| c.get("read"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_write = tokens
        .get("cache")
        .and_then(|c| c.get("write"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Skip zero-token messages
    if input == 0 && output == 0 {
        return None;
    }

    let project = session_projects
        .get(&session_id)
        .cloned()
        .unwrap_or_else(|| "opencode".to_string());

    Some(UsageRecord {
        provider: Provider::OpenCode,
        session_id,
        timestamp,
        project,
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cache_read,
        account_uuid: None,
    })
}
