use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, HomeFallback, Provider,
    XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct CodexProvider;

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
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
            let project = project_from_session_id(&session_id);
            parse_jsonl_file(path, &session_id, &project)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("CODEX_HOME"),
        &["sessions"],
        &[
            HomeFallback {
                base: XdgBase::Home,
                subpaths: &[".codex", "sessions"],
            },
            HomeFallback {
                base: XdgBase::Config,
                subpaths: &["codex", "sessions"],
            },
        ],
    )
}

/// Session ID: relative path under sessions/, strip .jsonl, normalize to /
fn session_id_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    if let Some(idx) = path_str.find("/sessions/") {
        let relative = &path_str[idx + "/sessions/".len()..];
        return relative
            .strip_suffix(".jsonl")
            .unwrap_or(relative)
            .replace('\\', "/");
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Project: first path component of session ID
fn project_from_session_id(session_id: &str) -> String {
    session_id
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("codex")
        .to_string()
}

#[derive(Default)]
struct CumulativeTotals {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
}

/// Codex uses a two-pass approach within a single file: turn_context lines
/// set the model, and token_count lines carry the actual usage data.
/// This stateful iteration doesn't fit the generic parse_jsonl_lines utility.
fn parse_jsonl_file(path: &Path, session_id: &str, project: &str) -> Vec<UsageRecord> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut last_model: Option<String> = None;
    let mut prev_totals = CumulativeTotals::default();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Fast path: only parse lines relevant to us
        if line.contains("\"turn_context\"") {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(model) = extract_model_from_turn_context(&parsed) {
                    last_model = Some(model);
                }
            }
            continue;
        }

        if !line.contains("\"token_count\"") {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(record) =
            extract_token_event(&parsed, session_id, project, &last_model, &mut prev_totals)
        {
            records.push(record);
        }
    }

    records
}

fn extract_model_from_turn_context(parsed: &serde_json::Value) -> Option<String> {
    let payload = parsed.get("payload")?;

    // Try payload.info.model, payload.info.metadata.model, payload.model, payload.metadata.model
    if let Some(m) = payload
        .get("info")
        .and_then(|i| i.get("model"))
        .and_then(|v| v.as_str())
    {
        return Some(m.to_string());
    }
    if let Some(m) = payload
        .get("info")
        .and_then(|i| i.get("metadata"))
        .and_then(|md| md.get("model"))
        .and_then(|v| v.as_str())
    {
        return Some(m.to_string());
    }
    if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
        return Some(m.to_string());
    }
    if let Some(m) = payload
        .get("metadata")
        .and_then(|md| md.get("model"))
        .and_then(|v| v.as_str())
    {
        return Some(m.to_string());
    }

    None
}

fn extract_token_event(
    parsed: &serde_json::Value,
    session_id: &str,
    project: &str,
    last_model: &Option<String>,
    prev_totals: &mut CumulativeTotals,
) -> Option<UsageRecord> {
    let payload = parsed.get("payload")?;

    // Verify this is a token_count event
    let event_type = payload.get("type").and_then(|v| v.as_str())?;
    if event_type != "token_count" {
        return None;
    }

    let info = payload.get("info")?;

    let timestamp_str = parsed.get("timestamp").and_then(|v| v.as_str())?;
    let timestamp: DateTime<Utc> = timestamp_str.parse().ok()?;

    // Model resolution: event fields first, then turn_context fallback
    let model = info
        .get("model")
        .and_then(|v| v.as_str())
        .or_else(|| {
            info.get("metadata")
                .and_then(|md| md.get("model"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| payload.get("model").and_then(|v| v.as_str()))
        .or_else(|| {
            payload
                .get("metadata")
                .and_then(|md| md.get("model"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string())
        .or_else(|| last_model.clone())
        .unwrap_or_else(|| "gpt-5".to_string());

    // Delta calculation: prefer last_token_usage, fallback to total_token_usage subtraction
    let (input, output, cached) = if let Some(last) = info.get("last_token_usage") {
        let input = last
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = last
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cached = last
            .get("cached_input_tokens")
            .or_else(|| last.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        (input, output, cached)
    } else if let Some(total) = info.get("total_token_usage") {
        let cur_input = total
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cur_output = total
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cur_cached = total
            .get("cached_input_tokens")
            .or_else(|| total.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let input = cur_input.saturating_sub(prev_totals.input_tokens);
        let output = cur_output.saturating_sub(prev_totals.output_tokens);
        let cached = cur_cached.saturating_sub(prev_totals.cached_input_tokens);

        prev_totals.input_tokens = cur_input;
        prev_totals.output_tokens = cur_output;
        prev_totals.cached_input_tokens = cur_cached;

        (input, output, cached)
    } else {
        return None;
    };

    // Skip zero-token events
    if input == 0 && output == 0 && cached == 0 {
        return None;
    }

    let message_id = format!("codex:{session_id}:{timestamp}:{input}:{output}");

    Some(UsageRecord {
        provider: "codex".to_string(),
        session_id: session_id.to_string(),
        timestamp,
        project: project.to_string(),
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    })
}
