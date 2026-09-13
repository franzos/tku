use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::{
    compute_provider_roots, discover_and_parse_with, discover_files, parse_jsonl_lines,
    HomeFallback, Provider as ProviderDriver, XdgBase,
};
use crate::storage::Storage;
use crate::types::{Provider, UsageRecord};

pub struct ClaudeProvider;

impl ProviderDriver for ClaudeProvider {
    fn id(&self) -> Provider {
        Provider::Claude
    }

    fn root_dirs(&self) -> Vec<PathBuf> {
        compute_roots()
    }

    /// Claude scatters transcript-adjacent content across siblings of
    /// `projects/`, so the default (usage roots only) misses most of it.
    ///
    /// `projects/` deliberately takes every file, not just `*.jsonl`: it also
    /// holds ~100 MB of spilled `tool-results/*.{txt,md}`, which is the same
    /// tool-output path that carries most secrets in the transcripts.
    ///
    /// `.credentials.json` is excluded on purpose. It holds the live Claude
    /// OAuth token *and* an `mcpOAuth` entry per connected MCP server, so
    /// redacting it would sign you out of Claude Code and every MCP server at
    /// once. `~/.claude.json` is in scope by contrast: its `mcpServers` blocks
    /// carry auth headers and `env` values, which are config, not live session
    /// state.
    fn scrub_targets(&self) -> Vec<crate::scrub::ScrubTarget> {
        use crate::scrub::ScrubTarget;

        let mut out = Vec::new();
        for root in compute_roots() {
            let Some(home) = root.parent().map(|p| p.to_path_buf()) else {
                continue;
            };
            out.push(ScrubTarget::any(root, "transcripts"));
            out.push(ScrubTarget::jsonl(
                home.join("history.jsonl"),
                "prompt-history",
            ));
            out.push(ScrubTarget::any(home.join("file-history"), "file-history"));
            out.push(ScrubTarget::any(home.join("paste-cache"), "paste-cache"));
            out.push(ScrubTarget::any(home.join("session-env"), "session-env"));
            out.push(ScrubTarget::any(home.join("debug"), "debug"));
            out.push(ScrubTarget::any(home.join("backups"), "config-backups"));
            out.push(ScrubTarget::any(home.join("jobs"), "jobs"));
        }

        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            out.push(ScrubTarget::json(home.join(".claude.json"), "config"));
        }

        out
    }

    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
        prune: bool,
    ) {
        let roots = compute_roots();
        let files = discover_files(&roots, "jsonl");
        // Per-record attribution: for each record, look up the account
        // active at its timestamp via the registry's switch log. This keeps
        // historical records correctly tagged even when the cache is wiped
        // (sqlite schema bump, bitcode corruption) and re-parsed under a
        // different active account than the one that wrote them.
        //
        // The registry was already updated with any implicit swap by
        // `detect_implicit_swap_pre_scan()` in main, so the lookup reflects
        // the current state. `live_uuid` is a fallback for records whose
        // timestamps fall before the earliest switch entry (e.g. on the
        // very first run before bootstrap has anchored to the earliest
        // record) and for the rare case where the registry has no entries
        // at all.
        //
        // Exec'd sessions are the exception: they run concurrently with
        // whatever account is globally active, so the switch log would
        // confidently tag them with the wrong one. Their path carries the
        // org_uuid, which is a strictly stronger signal.
        let registry = crate::accounts::load_registry("claude");
        let live_uuid = crate::accounts::current_claude_org_uuid()
            .or_else(|| registry.latest_switch().map(|s| s.org_uuid.clone()));
        let transcripts_base = crate::paths::transcripts_dir("claude");
        discover_and_parse_with(self.name(), files, storage, progress, prune, |path| {
            let mut records = parse_jsonl_file(path);
            let from_path = transcripts_base
                .as_deref()
                .and_then(|base| org_uuid_from_transcripts_path(base, path))
                .map(str::to_string);
            for r in &mut records {
                r.account_uuid = from_path.clone().or_else(|| {
                    registry
                        .account_at(r.timestamp)
                        .map(|e| e.org_uuid.clone())
                        .or_else(|| live_uuid.clone())
                });
            }
            records
        });
    }
}

fn compute_roots() -> Vec<PathBuf> {
    let mut roots = compute_provider_roots(
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
    );
    roots.extend(spawn_transcript_roots());
    roots
}

/// `account exec` sessions write through a symlink into
/// `<transcripts>/<org_uuid>/projects`. Roots point at the real directory, so
/// neither the scanner nor the watcher depends on symlink traversal.
///
/// Enumerated by listing the store, never from the registry: `remove_account`
/// keeps switch-log history on purpose, and the storage prune step drops cached
/// records for any file no longer walked — a registry-derived list would erase
/// a removed account's exec spend from every total on the next scan.
fn spawn_transcript_roots() -> Vec<PathBuf> {
    let Some(base) = crate::paths::transcripts_dir("claude") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path().join("projects"))
        .collect();
    roots.sort();
    roots
}

