use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, HomeFallback, Provider,
    XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct GeminiProvider;

impl Provider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
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
        let files = discover_files(&roots, "json");
        discover_and_parse_with(self.name(), files, storage, progress, |path| {
            parse_session_file(path)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("GEMINI_HOME"),
        &["tmp"],
        &[HomeFallback {
            base: XdgBase::Home,
            subpaths: &[".gemini", "tmp"],
        }],
    )
}

#[derive(Deserialize)]
struct GeminiSession {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "projectHash")]
    project_hash: Option<String>,
    messages: Option<Vec<GeminiMessage>>,
}

#[derive(Deserialize)]
struct GeminiMessage {
    id: Option<String>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    model: Option<String>,
    tokens: Option<GeminiTokens>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct GeminiTokens {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
}

fn parse_session_file(path: &Path) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let session: GeminiSession = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let session_id = session.session_id.unwrap_or_else(|| "unknown".to_string());
    let project = session
        .project_hash
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gemini".to_string());

    let file_mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0).unwrap_or_else(Utc::now));

    let messages = match session.messages {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut records = Vec::new();

    for msg in &messages {
        let msg_type = match &msg.msg_type {
            Some(t) => t,
            None => continue,
        };
        if msg_type != "gemini" {
            continue;
        }

        let tokens = match &msg.tokens {
            Some(t) => t,
            None => continue,
        };

        let model = match &msg.model {
            Some(m) => m.clone(),
            None => continue,
        };

        let input = tokens.input.unwrap_or(0);
        let output = tokens.output.unwrap_or(0);
        let cached = tokens.cached.unwrap_or(0);

        if input == 0 && output == 0 {
            continue;
        }

        let timestamp = msg
            .timestamp
            .as_ref()
            .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
            .or(file_mtime)
            .unwrap_or_else(Utc::now);

        let msg_id_str = msg.id.as_deref().unwrap_or("unknown");
        let message_id = format!("gemini:{session_id}:{msg_id_str}");

        records.push(UsageRecord {
            provider: "gemini".to_string(),
            session_id: session_id.clone(),
            timestamp,
            project: project.clone(),
            model,
            message_id,
            request_id: String::new(),
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cached,
        });
    }

    records
}
