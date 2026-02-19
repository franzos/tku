use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, parse_jsonl_lines,
    HomeFallback, Provider, XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct PiProvider;

impl Provider for PiProvider {
    fn name(&self) -> &str {
        "pi"
    }

    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
    ) {
        let roots = compute_roots();
        let files = discover_files(&roots, "jsonl");
        discover_and_parse_with(self.name(), files, storage, progress, |path| {
            let session_id = session_id_from_path(path);
            let project = project_from_path(path);
            parse_jsonl_file(path, &session_id, &project)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("PI_AGENT_DIR"),
        &["sessions"],
        &[
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".pi", "agent", "sessions"],
            },
            HomeFallback {
                base: XdgBase::Config,
                subpaths: &["pi", "agent", "sessions"],
            },
        ],
    )
}

/// Session ID: filename after first `_`, strip `.jsonl`
/// e.g. `2025-12-19T08-12-33-794Z_uuid.jsonl` -> `uuid`
fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    if let Some(idx) = stem.find('_') {
        let after = &stem[idx + 1..];
        if !after.is_empty() {
            return after.to_string();
        }
    }

    stem.to_string()
}

/// Project: parent directory name under sessions/
fn project_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    if let Some(idx) = path_str.find("/sessions/") {
        let after = &path_str[idx + "/sessions/".len()..];
        if let Some(slash) = after.find('/') {
            let dir = &after[..slash];
            if !dir.is_empty() {
                return dir.to_string();
            }
        }
    }
    "pi".to_string()
}

fn parse_jsonl_file(path: &Path, session_id: &str, project: &str) -> Vec<UsageRecord> {
    parse_jsonl_lines(path, "\"assistant\"", |line: &str| {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        extract_record(&parsed, session_id, project)
    })
}

fn extract_record(
    parsed: &serde_json::Value,
    session_id: &str,
    project: &str,
) -> Option<UsageRecord> {
    let timestamp_str = parsed.get("timestamp").and_then(|v| v.as_str())?;
    let timestamp: DateTime<Utc> = timestamp_str.parse().ok()?;

    let message = parsed.get("message")?;

    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "assistant" {
        return None;
    }

    let usage = message.get("usage")?;

    // Both input and output must be present and numeric
    let input = usage.get("input").and_then(|v| v.as_u64())?;
    let output = usage.get("output").and_then(|v| v.as_u64())?;

    let cache_read = usage.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_write = usage
        .get("cacheWrite")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let model = message
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let message_id = format!("pi:{session_id}:{timestamp}:{input}:{output}");

    Some(UsageRecord {
        provider: "pi".to_string(),
        session_id: session_id.to_string(),
        timestamp,
        project: project.to_string(),
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cache_read,
    })
}
