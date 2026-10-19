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
    /// Returns false when a pending change could not be persisted.
    fn flush(&mut self) -> bool;

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

/// Run `f` with `TKU_HOME` and `XDG_RUNTIME_DIR` pointing at a fresh scratch
/// dir, so a store under test never loads or writes the real cache.
#[cfg(test)]
fn with_scratch_home<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    let _guard = crate::paths::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "tku-storage-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let prev_runtime = std::env::var_os("XDG_RUNTIME_DIR");
    std::env::set_var("TKU_HOME", &dir);
    std::env::set_var("XDG_RUNTIME_DIR", &dir);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    std::env::remove_var("TKU_HOME");
    match prev_runtime {
        Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
        None => std::env::remove_var("XDG_RUNTIME_DIR"),
    }
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
