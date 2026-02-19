mod litellm;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;

use crate::cost::{ModelPricing, PricingMap};

const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

pub struct CachedPricing {
    map: HashMap<String, ModelPricing>,
}

impl PricingMap for CachedPricing {
    fn get(&self, model: &str) -> Option<&ModelPricing> {
        self.map.get(model)
    }
}

fn cache_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "tku").map(|d| d.cache_dir().join("pricing.json"))
}

fn cache_is_fresh(path: &PathBuf) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() < CACHE_TTL_SECS)
        .unwrap_or(false)
}

pub fn load_pricing(offline: bool) -> Result<CachedPricing> {
    let cache = cache_path();

    // Try cache first
    if let Some(ref path) = cache {
        if offline || cache_is_fresh(path) {
            if let Ok(data) = fs::read_to_string(path) {
                if let Ok(map) = litellm::parse_litellm_json(&data) {
                    return Ok(CachedPricing { map });
                }
            }
            if offline {
                bail!("--offline: no valid pricing cache found");
            }
        }
    }

    // Fetch fresh
    let data = litellm::fetch_litellm_json().context("Failed to fetch pricing data")?;
    let map = litellm::parse_litellm_json(&data).context("Failed to parse pricing data")?;

    // Write cache
    if let Some(ref path) = cache {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, &data);
    }

    Ok(CachedPricing { map })
}
