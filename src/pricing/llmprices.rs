use std::collections::HashMap;

use anyhow::Result;

use crate::cost::ModelPricing;

const LLMPRICES_URL: &str = "https://www.llm-prices.com/current-v1.json";

pub fn fetch_llmprices_json() -> Result<String> {
    let body = ureq::get(LLMPRICES_URL)
        .call()?
        .body_mut()
        .read_to_string()?;
    Ok(body)
}

pub fn parse_llmprices_json(data: &str) -> Result<HashMap<String, ModelPricing>> {
    let root: serde_json::Value = serde_json::from_str(data)?;
    let mut map = HashMap::new();

    let Some(prices) = root.get("prices").and_then(|v| v.as_array()) else {
        return Ok(map);
    };

    for entry in prices {
        let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
            continue;
        };

        // Prices are per million tokens
        let input_per_m = entry.get("input").and_then(|v| v.as_f64());
        let output_per_m = entry.get("output").and_then(|v| v.as_f64());

        let Some(input_per_m) = input_per_m else {
            continue;
        };
        let Some(output_per_m) = output_per_m else {
            continue;
        };

        let cache_read = entry
            .get("input_cached")
            .and_then(|v| v.as_f64())
            .map(|v| v / 1_000_000.0);

        let mp = ModelPricing {
            input_cost_per_token: input_per_m / 1_000_000.0,
            output_cost_per_token: output_per_m / 1_000_000.0,
            cache_read_input_token_cost: cache_read,
            cache_creation_input_token_cost: None,
        };

        map.insert(id.to_string(), mp);
    }

    Ok(map)
}
