use std::fs;

use serde::Deserialize;

use crate::paths;
use crate::pricing::PricingSource;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub pricing_source: Option<PricingSource>,
    pub currency: Option<String>,
    /// Ceiling on a single provider's cache file. Past it, records older than
    /// 180 days are collapsed to daily totals rather than dropped.
    pub cache_max_bytes: Option<u64>,
    /// Days after which a cached file whose newest record is older is rolled up
    /// to one record per (session, UTC day, project, model, account, fast mode).
    /// Totals and costs stay exact; intra-day detail is lost, so `model-burn`
    /// active time over older ranges degrades. Below 35 days, rollups reach into
    /// the 35-day subscription window. Default 180; values below 1 count as 1.
    pub rollup_after_days: Option<u32>,
    #[serde(default)]
    pub spawn: Option<SpawnConfig>,
}

pub const DEFAULT_ROLLUP_AFTER_DAYS: u32 = 180;

impl Config {
    pub fn rollup_after_days(&self) -> u32 {
        self.rollup_after_days
            .map_or(DEFAULT_ROLLUP_AFTER_DAYS, |d| d.max(1))
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct SpawnConfig {
    pub ephemeral: Option<bool>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup_days(toml: &str) -> u32 {
        toml::from_str::<Config>(toml).unwrap().rollup_after_days()
    }

    #[test]
    fn rollup_after_days_defaults_and_clamps() {
        assert_eq!(rollup_days(""), DEFAULT_ROLLUP_AFTER_DAYS);
        assert_eq!(rollup_days("rollup_after_days = 0"), 1);
        assert_eq!(rollup_days("rollup_after_days = 1"), 1);
        assert_eq!(rollup_days("rollup_after_days = 35"), 35);
    }

    #[test]
    fn configured_rollup_days_reads_config_toml() {
        crate::storage::with_scratch_home("config-rollup", || {
            let path = paths::config_file().unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert_eq!(
                crate::storage::bitcode_store::configured_rollup_days(),
                DEFAULT_ROLLUP_AFTER_DAYS
            );
            fs::write(&path, "rollup_after_days = 0\n").unwrap();
            assert_eq!(crate::storage::bitcode_store::configured_rollup_days(), 1);
        });
    }
}
