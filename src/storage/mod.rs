#[cfg(not(feature = "sqlite"))]
pub mod bitcode_store;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;

use std::path::{Path, PathBuf};

use crate::types::UsageRecord;

/// Storage backend for cached usage records.
///
/// All file-level operations are scoped by provider name so
/// multiple providers can share a backend without interference.
pub trait Storage {
    /// Check if a file is cached and fresh (matching mtime + size).
    fn is_cached(&mut self, provider: &str, file_path: &Path, mtime: i64, size: u64) -> bool;

    /// Store parsed records for a file.
    fn insert(
        &mut self,
        provider: &str,
        file_path: &Path,
        mtime: i64,
        size: u64,
        records: Vec<UsageRecord>,
    );

    /// Remove entries for files that no longer exist on disk.
    /// Only affects the given provider's entries.
    fn prune(&mut self, provider: &str, existing: &[PathBuf]);

    /// Persist any pending changes to disk. No-op if nothing changed.
    fn flush(&self);

    /// Move all cached records out of the store. Call after flush().
    fn drain_all(&mut self) -> Vec<UsageRecord>;
}

pub fn default_storage() -> Box<dyn Storage> {
    #[cfg(feature = "sqlite")]
    {
        Box::new(sqlite_store::SqliteStorage::open())
    }
    #[cfg(not(feature = "sqlite"))]
    {
        Box::new(bitcode_store::BitcodeStorage::new())
    }
}
