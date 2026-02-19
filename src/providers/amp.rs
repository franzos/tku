use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, HomeFallback, Provider,
    XdgBase,
};
use crate::storage::Storage;
use crate::types::UsageRecord;

pub struct AmpProvider;

impl Provider for AmpProvider {
    fn name(&self) -> &str {
        "amp"
    }

    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
    ) {
        let roots = compute_roots();
        let files = discover_files(&roots, "json");
        discover_and_parse_with(self.name(), files, storage, progress, |path| {
            parse_json_file(path)
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    compute_provider_roots(
        Some("AMP_DATA_DIR"),
        &["threads"],
        &[HomeFallback {
            base: XdgBase::Data,
            subpaths: &["amp", "threads"],
        }],
    )
}

fn parse_json_file(path: &Path) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let thread_id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Build messageId -> usage lookup for cache tokens
    let cache_map = build_cache_map(&parsed);

    // Extract from usageLedger.events
    let events = match parsed
        .get("usageLedger")
        .and_then(|l| l.get("events"))
        .and_then(|e| e.as_array())
    {
        Some(e) => e,
        None => return Vec::new(),
    };

    let mut records = Vec::new();

    for event in events {
        if let Some(record) = extract_ledger_event(event, &thread_id, &cache_map) {
            records.push(record);
        }
    }

    records
}

/// Map messageId (number) -> (cacheCreationInputTokens, cacheReadInputTokens)
fn build_cache_map(parsed: &serde_json::Value) -> HashMap<u64, (u64, u64)> {
    let mut map = HashMap::new();

    let messages = match parsed.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return map,
    };

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            continue;
        }

        let msg_id = match msg.get("messageId").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => continue,
        };

        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };

        let cache_creation = usage
            .get("cacheCreationInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cacheReadInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        map.insert(msg_id, (cache_creation, cache_read));
    }

    map
}

fn extract_ledger_event(
    event: &serde_json::Value,
    thread_id: &str,
    cache_map: &HashMap<u64, (u64, u64)>,
) -> Option<UsageRecord> {
    let timestamp_str = event.get("timestamp").and_then(|v| v.as_str())?;
    let timestamp: DateTime<Utc> = timestamp_str.parse().ok()?;

    let model = event
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let tokens = event.get("tokens")?;
    let input = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);

    // Look up cache tokens from the matching message
    let (cache_creation, cache_read) = event
        .get("toMessageId")
        .and_then(|v| v.as_u64())
        .and_then(|mid| cache_map.get(&mid))
        .copied()
        .unwrap_or((0, 0));

    // Use event.id as message_id (already unique per thread)
    let message_id = event
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(UsageRecord {
        provider: "amp".to_string(),
        session_id: thread_id.to_string(),
        timestamp,
        project: "amp".to_string(),
        model,
        message_id,
        request_id: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
    })
}
