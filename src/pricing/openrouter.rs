use std::collections::HashMap;

use anyhow::Result;

use crate::cost::ModelPricing;

const OPENROUTER_URL: &str = "https://api.openrouter.ai/api/v1/models";

pub fn fetch_openrouter_json() -> Result<String> {
    let body = ureq::get(OPENROUTER_URL)
        .call()?
        .body_mut()
        .read_to_string()?;
    Ok(body)
}

pub fn parse_openrouter_json(data: &str) -> Result<HashMap<String, ModelPricing>> {
    let raw: serde_json::Value = serde_json::from_str(data)?;
    let mut map = HashMap::new();

    let Some(models) = raw.get("data").and_then(|d| d.as_array()) else {
        return Ok(map);
    };

    for model in models {
        let Some(id) = model.get("id").and_then(|v| v.as_str()) else {
            continue;
        };

        let pricing = model.get("pricing");

        let input = pricing
            .and_then(|p| p.get("prompt"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        let output = pricing
            .and_then(|p| p.get("completion"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());

        let Some(input) = input else { continue };
        let Some(output) = output else { continue };

        let cache_read = pricing
            .and_then(|p| p.get("input_cache_read"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        let cache_creation = pricing
            .and_then(|p| p.get("input_cache_write"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());

        let mp = ModelPricing {
            input_cost_per_token: input,
            output_cost_per_token: output,
            cache_read_input_token_cost: cache_read,
            cache_creation_input_token_cost: cache_creation,
        };

        // Store under full ID (e.g. "anthropic/claude-opus-4-5")
        map.insert(id.to_string(), mp.clone());

        // Also store under short form (part after last '/')
        if let Some(short) = id.rsplit('/').next() {
            if short != id {
                map.entry(short.to_string()).or_insert_with(|| mp.clone());
            }
        }
    }

    Ok(map)
}
