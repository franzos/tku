use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Storage;
use crate::atomic_write::atomic_write;
use crate::paths;
use crate::types::UsageRecord;

/// Hard ceiling on bitcode cache file size. Matches the JSONL provider cap:
/// a cache larger than this is almost certainly corrupted or hostile, and
/// loading it into memory would OOM the process. Graceful-degrade: treat
/// as if no cache exists and re-parse from source.
const MAX_CACHE_BYTES: u64 = 500 * 1024 * 1024;

/// One file per provider: `~/.cache/tku/{provider}.bin`
///
/// Each provider's data is loaded/flushed independently so adding
/// a new provider doesn't affect existing ones' deserialization cost.
pub struct BitcodeStorage {
    providers: HashMap<String, ProviderCache>,
}

#[derive(Serialize, Deserialize, Default)]
struct ProviderCache {
    files: HashMap<String, CachedFile>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    mtime_secs: i64,
    size: u64,
    records: Vec<UsageRecord>,
}

impl BitcodeStorage {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Load (or create) the cache for a specific provider, lazily.
    fn provider_cache(&mut self, provider: &str) -> &mut ProviderCache {
        self.providers
            .entry(provider.to_string())
            .or_insert_with(|| {
                let Some(path) = paths::bitcode_cache_file(provider) else {
                    return ProviderCache::default();
                };
                // Pre-flight size check. `fs::read` allocates a Vec sized to
                // the file; a corrupted/hostile multi-GB file would OOM.
                if let Ok(meta) = fs::metadata(&path) {
                    if meta.len() > MAX_CACHE_BYTES {
                        eprintln!(
                            "tku: skipping oversize {provider} cache ({} bytes); will re-parse from source",
                            meta.len()
                        );
                        return ProviderCache::default();
                    }
                }
                let Ok(data) = fs::read(&path) else {
                    return ProviderCache::default();
                };
                bitcode::deserialize(&data).unwrap_or_default()
            })
    }
}

impl Storage for BitcodeStorage {
    fn is_cached(&mut self, provider: &str, file_path: &Path, mtime: i64, size: u64) -> bool {
        let pc = self.provider_cache(provider);
        let key = file_path.to_string_lossy();
        pc.files
            .get(key.as_ref())
            .is_some_and(|e| e.mtime_secs == mtime && e.size == size)
    }

    fn insert(
        &mut self,
        provider: &str,
        file_path: &Path,
        mtime: i64,
        size: u64,
        records: Vec<UsageRecord>,
    ) {
        let pc = self.provider_cache(provider);
        let key = file_path.to_string_lossy().to_string();
        pc.files.insert(
            key,
            CachedFile {
                mtime_secs: mtime,
                size,
                records,
            },
        );
        pc.dirty = true;
    }

    fn prune(&mut self, provider: &str, existing: &[PathBuf]) {
        let pc = self.provider_cache(provider);
        let known: HashSet<String> = existing
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let before = pc.files.len();
        pc.files.retain(|k, _| known.contains(k));
        if pc.files.len() != before {
            pc.dirty = true;
        }
    }

    fn flush(&self) {
        let Some(dir) = paths::cache_dir() else {
            return;
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("tku: failed to create cache dir: {e}");
            return;
        }

        for (name, pc) in &self.providers {
            if !pc.dirty {
                continue;
            }
            let data = match bitcode::serialize(pc) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("tku: failed to serialize {name} cache: {e}");
                    continue;
                }
            };
            let Some(path) = paths::bitcode_cache_file(name) else {
                continue;
            };
            if let Err(e) = atomic_write(&path, &data, None) {
                eprintln!("tku: failed to write {name} cache: {e}");
            }
        }
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        let mut all = Vec::new();
        for (_, mut pc) in self.providers.drain() {
            for (_, cf) in pc.files.drain() {
                all.extend(cf.records);
            }
        }
        all
    }
}
