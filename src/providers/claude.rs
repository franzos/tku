use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, parse_jsonl_lines,
    HomeFallback, Provider, XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct ClaudeProvider;

impl Provider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
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
            parse_jsonl_file(path)
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
                subpaths: &[".claude", "projects"],
            },
            HomeFallback {
                base: XdgBase::Config,
                subpaths: &["claude", "projects"],
            },
        ],
    )
}

fn parse_jsonl_file(path: &std::path::Path) -> Vec<UsageRecord> {
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let project = extract_project_from_path(path);

    parse_jsonl_lines(path, "\"type\":", |line: &str| {
        // Pre-filter: skip lines that can't contain usage data
        if !line.contains("\"type\":\"assistant\"") && !line.contains("\"type\":\"progress\"") {
            return None;
        }

        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        let line_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match line_type {
            "assistant" | "progress" => extract_record(&parsed, &session_id, &project),
            _ => None,
        }
    })
}

fn extract_project_from_path(path: &std::path::Path) -> String {
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Some(parent) = dir.parent() {
            if parent.file_name().is_some_and(|n| n == "projects") {
                let name = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                return extract_project_name(name);
            }
        }
        current = dir.parent();
    }

    "unknown".to_string()
}

/// Extract a meaningful project name from a Claude projects folder name.
/// Format is like "-home-franz-git-foo-bar" -> "foo-bar"
fn extract_project_name(encoded: &str) -> String {
    let parts: Vec<&str> = encoded.split('-').filter(|s| !s.is_empty()).collect();

    if let Some(git_idx) = parts.iter().position(|&p| p == "git") {
        if git_idx + 1 < parts.len() {
            return parts[git_idx + 1..].join("-");
        }
    }

    for marker in ["projects", "src", "code", "repos", "workspace"] {
        if let Some(idx) = parts.iter().position(|p| *p == marker) {
            if idx + 1 < parts.len() {
                return parts[idx + 1..].join("-");
            }
        }
    }

    if parts.len() >= 3 && parts[0] == "home" {
        return parts[2..].join("-");
    }

    parts.last().unwrap_or(&"unknown").to_string()
}

/// Extract a usage record from either an "assistant" or "progress" JSONL line.
/// Both types share the same structure once we resolve the path to the message object.
fn extract_record(
    parsed: &serde_json::Value,
    session_id: &str,
    project: &str,
) -> Option<UsageRecord> {
    let line_type = parsed.get("type").and_then(|v| v.as_str())?;

    // Resolve the paths to message, usage, timestamp, and requestId
    // depending on whether this is an "assistant" or "progress" record.
    let (message, timestamp_val, request_id_val) = match line_type {
        "assistant" => {
            let message = parsed.get("message")?;
            let ts = parsed.get("timestamp");
            let rid = parsed.get("requestId");
            (message, ts, rid)
        }
        "progress" => {
            let data = parsed.get("data")?;
            let data_type = data.get("type").and_then(|v| v.as_str())?;
            if data_type != "agent_progress" {
                return None;
            }
            let outer_message = data.get("message")?;
            let inner_message = outer_message.get("message")?;
            let ts = outer_message
                .get("timestamp")
                .or_else(|| parsed.get("timestamp"));
            let rid = outer_message.get("requestId");
            (inner_message, ts, rid)
        }
        _ => return None,
    };

    let usage = message.get("usage")?;
    let timestamp_str = timestamp_val?.as_str()?;
    let timestamp: DateTime<Utc> = timestamp_str.parse().ok()?;

    let model = message.get("model")?.as_str()?;
    if model == "<synthetic>" {
        return None;
    }
    let model = model.to_string();
    let message_id = message.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let request_id = request_id_val.and_then(|v| v.as_str()).unwrap_or("");

    let project = parsed
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|cwd| cwd.rsplit('/').next().unwrap_or(project).to_string())
        .unwrap_or_else(|| project.to_string());

    Some(UsageRecord {
        provider: "claude".to_string(),
        session_id: session_id.to_string(),
        timestamp,
        project,
        model,
        message_id: message_id.to_string(),
        request_id: request_id.to_string(),
        input_tokens: usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}
