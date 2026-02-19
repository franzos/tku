pub mod amp;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::storage::Storage;
use crate::types::UsageRecord;

pub trait Provider {
    fn name(&self) -> &str;
    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
    );
}

pub fn all_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::ClaudeProvider),
        Box::new(codex::CodexProvider),
        Box::new(pi::PiProvider),
        Box::new(amp::AmpProvider),
        Box::new(opencode::OpenCodeProvider),
    ]
}

pub(crate) struct DiscoveredFile {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: u64,
}

pub(crate) fn discovered_file(path: &Path) -> Option<DiscoveredFile> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(DiscoveredFile {
        path: path.to_path_buf(),
        mtime,
        size: meta.len(),
    })
}

pub(crate) fn discover_files(roots: &[PathBuf], extension: &str) -> Vec<DiscoveredFile> {
    let mut files = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().extension().is_some_and(|ext| ext == extension) {
                if let Some(df) = discovered_file(entry.path()) {
                    files.push(df);
                }
            }
        }
    }

    files
}

pub(crate) fn discover_and_parse_with<F>(
    name: &str,
    files: Vec<DiscoveredFile>,
    storage: &mut dyn Storage,
    progress: Option<&dyn Fn(usize, usize)>,
    parse: F,
) where
    F: Fn(&Path) -> Vec<UsageRecord> + Sync,
{
    let total = files.len();
    let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();

    // Phase 1: filter out cached files (sequential — needs &mut storage)
    let mut cached_count = 0;
    let mut uncached: Vec<&DiscoveredFile> = Vec::new();
    for file in &files {
        if storage.is_cached(name, &file.path, file.mtime, file.size) {
            cached_count += 1;
            if let Some(cb) = &progress {
                cb(cached_count, total);
            }
        } else {
            uncached.push(file);
        }
    }

    // Phase 2: parse uncached files in parallel
    let results: Vec<_> = uncached
        .par_iter()
        .map(|file| (*file, parse(&file.path)))
        .collect();

    // Phase 3: insert results (sequential — needs &mut storage)
    for (i, (file, records)) in results.into_iter().enumerate() {
        if let Some(cb) = &progress {
            cb(cached_count + i + 1, total);
        }
        storage.insert(name, &file.path, file.mtime, file.size, records);
    }

    storage.prune(name, &paths);
}

/// XDG base directory kind, determining which env var and fallback to use.
pub(crate) enum XdgBase {
    /// Uses XDG_CONFIG_HOME, falls back to ~/.config
    Config,
    /// Uses XDG_DATA_HOME, falls back to ~/.local/share
    Data,
    /// Direct ~/.<name> path (legacy tool defaults)
    Home,
}

/// A home-relative fallback path: XDG base kind + subpath segments to join.
pub(crate) struct HomeFallback {
    pub base: XdgBase,
    pub subpaths: &'static [&'static str],
}

/// Compute provider root directories using a common pattern:
///
/// 1. If `env_var` is set, use its value joined with each of `env_subpaths`
/// 2. Otherwise, for each `HomeFallback`, resolve the XDG base directory
///    and join its subpath segments
///
/// Returns an empty Vec if neither the env var nor HOME is available.
pub(crate) fn compute_provider_roots(
    env_var: Option<&str>,
    env_subpaths: &[&str],
    home_fallbacks: &[HomeFallback],
) -> Vec<PathBuf> {
    // If the provider-specific env var is set, use it directly
    if let Some(var_name) = env_var {
        if let Ok(val) = std::env::var(var_name) {
            let base = PathBuf::from(val);
            if env_subpaths.is_empty() {
                return vec![base];
            }
            return env_subpaths.iter().map(|sub| base.join(sub)).collect();
        }
    }

    let home = match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) => h,
        None => return Vec::new(),
    };

    home_fallbacks
        .iter()
        .map(|fb| {
            let base = match fb.base {
                XdgBase::Config => std::env::var("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".config")),
                XdgBase::Data => std::env::var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".local").join("share")),
                XdgBase::Home => home.clone(),
            };
            let mut path = base;
            for seg in fb.subpaths {
                path = path.join(seg);
            }
            path
        })
        .collect()
}

/// Parse a JSONL file, filtering and extracting records with a common pattern:
///
/// 1. Open file with BufReader
/// 2. For each line, check if it contains `filter` (fast pre-filter)
/// 3. If it passes, call `extract` with the raw line
/// 4. Collect all Some results
///
/// Returns an empty Vec on file open failure.
pub(crate) fn parse_jsonl_lines<F, T>(path: &Path, filter: &str, extract: F) -> Vec<T>
where
    F: Fn(&str) -> Option<T>,
{
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut results = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if !line.contains(filter) {
            continue;
        }

        if let Some(item) = extract(&line) {
            results.push(item);
        }
    }

    results
}
