use std::collections::HashMap;

use anyhow::Result;

use crate::cost::ModelPricing;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

const MAX_RESPONSE_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

pub fn fetch_litellm_json() -> Result<String> {
    let body = crate::http::agent()
        .get(LITELLM_URL)
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()?;
    Ok(body)
}

pub fn parse_litellm_json(data: &str) -> Result<HashMap<String, ModelPricing>> {
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(data)?;
    let mut map = HashMap::new();

    for (key, val) in &raw {
        let Some(input) = val.get("input_cost_per_token").and_then(|v| v.as_f64()) else {
            continue;
        };
        let Some(output) = val.get("output_cost_per_token").and_then(|v| v.as_f64()) else {
            continue;
        };

        let cache_read = val
            .get("cache_read_input_token_cost")
            .and_then(|v| v.as_f64());
        let cache_creation = val
            .get("cache_creation_input_token_cost")
            .and_then(|v| v.as_f64());

        let cache_creation_1h = val
            .get("cache_creation_input_token_cost_above_1hr")
            .and_then(|v| v.as_f64());

        let supports_fast_mode = val
            .get("supports_speed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && val
                .get("provider_specific_entry")
                .and_then(|v| v.get("fast"))
                .is_some();

        let pricing = ModelPricing {
            input_cost_per_token: input,
            output_cost_per_token: output,
            cache_read_input_token_cost: cache_read,
            cache_creation_input_token_cost: cache_creation,
            cache_creation_1h_input_token_cost: crate::cost::resolve_cache_creation_1h_cost(
                input,
                cache_creation,
                cache_creation_1h,
            ),
            supports_fast_mode,
        };

        // Store under the original key
        map.insert(key.clone(), pricing.clone());

        // Also store under normalized names for lookup
        for normalized in normalize_key(key) {
            map.entry(normalized).or_insert_with(|| pricing.clone());
        }
    }

    Ok(map)
}

/// Generate normalized variants of a LiteLLM key so Claude Code model names
/// (e.g. "claude-opus-4-5-20251101") can be looked up directly.
fn normalize_key(key: &str) -> Vec<String> {
    let mut variants = Vec::new();

    // Strip known prefixes: "anthropic.", "us.anthropic.", "eu.anthropic.", etc.
    let stripped = key;
    let stripped = strip_provider_prefix(stripped);

    if stripped != key {
        variants.push(stripped.to_string());
    }

    // Strip version suffixes: "-v1:0", "-v1", ":0"
    let without_suffix = strip_version_suffix(stripped);
    if without_suffix != stripped {
        variants.push(without_suffix.to_string());
    }

    variants
}

fn strip_provider_prefix(key: &str) -> &str {
    // Order matters: try longest prefixes first
    let prefixes = [
        "us.anthropic.",
        "eu.anthropic.",
        "au.anthropic.",
        "apac.anthropic.",
        "global.anthropic.",
        "anthropic.",
        "bedrock/",
        "openai/",
    ];

    for prefix in prefixes {
        if let Some(rest) = key.strip_prefix(prefix) {
            return rest;
        }
    }

    // Handle bedrock with region: "bedrock/us-west-2/..."
    if let Some(rest) = key.strip_prefix("bedrock/") {
        if let Some(idx) = rest.find('/') {
            return &rest[idx + 1..];
        }
    }

    key
}

fn strip_version_suffix(key: &str) -> &str {
    // Strip "-v1:0", "-v1", ":0"
    if let Some(stripped) = key.strip_suffix(":0") {
        if let Some(stripped2) = stripped.strip_suffix("-v1") {
            return stripped2;
        }
        return stripped;
    }
    if let Some(stripped) = key.strip_suffix("-v1") {
        return stripped;
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_mode_needs_both_the_flag_and_a_fast_entry() {
        let data = r#"{
            "claude-opus-5": {"input_cost_per_token": 5e-6, "output_cost_per_token": 25e-6,
                "cache_creation_input_token_cost": 6.25e-6,
                "cache_creation_input_token_cost_above_1hr": 1e-5,
                "supports_speed": true, "provider_specific_entry": {"us": 1.1, "fast": 2.0}},
            "claude-opus-4-7": {"input_cost_per_token": 5e-6, "output_cost_per_token": 25e-6,
                "supports_speed": true, "provider_specific_entry": {"us": 1.1}},
            "claude-sonnet-5": {"input_cost_per_token": 3e-6, "output_cost_per_token": 15e-6}
        }"#;
        let map = parse_litellm_json(data).unwrap();
        assert!(map["claude-opus-5"].supports_fast_mode);
        assert!(!map["claude-opus-4-7"].supports_fast_mode);
        assert!(!map["claude-sonnet-5"].supports_fast_mode);
    }

    #[test]
    fn the_one_hour_write_rate_is_explicit_when_published_and_derived_otherwise() {
        let data = r#"{
            "with-1h": {"input_cost_per_token": 5e-6, "output_cost_per_token": 25e-6,
                "cache_creation_input_token_cost": 6.25e-6,
                "cache_creation_input_token_cost_above_1hr": 1e-5},
            "without-1h": {"input_cost_per_token": 3e-6, "output_cost_per_token": 15e-6,
                "cache_creation_input_token_cost": 3.75e-6},
            "no-cache-write": {"input_cost_per_token": 3e-6, "output_cost_per_token": 15e-6}
        }"#;
        let map = parse_litellm_json(data).unwrap();
        assert_eq!(
            map["with-1h"].cache_creation_1h_input_token_cost,
            Some(1e-5)
        );
        assert_eq!(
            map["without-1h"].cache_creation_1h_input_token_cost,
            Some(6e-6)
        );
        assert_eq!(
            map["no-cache-write"].cache_creation_1h_input_token_cost,
            None
        );
    }
}
