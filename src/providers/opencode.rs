use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, HomeFallback, Provider,
    XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct OpenCodeProvider;

impl Provider for OpenCodeProvider {
    fn name(&self) -> &str {
        "opencode"
    }

    fn root_dirs(&self) -> Vec<PathBuf> {
        compute_roots()
    }

    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
    ) {
        let roots = compute_roots();

        // Pre-load session metadata for project resolution
        let session_projects = load_session_projects(&roots);

        let message_roots: Vec<PathBuf> = roots.iter().map(|r| r.join("message")).collect();
        let files = discover_files(&message_roots, "json");

        discover_and_parse_with(self.name(), files, storage, progress, |path| {
            parse_message_file(path, &session_projects)
        });
    }
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

/// Load all session files and build a sessionID -> project name map.
fn load_session_projects(storage_roots: &[PathBuf]) -> HashMap<String, String> {
    let mut map = HashMap::new();

    for root in storage_roots {
        let session_dir = root.join("session");
        if !session_dir.exists() {
            continue;
        }

        let files = discover_files(&[session_dir], "json");
        for file in files {
            if let Some((session_id, project)) = parse_session_file(&file.path) {
                map.insert(session_id, project);
            }
        }
    }

    map
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
        provider: "opencode".to_string(),
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
    })
}