/// The first path component under the transcripts base *is* the `org_uuid`, so
/// attribution survives an account being renamed, removed, or re-added under a
/// reused name — none of which a timestamp lookup in the switch log would.
fn org_uuid_from_transcripts_path<'a>(
    base: &std::path::Path,
    path: &'a std::path::Path,
) -> Option<&'a str> {
    match path.strip_prefix(base).ok()?.components().next()? {
        std::path::Component::Normal(name) => name.to_str(),
        _ => None,
    }
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
        provider: Provider::Claude,
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
        cache_creation_1h_input_tokens: usage
            .get("cache_creation")
            .and_then(|v| v.get("ephemeral_1h_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        fast_mode: usage.get("speed").and_then(|v| v.as_str()) == Some("fast"),
        // Filled in by discover_and_parse via per-record account_at lookup.
        account_uuid: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> UsageRecord {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        extract_record(&v, "sess", "proj").unwrap()
    }

    #[test]
    fn one_hour_cache_writes_are_read_from_the_ttl_split() {
        let r = parse(
            r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","requestId":"req_1",
                "message":{"id":"msg_1","model":"claude-opus-5",
                "usage":{"input_tokens":4,"output_tokens":7,
                "cache_creation_input_tokens":29350,
                "cache_creation":{"ephemeral_1h_input_tokens":29350,"ephemeral_5m_input_tokens":0},
                "cache_read_input_tokens":120}}}"#,
        );
        assert_eq!(r.cache_creation_input_tokens, 29350);
        assert_eq!(r.cache_creation_1h_input_tokens, 29350);
        assert_eq!(r.cache_read_input_tokens, 120);
    }

    #[test]
    fn a_mixed_ttl_split_keeps_the_flat_total_authoritative() {
        let r = parse(
            r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","requestId":"req_2",
                "message":{"id":"msg_2","model":"claude-opus-5",
                "usage":{"input_tokens":1,"output_tokens":1,
                "cache_creation_input_tokens":1000,
                "cache_creation":{"ephemeral_1h_input_tokens":400,"ephemeral_5m_input_tokens":600}}}}"#,
        );
        assert_eq!(r.cache_creation_input_tokens, 1000);
        assert_eq!(r.cache_creation_1h_input_tokens, 400);
    }

    #[test]
    fn a_record_without_the_split_is_all_five_minute() {
        let r = parse(
            r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","requestId":"req_3",
                "message":{"id":"msg_3","model":"claude-opus-5",
                "usage":{"input_tokens":1,"output_tokens":1,
                "cache_creation_input_tokens":8192}}}"#,
        );
        assert_eq!(r.cache_creation_input_tokens, 8192);
        assert_eq!(r.cache_creation_1h_input_tokens, 0);
    }

    #[test]
    fn an_org_uuid_is_read_from_a_transcripts_path_and_nowhere_else() {
        let base = std::path::Path::new("/data/tku/transcripts/claude");
        assert_eq!(
            org_uuid_from_transcripts_path(
                base,
                std::path::Path::new(
                    "/data/tku/transcripts/claude/org-abc/projects/-home-p/s1.jsonl"
                )
            ),
            Some("org-abc")
        );
        // Outside the base the caller must fall back to the switch log.
        assert_eq!(
            org_uuid_from_transcripts_path(
                base,
                std::path::Path::new("/home/u/.claude/projects/-home-p/s1.jsonl")
            ),
            None
        );
        assert_eq!(org_uuid_from_transcripts_path(base, base), None);
    }

    /// An org dir belonging to no registry entry still yields a root: a removed
    /// account keeps its history, and a dropped root would prune it away.
    #[test]
    fn transcript_roots_come_from_listing_not_the_registry() {
        let _guard = crate::paths::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!(
            "tku-claude-roots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = home.join("data").join("transcripts").join("claude");
        std::fs::create_dir_all(base.join("org-unknown").join("projects")).unwrap();
        std::fs::create_dir_all(base.join("org-other").join("projects")).unwrap();
        std::fs::write(base.join("stray-file"), b"x").unwrap();

        std::env::set_var("TKU_HOME", &home);
        let roots = spawn_transcript_roots();
        let claude_roots = compute_roots();
        std::env::remove_var("TKU_HOME");

        assert_eq!(
            roots,
            vec![
                base.join("org-other").join("projects"),
                base.join("org-unknown").join("projects"),
            ]
        );
        // And they reach the shared root list the scanner and watcher use.
        assert!(claude_roots.ends_with(&roots));

        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn speed_marks_only_fast_mode_records() {
        let line = |speed: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","requestId":"req_4",
                    "message":{{"id":"msg_4","model":"claude-opus-5",
                    "usage":{{"input_tokens":1,"output_tokens":1,"speed":{speed}}}}}}}"#
            )
        };
        assert!(parse(&line("\"fast\"")).fast_mode);
        assert!(!parse(&line("\"standard\"")).fast_mode);
        assert!(!parse(&line("null")).fast_mode);
    }
}
