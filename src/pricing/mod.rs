mod litellm;
mod llmprices;
mod openrouter;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::cost::{ModelPricing, PricingMap};

const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Default, ValueEnum, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PricingSource {
    #[default]
    Litellm,
    Openrouter,
    Llmprices,
}

impl std::fmt::Display for PricingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PricingSource::Litellm => write!(f, "litellm"),
            PricingSource::Openrouter => write!(f, "openrouter"),
            PricingSource::Llmprices => write!(f, "llmprices"),
        }
    }
}

pub struct CachedPricing {
    map: HashMap<String, ModelPricing>,
}

impl PricingMap for CachedPricing {
    fn get(&self, model: &str) -> Option<&ModelPricing> {
        self.map.get(model)
    }
}

fn cache_path(source: &PricingSource) -> Option<PathBuf> {
    ProjectDirs::from("", "", "tku").map(|d| d.cache_dir().join(format!("pricing-{}.json", source)))
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

fn fetch_raw(source: &PricingSource) -> Result<String> {
    match source {
        PricingSource::Litellm => litellm::fetch_litellm_json(),
        PricingSource::Openrouter => openrouter::fetch_openrouter_json(),
        PricingSource::Llmprices => llmprices::fetch_llmprices_json(),
    }
}

fn parse_raw(source: &PricingSource, data: &str) -> Result<HashMap<String, ModelPricing>> {
    match source {
        PricingSource::Litellm => litellm::parse_litellm_json(data),
        PricingSource::Openrouter => openrouter::parse_openrouter_json(data),
        PricingSource::Llmprices => llmprices::parse_llmprices_json(data),
    }
}

pub fn load_pricing(source: &PricingSource, offline: bool) -> Result<CachedPricing> {
    let cache = cache_path(source);

    // Try cache first
    if let Some(ref path) = cache {
        if offline || cache_is_fresh(path) {
            if let Ok(data) = fs::read_to_string(path) {
                if let Ok(map) = parse_raw(source, &data) {
                    return Ok(CachedPricing { map });
                }
            }
            if offline {
                bail!("--offline: no valid pricing cache found for {}", source);
            }
        }
    }

    // Fetch fresh
    let data =
        fetch_raw(source).context(format!("Failed to fetch pricing data from {}", source))?;
    let map = parse_raw(source, &data).context("Failed to parse pricing data")?;

    // Write cache
    if let Some(ref path) = cache {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, &data);
    }

    Ok(CachedPricing { map })
}
