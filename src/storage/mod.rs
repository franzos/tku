pub mod bitcode_store;
#[cfg(feature = "sqlite")]
pub mod import;
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
    ///
    /// Destructive: source transcripts are routinely deleted by the tools that
    /// wrote them, so these records usually have no other copy. Opt-in only,
    /// via `--prune`.
    fn prune(&mut self, provider: &str, existing: &[PathBuf]);

    /// Persist any pending changes to disk. No-op if nothing changed.
    /// Takes `&mut self` so a backend can compact before writing.
    fn flush(&mut self);

    /// Move all cached records out of the store. Call after flush().
    fn drain_all(&mut self) -> Vec<UsageRecord>;
}

pub fn default_storage() -> Box<dyn Storage> {
    #[cfg(feature = "sqlite")]
    {
        match sqlite_store::SqliteStorage::open() {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("warning: sqlite cache unavailable ({e}); falling back to bitcode cache");
                Box::new(bitcode_store::BitcodeStorage::new())
            }
        }
    }
    #[cfg(not(feature = "sqlite"))]
    {
        Box::new(bitcode_store::BitcodeStorage::new())
    }
}
