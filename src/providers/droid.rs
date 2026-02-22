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

pub struct DroidProvider;

impl Provider for DroidProvider {
    fn name(&self) -> &str {
        "droid"
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

        // Only process *.settings.json files
        let settings_files: Vec<_> = files
            .into_iter()
            .filter(|f| {
                f.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".settings.json"))
            })
            .collect();

        discover_and_parse_with(self.name(), settings_files, storage, progress, |path| {
            parse_settings_file(path)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("FACTORY_HOME"),
        &["sessions"],
        &[HomeFallback {
            base: XdgBase::Home,
            subpaths: &[".factory", "sessions"],
        }],
    )
}

#[derive(Deserialize)]
struct DroidSettingsJson {
    #[serde(rename = "tokenUsage")]
    token_usage: Option<DroidTokenUsage>,
    #[serde(rename = "providerLockTimestamp")]
    provider_lock_timestamp: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct DroidTokenUsage {
    #[serde(rename = "inputTokens")]
    input_tokens: Option<u64>,
    #[serde(rename = "outputTokens")]
    output_tokens: Option<u64>,
    #[serde(rename = "cacheCreationTokens")]
    cache_creation_tokens: Option<u64>,
    #[serde(rename = "cacheReadTokens")]
    cache_read_tokens: Option<u64>,
}

/// Normalize model name: strip `custom:` prefix, remove `[Provider]` brackets,
/// lowercase, dots to hyphens, collapse multiple hyphens.
fn normalize_model(raw: &str) -> String {
    let mut s = raw.to_string();

    // Strip custom: prefix
    if let Some(rest) = s.strip_prefix("custom:") {
        s = rest.to_string();
    }

    // Remove [Provider] bracket patterns
    while let Some(start) = s.find('[') {
        if let Some(end) = s[start..].find(']') {
            s = format!("{}{}", &s[..start], &s[start + end + 1..]);
        } else {
            break;
        }
    }

    s = s.to_lowercase();
    s = s.replace('.', "-");

    // Collapse multiple hyphens
    while s.contains("--") {
        s = s.replace("--", "-");
    }

    s.trim_matches('-').to_string()
}

fn parse_settings_file(path: &Path) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let settings: DroidSettingsJson = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let token_usage = match settings.token_usage {
        Some(t) => t,
        None => return Vec::new(),
    };

    let input = token_usage.input_tokens.unwrap_or(0);
    let output = token_usage.output_tokens.unwrap_or(0);

    if input == 0 && output == 0 {
        return Vec::new();
    }

    let cache_creation = token_usage.cache_creation_tokens.unwrap_or(0);
    let cache_read = token_usage.cache_read_tokens.unwrap_or(0);

    let model = settings
        .model
        .as_deref()
        .map(normalize_model)
        .unwrap_or_else(|| "unknown".to_string());

    // Session ID: filename stem minus .settings suffix
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .strip_suffix(".settings")
        .unwrap_or("unknown")
        .to_string();

    let timestamp = settings
        .provider_lock_timestamp
        .as_ref()
        .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| {
            std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .and_then(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0))
                .unwrap_or_else(Utc::now)
        });

    let message_id = format!("droid:{session_id}:{timestamp}:{input}:{output}");

    vec![UsageRecord {
        provider: "droid".to_string(),
        session_id,
        timestamp,
        project: "droid".to_string(),
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
    }]
}
