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

/// Data directory root. Home-based per-user location; used as the `spawn_dir`
/// fallback when `$XDG_RUNTIME_DIR` is unset.
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

/// Cached OAuth profile response per Claude org_uuid: `profile-<org>.json`.
/// Used to detect plan switches without hammering the API on every run.
pub fn profile_cache_file(org_uuid: &str) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("profile-{org_uuid}.json")))
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

// --- Runtime files ---

/// Root for `account exec`'s isolated Claude config dirs and per-account
/// locks. Prefers `$XDG_RUNTIME_DIR` (tmpfs, cleared at logout) so seeded
/// credentials don't linger on disk. Falls back to a `$HOME`-based per-user
/// dir, never a shared world-writable temp dir like `/tmp`.
pub fn spawn_dir(tool: &str) -> Option<PathBuf> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return Some(PathBuf::from(rt).join("tku").join("spawn").join(tool));
        }
    }
    data_dir().map(|d| d.join("spawn").join(tool))
}
