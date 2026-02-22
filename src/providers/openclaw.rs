use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, HomeFallback, Provider,
    XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct OpenClawProvider;

impl Provider for OpenClawProvider {
    fn name(&self) -> &str {
        "openclaw"
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
        None,
        &[],
        &[
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".openclaw", "agents"],
            },
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".clawdbot", "agents"],
            },
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".moltbot", "agents"],
            },
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".moldbot", "agents"],
            },
        ],
    )
}

/// Session ID: filename stem
fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Project: parent directory name under agents/
fn project_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    if let Some(idx) = path_str.find("/agents/") {
        let after = &path_str[idx + "/agents/".len()..];
        if let Some(slash) = after.find('/') {
            let dir = &after[..slash];
            if !dir.is_empty() {
                return dir.to_string();
            }
        }
    }
    "openclaw".to_string()
}

/// Stateful JSONL parser: track model via model_change entries,
/// extract tokens from assistant message entries.
fn parse_jsonl_file(path: &Path, session_id: &str, project: &str) -> Vec<UsageRecord> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut current_model = String::from("unknown");

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.contains("\"model_change\"") {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(model) = parsed.get("model").and_then(|v| v.as_str()) {
                    current_model = model.to_string();
                }
            }
            continue;
        }

        if !line.contains("\"message\"") || !line.contains("\"assistant\"") {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(record) = extract_message(&parsed, session_id, project, &current_model) {
            records.push(record);
        }
    }

    records
}

fn extract_message(
    parsed: &serde_json::Value,
    session_id: &str,
    project: &str,
    current_model: &str,
) -> Option<UsageRecord> {
    let message = parsed.get("message")?;

    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "assistant" {
        return None;
    }

    let usage = message.get("usage")?;

    let input = usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);

    if input == 0 && output == 0 {
        return None;
    }

    let cache_read = usage.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_write = usage
        .get("cacheWrite")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let model = message
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(current_model)
        .to_string();

    let timestamp_ms = message.get("timestamp").and_then(|v| v.as_i64())?;
    let timestamp: DateTime<Utc> = Utc.timestamp_millis_opt(timestamp_ms).single()?;

    let message_id = format!("openclaw:{session_id}:{timestamp}:{input}:{output}");

    Some(UsageRecord {
        provider: "openclaw".to_string(),
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
