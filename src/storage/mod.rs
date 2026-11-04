pub mod bitcode_store;
mod format;
#[cfg(feature = "sqlite")]
pub mod import;
mod legacy;
pub mod relocate;
mod rollup;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;
pub mod upgrade;

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
    /// A backend may defer changes that are safe to redo from source, such as
    /// inserts of live files; those stay pending and are not a failure.
    /// Returns false when a pending change could not be persisted.
    fn flush(&mut self) -> bool;

    /// Make every later `flush` write all pending changes, deferred ones included.
    fn force_write(&mut self) {}

    /// Move all cached records out of the store. Call after flush().
    fn drain_all(&mut self) -> Vec<UsageRecord>;

    /// Retag the `claude` records of account `old` as `new`, including the
    /// account segment of compacted aggregate message ids. When both dirs are
    /// given, cached file paths under `old_dir` move under `new_dir`; a path
    /// whose target is already cached keeps its old key.
    fn rekey_account(
        &mut self,
        old: &str,
        new: &str,
        old_dir: Option<&Path>,
        new_dir: Option<&Path>,
    );
}

/// `path` moved from under `old_dir` to under `new_dir`, or `None` when it is
/// not under `old_dir`.
fn remap_path(path: &str, old_dir: &Path, new_dir: &Path) -> Option<String> {
    let rest = Path::new(path).strip_prefix(old_dir).ok()?;
    Some(new_dir.join(rest).to_string_lossy().into_owned())
}

/// Run `f` under a [`crate::paths::TestHome`] on a fresh scratch dir, so a
/// store under test never loads or writes the real cache or walks real transcripts.
#[cfg(test)]
pub(crate) fn with_scratch_home<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    let dir = std::env::temp_dir().join(format!(
        "tku-storage-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let home = crate::paths::TestHome::set(&dir);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    drop(home);
    let _ = std::fs::remove_dir_all(&dir);
    result.unwrap_or_else(|e| std::panic::resume_unwind(e))
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
