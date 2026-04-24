//! Central path resolution for tku.
//!
//! Every on-disk file the binary reads or writes flows through here so the
//! layout stays consistent and is trivially relocatable for tests.
//!
//! `TKU_HOME` override: if the environment variable is set, the three roots
//! (`cache`, `config`, `data`) live under `$TKU_HOME/{cache,config,data}`
//! instead of the platform-specific `ProjectDirs` locations. Useful for
//! integration tests and sandboxed runs; a single env-var gives a fully
//! isolated overlay without touching the user's real caches.

use std::path::PathBuf;

use directories::ProjectDirs;

pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "tku")
}

fn tku_home() -> Option<PathBuf> {
    std::env::var_os("TKU_HOME").map(PathBuf::from)
}

pub fn cache_dir() -> Option<PathBuf> {
    if let Some(h) = tku_home() {
        Some(h.join("cache"))
    } else {
        project_dirs().map(|d| d.cache_dir().to_path_buf())
    }
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(h) = tku_home() {
        Some(h.join("config"))
    } else {
        project_dirs().map(|d| d.config_dir().to_path_buf())
    }
}

/// Data directory root. Currently unused inside tku (no persistent non-cache
/// state lives here yet) — kept for parity with the `cache/config/data` triad
/// so callers can land incrementally without new path plumbing.
#[allow(dead_code)]
pub fn data_dir() -> Option<PathBuf> {
    if let Some(h) = tku_home() {
        Some(h.join("data"))
    } else {
        project_dirs().map(|d| d.data_dir().to_path_buf())
    }
}

// --- Cache files ---

/// Pricing JSON cache, one per source: `pricing-litellm.json`, etc.
pub fn pricing_cache_file(source: &str) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("pricing-{source}.json")))
}

/// Currency exchange-rate cache. Single file shared across currency codes;
/// the rate table inside covers all symbols.
pub fn exchange_cache_file() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("exchange.json"))
}

/// Per-provider bitcode cache file, e.g. `claude.bin`.
pub fn bitcode_cache_file(provider: &str) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("{provider}.bin")))
}

/// Sqlite records database (feature = "sqlite").
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub fn sqlite_db_file() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("records.db"))
}

/// Subscription snapshot store, one per tool: `subscription-claude.json`.
pub fn subscription_snapshot_file(tool: &str) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("subscription-{tool}.json")))
}

// --- Config files ---

pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Root of the stashed-credentials hierarchy for a given tool.
pub fn accounts_dir(tool: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join("accounts").join(tool))
}

pub fn registry_file(tool: &str) -> Option<PathBuf> {
    accounts_dir(tool).map(|d| d.join("registry.json"))
}
