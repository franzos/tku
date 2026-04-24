use std::fs;

use serde::Deserialize;

use crate::paths;
use crate::pricing::PricingSource;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub pricing_source: Option<PricingSource>,
    pub currency: Option<String>,
}

pub fn load_config() -> Config {
    let Some(path) = paths::config_file() else {
        return Config::default();
    };

    let Ok(data) = fs::read_to_string(&path) else {
        return Config::default();
    };

    match toml::from_str(&data) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Warning: invalid config at {}: {}", path.display(), e);
            Config::default()
        }
    }
}
