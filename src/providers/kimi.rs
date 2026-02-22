use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, parse_jsonl_lines,
    HomeFallback, Provider, XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct KimiProvider;

impl Provider for KimiProvider {
    fn name(&self) -> &str {
        "kimi"
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
        let config_model = read_config_model();
        let files = discover_files(&roots, "jsonl");
        discover_and_parse_with(self.name(), files, storage, progress, |path| {
            let session_id = session_id_from_path(path);
            let project = project_from_path(path);
            parse_wire_file(path, &session_id, &project, &config_model)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("KIMI_HOME"),
        &["sessions"],
        &[HomeFallback {
            base: XdgBase::Home,
            subpaths: &[".kimi", "sessions"],
        }],
    )
}

/// Read model from ~/.kimi/config.json
fn read_config_model() -> String {
    let config_path = std::env::var("KIMI_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".kimi")))
        .map(|p| p.join("config.json"));

    if let Some(path) = config_path {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(model) = parsed.get("model").and_then(|v| v.as_str()) {
                    return model.to_string();
                }
            }
        }
    }

    "kimi-for-coding".to_string()
}

/// Session ID: parent directory name (UUID) from wire.jsonl path
fn session_id_from_path(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Project: grandparent directory (GROUP_ID) from wire.jsonl path
fn project_from_path(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty() && *s != "sessions")
        .unwrap_or("kimi")
        .to_string()
}

fn parse_wire_file(
    path: &Path,
    session_id: &str,
    project: &str,
    config_model: &str,
) -> Vec<UsageRecord> {
    parse_jsonl_lines(path, "token_usage", |line: &str| {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        extract_record(&parsed, session_id, project, config_model)
    })
}

fn extract_record(
    parsed: &serde_json::Value,
    session_id: &str,
    project: &str,
    config_model: &str,
) -> Option<UsageRecord> {
    // Skip metadata lines
    let line_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if line_type == "metadata" {
        return None;
    }

    // Must be a StatusUpdate message with token_usage
    let message = parsed.get("message")?;
    let msg_type = message.get("type").and_then(|v| v.as_str())?;
    if msg_type != "StatusUpdate" {
        return None;
    }

    let payload = message.get("payload")?;
    let token_usage = payload.get("token_usage")?;

    let input = token_usage
        .get("input_other")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = token_usage
        .get("output")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = token_usage
        .get("input_cache_read")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation = token_usage
        .get("input_cache_creation")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if input == 0 && output == 0 {
        return None;
    }

    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(config_model)
        .to_string();

    // Timestamp: float seconds
    let ts_secs = parsed.get("timestamp").and_then(|v| v.as_f64())?;
    let ts_millis = (ts_secs * 1000.0) as i64;
    let timestamp: DateTime<Utc> = Utc.timestamp_millis_opt(ts_millis).single()?;

    // Message ID: prefer payload.message_id, fall back to composite
    let message_id = payload
        .get("message_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("kimi:{session_id}:{timestamp}:{input}:{output}"));

    Some(UsageRecord {
        provider: "kimi".to_string(),
        session_id: session_id.to_string(),
        timestamp,
        project: project.to_string(),
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
    })
}
