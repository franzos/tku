use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};

use super::{format, legacy, rollup, Storage};
use crate::atomic_write::atomic_write;
use crate::paths;
use crate::types::{self, Provider, UsageRecord};

/// Default ceiling on a provider's cache file. Configurable via
/// `cache_max_bytes` in `config.toml`.
///
/// The limit is set by rewrite cost, not disk: `flush` re-serializes the whole
/// provider file on every run that saw a new session, so the ceiling is also
/// the worst-case write per run. Rollups keep a normal corpus far below it.
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Fraction of the ceiling at which every run starts warning.
const WARN_FRACTION: f64 = 0.85;

/// Floor under the pre-read size guard, independent of the configured ceiling.
/// Reading is what risks an OOM; a file this size is loadable on any machine
/// that can run the scan, and refusing it would strand a store the user could
/// otherwise compact back down.
const ABSOLUTE_READ_LIMIT: u64 = 1024 * 1024 * 1024;

/// Ages tried in turn when the ceiling is reached: files whose newest record is
/// older are rolled up to daily totals; the first step that brings the file
/// under the ceiling wins.
const COMPACT_CUTOFF_DAYS: &[i64] = &[180, 90, 30];

/// How long `flush` waits for another process to finish writing the store.
pub(crate) const STORE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A file modified within this many seconds is live: its insert alone does not
/// make `flush` write, since the next run re-parses it from source anyway.
const LIVE_WINDOW_SECS: i64 = 300;

/// One file per provider: `~/.cache/tku/{provider}.bin`
///
/// Each provider's data is loaded/flushed independently so adding
/// a new provider doesn't affect existing ones' deserialization cost.
///
/// Despite the name this is an archive as much as a cache: source transcripts
/// are deleted by the tools that write them (Claude Code drops sessions past
/// `cleanupPeriodDays`), so records here routinely outlive their source file
/// and cannot be re-derived. Files older than `rollup_after_days` are rolled up
/// to daily totals on flush, the ceiling rolls up younger ones, and a store
/// still over it is written as is. Records are only deleted by an explicit `--prune`.
pub struct BitcodeStorage {
    providers: HashMap<String, ProviderCache>,
    max_bytes: u64,
    rollup_days: u32,
    /// Write dirty providers even when only live files changed.
    force: bool,
}

#[derive(Default)]
pub(crate) struct ProviderCache {
    pub(crate) files: HashMap<String, CachedFile>,
    /// Decoded from the legacy format; `flush` keeps writing that format, see [`write_legacy`].
    pub(crate) legacy: bool,
    pub(crate) dirty: bool,
    /// A change that cannot be redone from source is pending; see [`LIVE_WINDOW_SECS`].
    pub(crate) needs_write: bool,
    pub(crate) changes: Changes,
}

/// A file's key, `mtime_secs`, `size`, `sealed` and records.
#[cfg(test)]
type FileSnapshot<'a> = (&'a str, i64, u64, &'a [u64], &'a [UsageRecord]);

#[cfg(test)]
impl ProviderCache {
    /// Every file's content, sorted by key, for comparing caches in tests.
    pub(crate) fn snapshot(&self) -> Vec<FileSnapshot<'_>> {
        let mut files: Vec<_> = self
            .files
            .iter()
            .map(|(k, cf)| {
                (
                    k.as_str(),
                    cf.mtime_secs,
                    cf.size,
                    cf.sealed.as_slice(),
                    cf.records.as_slice(),
                )
            })
            .collect();
        files.sort_unstable_by_key(|f| f.0);
        files
    }
}

/// What this process did to a provider since its last flush, so the flush can
/// merge with whatever another process wrote in the meantime.
#[derive(Default)]
pub(crate) struct Changes {
    /// Keys inserted, or the targets of a path remap.
    touched: HashSet<String>,
    /// Keys pruned, or the sources of a path remap.
    removed: HashSet<String>,
    rekeys: Vec<Rekey>,
}

struct Rekey {
    old: String,
    new: String,
    dirs: Option<(PathBuf, PathBuf)>,
}

impl Changes {
    /// Every key of `pc` touched, so a merge keeps its entries unless the other side's are newer.
    pub(crate) fn touching_all(pc: &ProviderCache) -> Self {
        Self {
            touched: pc.files.keys().cloned().collect(),
            ..Self::default()
        }
    }

    fn touch(&mut self, key: &str) {
        self.removed.remove(key);
        self.touched.insert(key.to_string());
    }

    fn remove(&mut self, key: &str) {
        self.touched.remove(key);
        self.removed.insert(key.to_string());
    }
}

#[derive(Clone)]
pub(crate) struct CachedFile {
    pub(crate) mtime_secs: i64,
    pub(crate) size: u64,
    pub(crate) records: Vec<UsageRecord>,
    /// Stable fingerprints of the messages rolled up into this file's records.
    pub(crate) sealed: Vec<u64>,
}

impl BitcodeStorage {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            max_bytes: configured_max_bytes(),
            rollup_days: configured_rollup_days(),
            force: false,
        }
    }

    /// Whether this provider already has an entry for `file_path`, regardless
    /// of whether it is still fresh. Used by the sqlite import to leave
    /// existing entries alone.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub fn contains(&mut self, provider: &str, file_path: &Path) -> bool {
        let key = file_path.to_string_lossy().to_string();
        self.provider_cache(provider).files.contains_key(&key)
    }

    /// Every fingerprint sealed in this provider's files.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn sealed_fingerprints(&mut self, provider: &str) -> HashSet<u64> {
        self.provider_cache(provider)
            .files
            .values()
            .flat_map(|cf| cf.sealed.iter().copied())
            .collect()
    }

    /// Whether this provider's store is still in the legacy format, which cannot hold seals.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn is_legacy(&mut self, provider: &str) -> bool {
        self.provider_cache(provider).legacy
    }

    /// [`Storage::insert`] of a file that already sealed the fingerprints in `sealed`.
    pub(crate) fn insert_sealed(
        &mut self,
        provider: &str,
        file_path: &Path,
        mtime: i64,
        size: u64,
        records: Vec<UsageRecord>,
        sealed: Vec<u64>,
    ) {
        let pc = self.provider_cache(provider);
        let key = file_path.to_string_lossy().to_string();
        pc.changes.touch(&key);
        pc.files.insert(
            key,
            CachedFile {
                mtime_secs: mtime,
                size,
                records: crate::dedup::dedup(records),
                sealed,
            },
        );
        pc.dirty = true;
        if Utc::now().timestamp() - mtime > LIVE_WINDOW_SECS {
            pc.needs_write = true;
        }
    }

    /// Load (or create) the cache for a specific provider, lazily.
    fn provider_cache(&mut self, provider: &str) -> &mut ProviderCache {
        let max_bytes = self.max_bytes;
        self.providers
            .entry(provider.to_string())
            .or_insert_with(|| load_provider(provider, max_bytes))
    }
}

pub(crate) fn configured_max_bytes() -> u64 {
    crate::config::load_config()
        .cache_max_bytes
        .unwrap_or(DEFAULT_MAX_CACHE_BYTES)
}

pub(crate) fn configured_rollup_days() -> u32 {
    crate::config::load_config().rollup_after_days()
}

/// Read one provider's file, quarantining anything unreadable.
///
/// Never returns an empty cache while leaving a non-empty file in place: an
/// undecodable payload is renamed aside first, so the subsequent flush can't
/// overwrite records that were merely unreadable by this build.
fn load_provider(provider: &str, max_bytes: u64) -> ProviderCache {
    let Some(path) = paths::bitcode_cache_file(provider) else {
        return ProviderCache::default();
    };
    match read_provider(&path, max_bytes, true) {
        Ok(pc) => pc.unwrap_or_default(),
        Err(e) => {
            // Readable-but-not-read (permissions, I/O error). Leave it alone
            // and run without a cache rather than replace what we can't see.
            eprintln!("tku: cannot read {provider} cache ({e}); running without it this time");
            ProviderCache::default()
        }
    }
}

/// [`load_provider`] for a known path. Errors when the file exists but could
/// not be read, or is unusable and `quarantine_bad` is false; a missing, empty
/// or quarantined file is `None`.
pub(crate) fn read_provider(
    path: &Path,
    max_bytes: u64,
    quarantine_bad: bool,
) -> std::io::Result<Option<ProviderCache>> {
    // Pre-flight size check. `fs::read` allocates a Vec sized to the file, so
    // a runaway file would OOM the process before we could compact it. The
    // bound is absolute, not a multiple of the ceiling: lowering `cache_max_bytes`
    // must compact an existing store down, not refuse to open it.
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > max_bytes.saturating_mul(2).max(ABSOLUTE_READ_LIMIT) {
            let why = format!("{} bytes, over twice the ceiling", meta.len());
            if !quarantine_bad {
                return Err(std::io::Error::other(why));
            }
            quarantine(path, &why);
            return Ok(None);
        }
    }
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if data.is_empty() {
        return Ok(None);
    }
    match decode(&data) {
        Some(pc) => Ok(Some(pc)),
        None if !quarantine_bad => Err(std::io::Error::other("unrecognized format")),
        None => {
            quarantine(path, "unrecognized format");
            Ok(None)
        }
    }
}

/// Move an undecodable cache file aside so a later build can recover it.
fn quarantine(path: &Path, why: &str) {
    let stamp = Utc::now().format("%Y%m%d%H%M%S");
    let aside = path.with_extension(format!("bin.quarantine-{stamp}"));
    match fs::rename(path, &aside) {
        Ok(()) => eprintln!(
            "tku: {} is unusable ({why}); kept at {} and starting a fresh cache. \
             Its records are still in that file — don't delete it if you need the history.",
            path.display(),
            aside.display()
        ),
        Err(e) => eprintln!(
            "tku: {} is unusable ({why}) and could not be moved aside ({e}); \
             refusing to overwrite it",
            path.display()
        ),
    }
}

/// Decode a native or legacy payload. `None` for anything else, which the
/// caller quarantines rather than overwrites: the store may be the only copy.
fn decode(data: &[u8]) -> Option<ProviderCache> {
    if format::is_native(data) {
        format::decode(data)
    } else {
        legacy::decode(data)
    }
}

/// Whether `flush` writes the legacy format: the disk copy read under the lock
/// decides, and a missing or empty file keeps the format memory was loaded in.
///
/// A native disk copy means another process migrated and upgraded every legacy entry.
fn write_legacy(mem_legacy: bool, disk: Option<&ProviderCache>) -> bool {
    disk.map_or(mem_legacy, |d| d.legacy)
}

/// Fold `disk` into `mem`, where `changes` is what this process did to `mem`.
///
/// Removed keys are never taken from disk. A touched key keeps the in-memory
/// entry unless the on-disk `mtime_secs` is newer. Every other key on disk
/// replaces the in-memory entry, so another process's compaction or dedup
/// survives; keys only in memory are kept. Recorded rekeys are replayed on
/// entries taken from disk. With empty `changes`, `disk` wins every key it has.
pub(crate) fn merge_from_disk(mem: &mut ProviderCache, disk: ProviderCache, changes: &Changes) {
    let disk_keys: HashSet<String> = disk.files.keys().cloned().collect();
    for (mut key, mut cf) in disk.files {
        if changes.removed.contains(&key) {
            continue;
        }
        for rekey in &changes.rekeys {
            rekey_records(&mut cf.records, &rekey.old, &rekey.new);
            if let Some((old_dir, new_dir)) = &rekey.dirs {
                if let Some(to) = super::remap_path(&key, old_dir, new_dir) {
                    // Same rule as `rekey_account`: an existing target keeps the source key.
                    if !mem.files.contains_key(&to) && !disk_keys.contains(&to) {
                        key = to;
                    }
                }
            }
        }
        if changes.removed.contains(&key) {
            continue;
        }
        if changes.touched.contains(&key)
            && mem
                .files
                .get(&key)
                .is_some_and(|m| m.mtime_secs >= cf.mtime_secs)
        {
            continue;
        }
        mem.files.insert(key, cf);
    }
}

/// Retag the records of account `old` as `new`, recomputing aggregate message
/// ids. Returns whether any record changed.
fn rekey_records(records: &mut [UsageRecord], old: &str, new: &str) -> bool {
    let mut changed = false;
    for r in records {
        if r.account_uuid.as_deref() != Some(old) {
            continue;
        }
        r.account_uuid = Some(new.to_string());
        if r.aggregate {
            r.message_id = types::aggregate_id(r);
        }
        changed = true;
    }
    changed
}

/// Serialize, rolling up progressively younger files until the payload fits.
///
/// A payload still over the ceiling after every step is returned and written
/// as is. Dropping records to hit a size target would delete history that no
/// longer exists anywhere else, so the ceiling yields rather than the archive.
/// The pre-rollup entries of every file it rolls up are appended to `undo`.
pub(crate) fn serialize_within(
    pc: &mut ProviderCache,
    max_bytes: u64,
    provider: &str,
    undo: &mut rollup::Undo,
) -> Option<Vec<u8>> {
    let mut data = serialize(pc, provider)?;
    if data.len() as u64 <= max_bytes {
        return Some(data);
    }
    // The legacy format cannot carry `sealed`, so a legacy store is never rolled up.
    let steps = if pc.legacy {
        &[][..]
    } else {
        COMPACT_CUTOFF_DAYS
    };
    for &days in steps {
        let changed = rollup::rollup(&mut pc.files, Utc::now() - Duration::days(days));
        if changed.is_empty() {
            continue;
        }
        undo.extend(changed);
        data = serialize(pc, provider)?;
        eprintln!(
            "tku: {provider} cache over the {} ceiling; rolled up files older than {days} days to {}",
            human(max_bytes),
            human(data.len() as u64)
        );
        if data.len() as u64 <= max_bytes {
            return Some(data);
        }
    }
    let config = paths::config_file()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "config.toml".into());
    eprintln!(
        "tku: {provider} cache is {} and rollups could not get it under the {} ceiling. \
         Writing it as is; no records were dropped. Raise cache_max_bytes in {config}.",
        human(data.len() as u64),
        human(max_bytes)
    );
    Some(data)
}

/// Put back the entries a failed flush rolled up: an unwritten seal must not
/// survive in memory while the disk copy lets another file seal the same message.
fn restore(pc: &mut ProviderCache, undo: rollup::Undo) {
    for (key, cf) in undo.into_iter().rev() {
        pc.files.insert(key, cf);
    }
}

/// Encode `pc` natively, or in the legacy format when `pc.legacy` is set.
fn serialize(pc: &ProviderCache, provider: &str) -> Option<Vec<u8>> {
    if !pc.legacy {
        return Some(format::encode(pc));
    }
    match legacy::encode(pc) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("tku: failed to serialize {provider} cache: {e}");
            None
        }
    }
}

/// Append `pc`'s records to `out` in sorted key order, dropping raw copies of
/// messages sealed in a different file.
fn drain_sorted(pc: ProviderCache, out: &mut Vec<UsageRecord>) {
    let mut files: Vec<(String, CachedFile)> = pc.files.into_iter().collect();
    files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let sealed_in: HashMap<u64, usize> = files
        .iter()
        .enumerate()
        .flat_map(|(i, (_, cf))| cf.sealed.iter().map(move |&fp| (fp, i)))
        .collect();
    for (i, (_, cf)) in files.into_iter().enumerate() {
        if sealed_in.is_empty() {
            out.extend(cf.records);
            continue;
        }
        out.extend(cf.records.into_iter().filter(|r| {
            r.aggregate
                || (r.message_id.is_empty() && r.request_id.is_empty())
                || sealed_in
                    .get(&crate::dedup::stable_fingerprint(
                        r.provider,
                        &r.message_id,
                        &r.request_id,
                    ))
                    .is_none_or(|&owner| owner == i)
        }));
    }
}

pub(crate) fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MIB)
}

/// Take the store lock beside `path`, waiting up to [`STORE_LOCK_WAIT`].
/// `None` (after a warning) when it could not be taken.
pub(crate) fn lock_store(path: &Path, provider: &str) -> Option<StoreLock> {
    match try_lock_store(path, STORE_LOCK_WAIT) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            eprintln!(
                "tku: another tku process held the store lock for {}s; {provider} cache not saved this run",
                STORE_LOCK_WAIT.as_secs()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "tku: cannot lock the store beside {} ({e}); {provider} cache not saved this run",
                path.display()
            );
            None
        }
    }
}

/// The store lock beside `path`, waiting up to `wait`. `Ok(None)` when another
/// process still holds it.
pub(crate) fn try_lock_store(
    path: &Path,
    wait: std::time::Duration,
) -> std::io::Result<Option<StoreLock>> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("store path has no parent"))?;
    fs::create_dir_all(dir)?;
    StoreLock::acquire_wait(&dir.join(".store.lock"), wait)
}

/// `flock` on the store's lock file, released by the kernel when the file is
/// closed, including on crash, so a dead holder never leaves it stale.
pub(crate) struct StoreLock {
    _file: fs::File,
}

impl StoreLock {
    /// `Ok(None)` when another process still holds it after `wait`.
    fn acquire_wait(path: &Path, wait: std::time::Duration) -> std::io::Result<Option<Self>> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts.open(path)?;
        let started = std::time::Instant::now();
        loop {
            if try_flock(&file)? {
                return Ok(Some(Self { _file: file }));
            }
            if started.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(unix)]
fn try_flock(file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `flock` only operates on the descriptor, which `file` keeps open.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(not(unix))]
fn try_flock(_file: &fs::File) -> std::io::Result<bool> {
    Ok(true)
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
        self.insert_sealed(provider, file_path, mtime, size, records, Vec::new());
    }

    fn prune(&mut self, provider: &str, existing: &[PathBuf]) {
        let pc = self.provider_cache(provider);
        let known: HashSet<String> = existing
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let before = pc.files.len();
        let dropped: usize = pc
            .files
            .iter()
            .filter(|(k, _)| !known.contains(*k))
            .map(|(_, cf)| cf.records.len())
            .sum();
        let changes = &mut pc.changes;
        pc.files.retain(|k, _| {
            let keep = known.contains(k);
            if !keep {
                changes.remove(k);
            }
            keep
        });
        if pc.files.len() != before {
            eprintln!(
                "tku: --prune deleted {} {provider} records from {} source files that no longer exist. \
                 This history is not recoverable from disk.",
                dropped,
                before - pc.files.len()
            );
            pc.dirty = true;
            pc.needs_write = true;
        }
    }

    fn flush(&mut self) -> bool {
        if !self.providers.values().any(|pc| pc.dirty) {
            return true;
        }
        let max_bytes = self.max_bytes;
        let rollup_days = self.rollup_days;
        let rollup_cutoff = Utc::now() - Duration::days(i64::from(rollup_days));
        let force = self.force;
        let mut ok = true;
        for (name, pc) in &mut self.providers {
            if !pc.dirty || (!pc.needs_write && !force) {
                continue;
            }
            let Some(path) = paths::bitcode_cache_file(name) else {
                ok = false;
                continue;
            };
            let Some(_lock) = lock_store(&path, name) else {
                ok = false;
                continue;
            };
            let disk = match read_provider(&path, max_bytes, false) {
                Ok(disk) => disk,
                Err(e) => {
                    eprintln!("tku: cannot read {name} cache ({e}); not overwriting it");
                    ok = false;
                    continue;
                }
            };
            let changes = std::mem::take(&mut pc.changes);
            let was_legacy = pc.legacy;
            pc.legacy = write_legacy(pc.legacy, disk.as_ref());
            merge_from_disk(pc, disk.unwrap_or_default(), &changes);
            let mut undo = if pc.legacy {
                Vec::new()
            } else {
                rollup::rollup(&mut pc.files, rollup_cutoff)
            };
            let Some(data) = serialize_within(pc, max_bytes, name, &mut undo) else {
                restore(pc, undo);
                pc.changes = changes;
                pc.legacy = was_legacy;
                ok = false;
                continue;
            };
            if let Err(e) = atomic_write(&path, &data, None) {
                eprintln!("tku: failed to write {name} cache: {e}");
                restore(pc, undo);
                pc.changes = changes;
                pc.legacy = was_legacy;
                ok = false;
                continue;
            }
            pc.dirty = false;
            pc.needs_write = false;
            // Only the approaching-the-ceiling case: once it is actually
            // exceeded, `serialize_within` has already said what it did.
            let used = data.len() as u64;
            if used <= max_bytes && used as f64 >= max_bytes as f64 * WARN_FRACTION {
                eprintln!(
                    "tku: {name} cache at {:.0}% of the {} ceiling ({}). \
                     Files older than {} days are kept as daily totals; past the ceiling, files \
                     down to 30 days old are rolled up too (totals are kept, intra-day detail is not). \
                     Raise cache_max_bytes to keep full detail.",
                    used as f64 / max_bytes as f64 * 100.0,
                    human(max_bytes),
                    human(used),
                    rollup_days
                );
            }
        }
        ok
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        // A provider whose transcripts are all gone is never loaded by `is_cached`.
        for p in Provider::iter() {
            self.provider_cache(p.as_str());
        }
        let mut providers: Vec<(String, ProviderCache)> = self.providers.drain().collect();
        providers.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut all = Vec::new();
        for (_, pc) in providers {
            drain_sorted(pc, &mut all);
        }
        all
    }

    fn rekey_account(
        &mut self,
        old: &str,
        new: &str,
        old_dir: Option<&Path>,
        new_dir: Option<&Path>,
    ) {
        let pc = self.provider_cache(Provider::Claude.as_str());
        for cf in pc.files.values_mut() {
            rekey_records(&mut cf.records, old, new);
        }

        if let (Some(old_dir), Some(new_dir)) = (old_dir, new_dir) {
            let moves: Vec<(String, String)> = pc
                .files
                .keys()
                .filter_map(|k| Some((k.clone(), super::remap_path(k, old_dir, new_dir)?)))
                .collect();
            for (from, to) in moves {
                // Never overwrite an existing entry: its records may be the only copy.
                if pc.files.contains_key(&to) {
                    continue;
                }
                if let Some(cf) = pc.files.remove(&from) {
                    pc.changes.remove(&from);
                    pc.changes.touch(&to);
                    pc.files.insert(to, cf);
                }
            }
        }
        pc.changes.rekeys.push(Rekey {
            old: old.to_string(),
            new: new.to_string(),
            dirs: old_dir
                .zip(new_dir)
                .map(|(o, n)| (o.to_path_buf(), n.to_path_buf())),
        });
        // Dirty even when nothing changed here, so the flush replays the rekey on disk entries.
        pc.dirty = true;
        pc.needs_write = true;
    }

    fn force_write(&mut self) {
        self.force = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: &str, model: &str, out: u64) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s1".into(),
            timestamp: ts.parse().unwrap(),
            project: "p".into(),
            model: model.into(),
            message_id: format!("msg-{ts}-{out}"),
            request_id: format!("req-{ts}-{out}"),
            input_tokens: 1,
            output_tokens: out,
            cache_creation_input_tokens: 2,
            cache_creation_1h_input_tokens: 1,
            cache_read_input_tokens: 3,
            fast_mode: false,
            account_uuid: None,
            aggregate: false,
        }
    }

    fn file(records: Vec<UsageRecord>) -> CachedFile {
        CachedFile {
            mtime_secs: 0,
            size: 0,
            records,
            sealed: Vec::new(),
        }
    }

    /// `cf` rolled up as the only file of a store.
    fn rolled(cf: CachedFile) -> CachedFile {
        let mut files = HashMap::from([(String::new(), cf)]);
        assert!(!rollup::rollup(&mut files, Utc::now()).is_empty());
        files.remove("").unwrap()
    }

    fn total_output(records: &[UsageRecord]) -> u64 {
        records.iter().map(|r| r.output_tokens).sum()
    }

    #[test]
    fn over_ceiling_orphans_are_written_whole() {
        let mut pc = ProviderCache::default();
        for f in 0..5 {
            let records = (1..=20)
                .map(|d| rec(&format!("2020-01-{d:02}T01:00:00Z"), "opus", f * 100 + d))
                .collect();
            pc.files
                .insert(format!("/nonexistent/tku-test/{f}.jsonl"), file(records));
        }
        let keys: HashSet<String> = pc.files.keys().cloned().collect();
        let sum = |pc: &ProviderCache| -> (u64, u64) {
            pc.files
                .values()
                .flat_map(|cf| &cf.records)
                .fold((0, 0), |(i, o), r| {
                    (i + r.input_tokens, o + r.output_tokens)
                })
        };
        let before = sum(&pc);

        let data = serialize_within(&mut pc, 64, "claude", &mut Vec::new()).unwrap();
        assert!(
            data.len() > 64,
            "compacted payload must still exceed the ceiling"
        );

        let decoded = decode(&data).unwrap();
        assert_eq!(decoded.files.keys().cloned().collect::<HashSet<_>>(), keys);
        assert_eq!(sum(&decoded), before);
    }

    #[test]
    fn unrecognized_payload_decodes_to_nothing_rather_than_garbage() {
        assert!(decode(b"not a cache").is_none());
        let wrong = bitcode::serialize(&(0u32, Vec::<u64>::new())).unwrap();
        assert!(decode(&wrong).is_none());
        let mut future = format::encode(&ProviderCache::default());
        future[8] = 3;
        assert!(decode(&future).is_none());
    }

    /// An undecodable file is renamed, not replaced: the bytes stay recoverable.
    #[test]
    fn quarantine_moves_the_file_aside() {
        let dir = std::env::temp_dir().join(format!("tku-quarantine-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude.bin");
        fs::write(&path, b"garbage").unwrap();

        quarantine(&path, "test");

        assert!(!path.exists());
        let aside: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("quarantine"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(fs::read(aside[0].path()).unwrap(), b"garbage");

        fs::remove_dir_all(&dir).unwrap();
    }

    fn tagged(ts: &str, out: u64, account: Option<&str>) -> UsageRecord {
        UsageRecord {
            account_uuid: account.map(str::to_string),
            ..rec(ts, "opus", out)
        }
    }

    fn records_of(storage: &mut BitcodeStorage, key: &str) -> Vec<UsageRecord> {
        storage.provider_cache("claude").files[key].records.clone()
    }

    #[test]
    fn rekey_account_rewrites_tags_and_aggregate_message_ids() {
        super::super::with_scratch_home("bitcode-rekey-agg", || {
            let mut storage = BitcodeStorage::new();
            let key = "/proj/a.jsonl";
            storage.insert(
                "claude",
                Path::new(key),
                0,
                0,
                vec![
                    tagged("2020-01-01T01:00:00Z", 10, Some("old")),
                    tagged("2020-01-01T02:00:00Z", 5, Some("old")),
                    tagged("2020-01-02T01:00:00Z", 7, Some("old")),
                ],
            );
            assert!(
                !rollup::rollup(&mut storage.provider_cache("claude").files, Utc::now()).is_empty()
            );
            storage.provider_cache("claude").dirty = false;

            storage.rekey_account("old", "new", None, None);

            assert!(storage.provider_cache("claude").dirty);
            let rekeyed = records_of(&mut storage, key);
            for r in &rekeyed {
                assert_eq!(r.account_uuid.as_deref(), Some("new"));
                assert!(r.aggregate);
                assert_eq!(r.message_id, types::aggregate_id(r));
            }

            assert_eq!(rekeyed[0].output_tokens, 15);
            assert!(
                rollup::rollup(&mut storage.provider_cache("claude").files, Utc::now()).is_empty()
            );
        });
    }

    #[test]
    fn rekey_account_leaves_other_accounts_and_untagged_records() {
        super::super::with_scratch_home("bitcode-rekey-others", || {
            let mut storage = BitcodeStorage::new();
            storage.insert(
                "claude",
                Path::new("/proj/other.jsonl"),
                0,
                0,
                vec![
                    tagged("2020-01-01T01:00:00Z", 1, Some("other")),
                    tagged("2020-01-01T02:00:00Z", 2, Some("other")),
                ],
            );
            assert!(
                !rollup::rollup(&mut storage.provider_cache("claude").files, Utc::now()).is_empty()
            );
            storage.insert(
                "claude",
                Path::new("/proj/mixed.jsonl"),
                0,
                0,
                vec![
                    tagged("2020-01-01T01:00:00Z", 3, Some("old")),
                    tagged("2020-01-01T02:00:00Z", 4, None),
                ],
            );
            let other_before = records_of(&mut storage, "/proj/other.jsonl");

            storage.rekey_account("old", "new", None, None);

            let other_after = records_of(&mut storage, "/proj/other.jsonl");
            assert_eq!(other_after.len(), 1);
            assert_eq!(other_after[0].account_uuid.as_deref(), Some("other"));
            assert_eq!(other_after[0].message_id, other_before[0].message_id);

            let mixed = records_of(&mut storage, "/proj/mixed.jsonl");
            assert_eq!(mixed[0].account_uuid.as_deref(), Some("new"));
            assert_eq!(mixed[0].message_id, "msg-2020-01-01T01:00:00Z-3");
            assert_eq!(mixed[1].account_uuid, None);
        });
    }

    #[test]
    fn rekey_account_remaps_file_paths_under_old_dir() {
        super::super::with_scratch_home("bitcode-rekey-paths", || {
            let mut storage = BitcodeStorage::new();
            for key in [
                "/old/proj/a.jsonl",
                "/old/proj/c.jsonl",
                "/new/proj/c.jsonl",
                "/oldish/b.jsonl",
            ] {
                storage.insert(
                    "claude",
                    Path::new(key),
                    0,
                    0,
                    vec![tagged("2020-01-01T01:00:00Z", 1, None)],
                );
            }
            storage.provider_cache("claude").dirty = false;

            storage.rekey_account(
                "old",
                "new",
                Some(Path::new("/old")),
                Some(Path::new("/new")),
            );

            let pc = storage.provider_cache("claude");
            assert!(pc.dirty);
            let mut keys: Vec<&str> = pc.files.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "/new/proj/a.jsonl",
                    "/new/proj/c.jsonl",
                    "/old/proj/c.jsonl",
                    "/oldish/b.jsonl"
                ]
            );
        });
    }

    /// An instance whose claude cache has been read from disk.
    fn loaded() -> BitcodeStorage {
        let mut storage = BitcodeStorage::new();
        storage.provider_cache("claude");
        storage
    }

    fn insert_at(storage: &mut BitcodeStorage, key: &str, mtime: i64, records: Vec<UsageRecord>) {
        storage.insert("claude", Path::new(key), mtime, 0, records);
    }

    fn sorted_keys(storage: &mut BitcodeStorage) -> Vec<String> {
        let mut keys: Vec<String> = storage
            .provider_cache("claude")
            .files
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn store_lock_excludes_a_second_holder_until_dropped() {
        let dir = std::env::temp_dir().join(format!("tku-store-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".store.lock");
        let wait = std::time::Duration::ZERO;

        let first = StoreLock::acquire_wait(&path, wait).unwrap();
        assert!(first.is_some());
        assert!(StoreLock::acquire_wait(&path, wait).unwrap().is_none());
        drop(first);
        assert!(StoreLock::acquire_wait(&path, wait).unwrap().is_some());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flush_keeps_inserts_from_both_instances() {
        super::super::with_scratch_home("bitcode-merge-both", || {
            let mut a = loaded();
            let mut b = loaded();
            insert_at(
                &mut a,
                "/a.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            insert_at(
                &mut b,
                "/b.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 2)],
            );
            assert!(a.flush());
            assert!(b.flush());

            assert_eq!(sorted_keys(&mut loaded()), ["/a.jsonl", "/b.jsonl"]);
        });
    }

    #[test]
    fn flush_does_not_resurrect_a_pruned_key() {
        super::super::with_scratch_home("bitcode-merge-prune", || {
            let mut seed = loaded();
            insert_at(
                &mut seed,
                "/gone.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            insert_at(
                &mut seed,
                "/kept.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 2)],
            );
            assert!(seed.flush());

            let mut a = loaded();
            let mut b = loaded();
            insert_at(
                &mut b,
                "/new.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 3)],
            );
            assert!(b.flush());
            a.prune("claude", &[PathBuf::from("/kept.jsonl")]);
            assert!(a.flush());

            assert_eq!(sorted_keys(&mut loaded()), ["/kept.jsonl", "/new.jsonl"]);
        });
    }

    #[test]
    fn flush_takes_a_newer_on_disk_entry_over_a_touched_one() {
        super::super::with_scratch_home("bitcode-merge-mtime", || {
            let mut a = loaded();
            let mut b = loaded();
            insert_at(
                &mut a,
                "/newer-on-disk.jsonl",
                10,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            insert_at(
                &mut a,
                "/older-on-disk.jsonl",
                10,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            insert_at(
                &mut b,
                "/newer-on-disk.jsonl",
                20,
                vec![rec("2020-01-01T01:00:00Z", "opus", 2)],
            );
            insert_at(
                &mut b,
                "/older-on-disk.jsonl",
                5,
                vec![rec("2020-01-01T01:00:00Z", "opus", 2)],
            );
            assert!(b.flush());
            assert!(a.flush());

            let mut c = loaded();
            let files = &c.provider_cache("claude").files;
            assert_eq!(files["/newer-on-disk.jsonl"].mtime_secs, 20);
            assert_eq!(files["/newer-on-disk.jsonl"].records[0].output_tokens, 2);
            assert_eq!(files["/older-on-disk.jsonl"].mtime_secs, 10);
            assert_eq!(files["/older-on-disk.jsonl"].records[0].output_tokens, 1);
        });
    }

    #[test]
    fn flush_keeps_another_instances_compaction() {
        super::super::with_scratch_home("bitcode-merge-compact", || {
            let mut seed = loaded();
            seed.rollup_days = NEVER;
            insert_at(
                &mut seed,
                "/x.jsonl",
                1,
                vec![
                    rec("2020-01-01T01:00:00Z", "opus", 1),
                    rec("2020-01-01T02:00:00Z", "opus", 2),
                ],
            );
            assert!(seed.flush());

            let mut a = loaded();
            let mut b = loaded();
            a.max_bytes = 64;
            a.rollup_days = NEVER;
            b.rollup_days = NEVER;
            a.provider_cache("claude").dirty = true;
            a.force_write();
            assert!(a.flush());
            insert_at(
                &mut b,
                "/y.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 3)],
            );
            assert!(b.flush());

            let x = records_of(&mut loaded(), "/x.jsonl");
            assert_eq!(x.len(), 1);
            assert_eq!(x[0].output_tokens, 3);
            assert!(x[0].aggregate);
        });
    }

    #[test]
    fn flush_replays_rekeys_on_entries_from_disk() {
        super::super::with_scratch_home("bitcode-merge-rekey", || {
            let mut seed = loaded();
            insert_at(
                &mut seed,
                "/old/proj/a.jsonl",
                1,
                vec![tagged("2020-01-01T01:00:00Z", 1, Some("old"))],
            );
            assert!(seed.flush());

            let mut a = loaded();
            let mut b = loaded();
            insert_at(
                &mut b,
                "/old/proj/b.jsonl",
                1,
                vec![tagged("2020-01-01T01:00:00Z", 2, Some("old"))],
            );
            assert!(b.flush());
            a.rekey_account(
                "old",
                "new",
                Some(Path::new("/old")),
                Some(Path::new("/new")),
            );
            assert!(a.flush());

            let mut c = loaded();
            assert_eq!(
                sorted_keys(&mut c),
                ["/new/proj/a.jsonl", "/new/proj/b.jsonl"]
            );
            for cf in c.provider_cache("claude").files.values() {
                for r in &cf.records {
                    assert_eq!(r.account_uuid.as_deref(), Some("new"));
                }
            }
        });
    }

    const LEGACY_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/legacy-claude.bin"
    );
    const FIXTURE_DIR: &str = "/home/user/.claude/projects/-home-user-git-app";
    const ACCT: &str = "3f2a9c1e-7b4d-4e8a-9c21-5d6e7f8a9b0c";

    #[allow(clippy::too_many_arguments)]
    fn claude(
        session: &str,
        ts: &str,
        msg: &str,
        req: &str,
        input: u64,
        out: u64,
        account: Option<&str>,
        fast: bool,
    ) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: session.into(),
            timestamp: ts.parse().unwrap(),
            project: "app".into(),
            model: "claude-opus-4-1-20250805".into(),
            message_id: msg.into(),
            request_id: req.into(),
            input_tokens: input,
            output_tokens: out,
            cache_creation_input_tokens: 1200,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 18000,
            fast_mode: fast,
            account_uuid: account.map(str::to_string),
            aggregate: false,
        }
    }

    fn fixture_file(
        session: &str,
        mtime_secs: i64,
        records: Vec<UsageRecord>,
    ) -> (String, CachedFile) {
        (
            format!("{FIXTURE_DIR}/{session}.jsonl"),
            CachedFile {
                mtime_secs,
                size: 4096 * records.len() as u64,
                records,
                sealed: Vec::new(),
            },
        )
    }

    /// The legacy store pinned by `tests/fixtures/legacy-claude.bin`.
    fn legacy_fixture_cache() -> ProviderCache {
        let acct = Some(ACCT);
        let mut pc = ProviderCache::default();

        // Streaming lines: one message per content block, partial output on all but the last.
        let (k, cf) = fixture_file(
            "a1b2c3d4-0001-4000-8000-000000000001",
            1_790_000_000,
            vec![
                claude(
                    "a1b2c3d4-0001-4000-8000-000000000001",
                    "2026-10-01T09:00:00.123Z",
                    "msg_01StreamAAAAAAAAAAAAAAA",
                    "req_011StreamAAAAAAAAAAAAA",
                    3,
                    6,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0001-4000-8000-000000000001",
                    "2026-10-01T09:00:01.456Z",
                    "msg_01StreamAAAAAAAAAAAAAAA",
                    "req_011StreamAAAAAAAAAAAAA",
                    3,
                    6,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0001-4000-8000-000000000001",
                    "2026-10-01T09:00:02.789Z",
                    "msg_01StreamAAAAAAAAAAAAAAA",
                    "req_011StreamAAAAAAAAAAAAA",
                    3,
                    410,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0001-4000-8000-000000000001",
                    "2026-10-01T09:05:00Z",
                    "msg_01SharedBBBBBBBBBBBBBBB",
                    "req_011SharedBBBBBBBBBBBBB",
                    5,
                    120,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0001-4000-8000-000000000001",
                    "2026-10-01T09:10:00Z",
                    "msg_01FastCCCCCCCCCCCCCCCCC",
                    "req_011FastCCCCCCCCCCCCCCC",
                    7,
                    90,
                    acct,
                    true,
                ),
            ],
        );
        pc.files.insert(k, cf);

        // A resumed session replaying one message of the first.
        let (k, cf) = fixture_file(
            "a1b2c3d4-0002-4000-8000-000000000002",
            1_790_001_000,
            vec![
                claude(
                    "a1b2c3d4-0002-4000-8000-000000000002",
                    "2026-10-01T09:05:00Z",
                    "msg_01SharedBBBBBBBBBBBBBBB",
                    "req_011SharedBBBBBBBBBBBBB",
                    5,
                    120,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0002-4000-8000-000000000002",
                    "2026-10-02T14:00:00Z",
                    "msg_01ResumeDDDDDDDDDDDDDDD",
                    "req_011ResumeDDDDDDDDDDDDD",
                    4,
                    300,
                    None,
                    false,
                ),
            ],
        );
        pc.files.insert(k, cf);

        // An old file compacted to daily aggregates.
        let (k, cf) = fixture_file(
            "a1b2c3d4-0003-4000-8000-000000000003",
            1_736_000_000,
            vec![
                claude(
                    "a1b2c3d4-0003-4000-8000-000000000003",
                    "2025-01-10T08:00:00Z",
                    "msg_01OldEEEEEEEEEEEEEEEEE1",
                    "req_011OldEEEEEEEEEEEEEEE1",
                    10,
                    100,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0003-4000-8000-000000000003",
                    "2025-01-10T09:00:00Z",
                    "msg_01OldEEEEEEEEEEEEEEEEE2",
                    "req_011OldEEEEEEEEEEEEEEE2",
                    20,
                    200,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0003-4000-8000-000000000003",
                    "2025-01-11T08:00:00Z",
                    "msg_01OldEEEEEEEEEEEEEEEEE3",
                    "req_011OldEEEEEEEEEEEEEEE3",
                    30,
                    300,
                    None,
                    false,
                ),
            ],
        );
        let old = cf.records;
        let cf = CachedFile {
            records: vec![summed(&old[..2]), summed(&old[2..])],
            ..cf
        };
        pc.files.insert(k, cf);

        // Ceiling case: the earlier file keeps the shared message, the later one
        // keeps only its first partial streaming line, and is then compacted.
        let (k, cf) = fixture_file(
            "a1b2c3d4-0004-4000-8000-000000000004",
            1_737_000_000,
            vec![
                claude(
                    "a1b2c3d4-0004-4000-8000-000000000004",
                    "2025-01-20T10:00:00Z",
                    "msg_01CeilFFFFFFFFFFFFFFFF1",
                    "req_011CeilFFFFFFFFFFFFFFF1",
                    8,
                    80,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0004-4000-8000-000000000004",
                    "2026-10-03T10:00:00Z",
                    "msg_01CeilFFFFFFFFFFFFFFFF2",
                    "req_011CeilFFFFFFFFFFFFFFF2",
                    9,
                    90,
                    acct,
                    false,
                ),
            ],
        );
        pc.files.insert(k, cf);
        let (k, cf) = fixture_file(
            "a1b2c3d4-0005-4000-8000-000000000005",
            1_737_100_000,
            vec![
                claude(
                    "a1b2c3d4-0005-4000-8000-000000000005",
                    "2025-01-20T10:00:00Z",
                    "msg_01CeilFFFFFFFFFFFFFFFF1",
                    "req_011CeilFFFFFFFFFFFFFFF1",
                    8,
                    80,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0005-4000-8000-000000000005",
                    "2025-01-21T11:00:00Z",
                    "msg_01CeilGGGGGGGGGGGGGGGGG",
                    "req_011CeilGGGGGGGGGGGGGGG",
                    2,
                    6,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0005-4000-8000-000000000005",
                    "2025-01-21T11:00:01Z",
                    "msg_01CeilGGGGGGGGGGGGGGGGG",
                    "req_011CeilGGGGGGGGGGGGGGG",
                    2,
                    6,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0005-4000-8000-000000000005",
                    "2025-01-21T11:00:02Z",
                    "msg_01CeilGGGGGGGGGGGGGGGGG",
                    "req_011CeilGGGGGGGGGGGGGGG",
                    2,
                    410,
                    acct,
                    false,
                ),
                claude(
                    "a1b2c3d4-0005-4000-8000-000000000005",
                    "2025-01-21T12:00:00Z",
                    "msg_01CeilHHHHHHHHHHHHHHHHH",
                    "req_011CeilHHHHHHHHHHHHHHH",
                    6,
                    60,
                    None,
                    false,
                ),
            ],
        );
        let trimmed = cf.records;
        let cf = CachedFile {
            records: vec![summed(&trimmed[1..2]), summed(&trimmed[4..])],
            ..cf
        };
        pc.files.insert(k, cf);
        pc
    }

    /// A legacy-encoder aggregate of `group`, which shares one aggregate id.
    fn summed(group: &[UsageRecord]) -> UsageRecord {
        let mut agg = group[0].clone();
        for r in &group[1..] {
            agg.input_tokens += r.input_tokens;
            agg.output_tokens += r.output_tokens;
            agg.cache_creation_input_tokens += r.cache_creation_input_tokens;
            agg.cache_creation_1h_input_tokens += r.cache_creation_1h_input_tokens;
            agg.cache_read_input_tokens += r.cache_read_input_tokens;
            agg.timestamp = agg.timestamp.min(r.timestamp);
        }
        agg.aggregate = true;
        agg.message_id = types::aggregate_id(&agg);
        agg.request_id = types::AGG_MARKER.into();
        agg
    }

    /// Writes the fixture once; regenerating it is never part of a normal run.
    #[test]
    #[ignore]
    fn generate_legacy_fixture() {
        let data = legacy::encode(&legacy_fixture_cache()).unwrap();
        fs::create_dir_all(Path::new(LEGACY_FIXTURE).parent().unwrap()).unwrap();
        fs::write(LEGACY_FIXTURE, data).unwrap();
    }

    #[test]
    fn legacy_fixture_decodes_with_expected_records() {
        let pc = decode(include_bytes!("../../tests/fixtures/legacy-claude.bin")).unwrap();
        assert!(pc.legacy);
        let expected = legacy_fixture_cache();

        let summary = |pc: &ProviderCache| {
            let mut files: Vec<(String, i64, u64, usize, u64, u64, usize)> = pc
                .files
                .iter()
                .map(|(k, cf)| {
                    (
                        k.clone(),
                        cf.mtime_secs,
                        cf.size,
                        cf.records.len(),
                        cf.records.iter().map(|r| r.input_tokens).sum(),
                        cf.records.iter().map(|r| r.output_tokens).sum(),
                        cf.records.iter().filter(|r| r.aggregate).count(),
                    )
                })
                .collect();
            files.sort();
            files
        };
        let session =
            |n: u8| format!("{FIXTURE_DIR}/a1b2c3d4-000{n}-4000-8000-00000000000{n}.jsonl");
        assert_eq!(
            summary(&pc),
            [
                (session(1), 1_790_000_000, 20480, 5, 21, 632, 0),
                (session(2), 1_790_001_000, 8192, 2, 9, 420, 0),
                (session(3), 1_736_000_000, 12288, 2, 60, 600, 2),
                (session(4), 1_737_000_000, 8192, 2, 17, 170, 0),
                (session(5), 1_737_100_000, 20480, 2, 8, 66, 2),
            ]
        );
        assert_eq!(summary(&pc), summary(&expected));

        let all: Vec<&UsageRecord> = pc.files.values().flat_map(|cf| &cf.records).collect();
        assert_eq!(all.len(), 13);
        assert_eq!(all.iter().filter(|r| r.fast_mode).count(), 1);
        assert_eq!(all.iter().filter(|r| r.account_uuid.is_none()).count(), 3);
    }

    /// Record for record what the encoder that wrote the fixture was given,
    /// with aggregates flagged.
    #[test]
    fn legacy_fixture_decodes_to_the_generator_input() {
        let pc = decode(include_bytes!("../../tests/fixtures/legacy-claude.bin")).unwrap();
        assert_eq!(pc.snapshot(), legacy_fixture_cache().snapshot());
    }

    #[test]
    fn merge_with_no_changes_lets_disk_win() {
        let mut mem = ProviderCache::default();
        mem.files.insert(
            "/both.jsonl".into(),
            file(vec![rec("2020-01-01T01:00:00Z", "opus", 1)]),
        );
        mem.files.insert("/mem.jsonl".into(), file(vec![]));
        let mut disk = ProviderCache::default();
        disk.files.insert(
            "/both.jsonl".into(),
            file(vec![rec("2020-01-01T01:00:00Z", "opus", 2)]),
        );
        disk.files.insert("/disk.jsonl".into(), file(vec![]));

        merge_from_disk(&mut mem, disk, &Changes::default());

        let mut keys: Vec<&str> = mem.files.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["/both.jsonl", "/disk.jsonl", "/mem.jsonl"]);
        assert_eq!(mem.files["/both.jsonl"].records[0].output_tokens, 2);
    }

    fn store_path() -> PathBuf {
        let path = paths::bitcode_cache_file("claude").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn compacted(key: &str) -> ProviderCache {
        let cf = rolled(file(vec![
            tagged("2020-01-01T01:00:00Z", 10, Some("old")),
            tagged("2020-01-01T02:00:00Z", 5, Some("old")),
        ]));
        let mut pc = ProviderCache::default();
        pc.files.insert(key.into(), cf);
        pc
    }

    #[test]
    fn a_legacy_store_is_flushed_in_the_legacy_format() {
        super::super::with_scratch_home("bitcode-legacy-guard", || {
            let path = store_path();
            fs::write(&path, legacy::encode(&compacted("/old.jsonl")).unwrap()).unwrap();

            let mut storage = loaded();
            assert!(storage.provider_cache("claude").legacy);
            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![rec("2020-01-02T01:00:00Z", "opus", 7)],
            );
            assert!(storage.flush());

            let data = fs::read(&path).unwrap();
            assert!(!format::is_native(&data));
            let disk = legacy::decode(&data).unwrap();
            assert_eq!(disk.snapshot().len(), 2);
            assert_eq!(disk.files["/new.jsonl"].records[0].output_tokens, 7);
            let agg = &disk.files["/old.jsonl"].records;
            assert_eq!(agg.len(), 1);
            assert!(agg[0].aggregate);
            assert_eq!(agg[0].output_tokens, 15);
        });
    }

    /// Another process migrated the store after this one loaded it as legacy.
    #[test]
    fn a_legacy_instance_writes_native_over_a_native_disk_copy() {
        super::super::with_scratch_home("bitcode-legacy-migrated", || {
            let path = store_path();
            fs::write(&path, legacy::encode(&compacted("/old.jsonl")).unwrap()).unwrap();
            let mut storage = loaded();
            fs::write(&path, format::encode(&compacted("/old.jsonl"))).unwrap();

            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![rec("2020-01-02T01:00:00Z", "opus", 7)],
            );
            assert!(storage.flush());
            assert!(!storage.provider_cache("claude").legacy);

            let data = fs::read(&path).unwrap();
            assert!(format::is_native(&data));
            assert_eq!(sorted_keys(&mut loaded()), ["/new.jsonl", "/old.jsonl"]);
        });
    }

    #[test]
    fn a_legacy_instance_writes_native_over_an_empty_native_store() {
        super::super::with_scratch_home("bitcode-legacy-empty-native", || {
            let path = store_path();
            fs::write(&path, legacy::encode(&compacted("/old.jsonl")).unwrap()).unwrap();
            let mut storage = loaded();
            fs::write(&path, format::encode(&ProviderCache::default())).unwrap();

            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![rec("2020-01-02T01:00:00Z", "opus", 7)],
            );
            assert!(storage.flush());
            assert!(format::is_native(&fs::read(&path).unwrap()));
        });
    }

    /// An older binary rewrote the store in the legacy format after this one loaded it.
    #[test]
    fn a_native_instance_writes_legacy_over_a_legacy_disk_copy() {
        super::super::with_scratch_home("bitcode-native-then-legacy", || {
            let path = store_path();
            fs::write(&path, format::encode(&compacted("/old.jsonl"))).unwrap();
            let mut storage = loaded();
            assert!(!storage.provider_cache("claude").legacy);
            fs::write(&path, legacy::encode(&compacted("/old.jsonl")).unwrap()).unwrap();

            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![rec("2020-01-02T01:00:00Z", "opus", 7)],
            );
            assert!(storage.flush());
            let data = fs::read(&path).unwrap();
            assert!(!format::is_native(&data));
            assert_eq!(legacy::decode(&data).unwrap().files.len(), 2);
        });
    }

    #[test]
    fn a_legacy_instance_writes_legacy_when_the_file_is_gone() {
        super::super::with_scratch_home("bitcode-legacy-removed", || {
            let path = store_path();
            fs::write(&path, legacy::encode(&compacted("/old.jsonl")).unwrap()).unwrap();
            let mut storage = loaded();
            fs::remove_file(&path).unwrap();

            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![rec("2020-01-02T01:00:00Z", "opus", 7)],
            );
            assert!(storage.flush());
            let data = fs::read(&path).unwrap();
            assert!(!format::is_native(&data));
            assert_eq!(legacy::decode(&data).unwrap().files.len(), 2);
        });
    }

    #[test]
    fn a_native_store_survives_flush_and_merge_between_instances() {
        super::super::with_scratch_home("bitcode-native-merge", || {
            let mut seed = loaded();
            let mut pc = compacted("/agg.jsonl");
            pc.files.get_mut("/agg.jsonl").unwrap().sealed = vec![1, 2];
            seed.providers.insert("claude".into(), pc);
            seed.provider_cache("claude").dirty = true;
            seed.force_write();
            assert!(seed.flush());
            assert!(format::is_native(&fs::read(store_path()).unwrap()));

            let mut a = loaded();
            let mut b = loaded();
            assert!(!a.provider_cache("claude").legacy);
            insert_at(
                &mut a,
                "/a.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            insert_at(
                &mut b,
                "/b.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 2)],
            );
            assert!(a.flush());
            assert!(b.flush());

            let data = fs::read(store_path()).unwrap();
            assert!(format::is_native(&data));
            let mut c = loaded();
            assert_eq!(sorted_keys(&mut c), ["/a.jsonl", "/agg.jsonl", "/b.jsonl"]);
            let agg = &c.provider_cache("claude").files["/agg.jsonl"];
            assert_eq!(agg.sealed, [1, 2]);
            assert!(agg.records[0].aggregate);
            assert_eq!(
                agg.records[0].message_id,
                types::aggregate_id(&agg.records[0])
            );
        });
    }

    #[test]
    fn a_new_store_is_written_native() {
        super::super::with_scratch_home("bitcode-new-native", || {
            let mut storage = loaded();
            insert_at(
                &mut storage,
                "/a.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            assert!(storage.flush());
            assert!(format::is_native(&fs::read(store_path()).unwrap()));
        });
    }

    #[test]
    fn a_garbage_store_is_quarantined_and_replaced_natively() {
        super::super::with_scratch_home("bitcode-garbage", || {
            let path = store_path();
            fs::write(&path, b"garbage").unwrap();

            let mut storage = loaded();
            assert!(storage.provider_cache("claude").files.is_empty());
            let aside: Vec<_> = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains("quarantine"))
                .collect();
            assert_eq!(aside.len(), 1);
            assert_eq!(fs::read(aside[0].path()).unwrap(), b"garbage");

            insert_at(
                &mut storage,
                "/a.jsonl",
                1,
                vec![rec("2020-01-01T01:00:00Z", "opus", 1)],
            );
            assert!(storage.flush());
            assert!(format::is_native(&fs::read(&path).unwrap()));
        });
    }

    #[test]
    fn drain_is_sorted_and_drops_messages_sealed_elsewhere() {
        super::super::with_scratch_home("bitcode-drain-sealed", || {
            let fp = |r: &UsageRecord| {
                crate::dedup::stable_fingerprint(r.provider, &r.message_id, &r.request_id)
            };
            let sealed_msg = rec("2020-01-01T01:00:00Z", "opus", 1);
            let kept_msg = rec("2020-01-01T02:00:00Z", "opus", 2);
            let keyless = UsageRecord {
                message_id: String::new(),
                request_id: String::new(),
                ..rec("2020-01-01T03:00:00Z", "opus", 3)
            };

            let mut pc = compacted("/b.jsonl");
            let owner = pc.files.get_mut("/b.jsonl").unwrap();
            owner.sealed = vec![fp(&sealed_msg), fp(&keyless)];
            // A raw copy in the sealing file itself is not filtered.
            owner.records.push(sealed_msg.clone());
            pc.files.insert(
                "/a.jsonl".into(),
                file(vec![sealed_msg.clone(), kept_msg.clone(), keyless.clone()]),
            );
            pc.files
                .insert("/c.jsonl".into(), file(vec![sealed_msg.clone()]));

            let mut storage = BitcodeStorage::new();
            storage.providers.insert("claude".into(), pc);
            let out: Vec<u64> = storage
                .drain_all()
                .iter()
                .map(|r| r.output_tokens)
                .collect();
            assert_eq!(out, [2, 3, 15, 1]);
        });
    }

    /// A rollup age no test record reaches.
    const NEVER: u32 = 100_000;

    fn msg(id: &str, ts: &str, out: u64) -> UsageRecord {
        UsageRecord {
            message_id: id.into(),
            request_id: format!("req-{id}"),
            ..rec(ts, "opus", out)
        }
    }

    fn sealed_fp(r: &UsageRecord) -> u64 {
        crate::dedup::stable_fingerprint(r.provider, &r.message_id, &r.request_id)
    }

    /// What a report sees: drained, then deduplicated as at read time.
    fn report(mut storage: BitcodeStorage) -> Vec<UsageRecord> {
        crate::dedup::dedup(storage.drain_all())
    }

    #[test]
    fn flush_rolls_up_files_older_than_the_configured_age() {
        super::super::with_scratch_home("bitcode-rollup-eager", || {
            let mut storage = loaded();
            storage.rollup_days = 30;
            let old = (Utc::now() - Duration::days(31)).to_rfc3339();
            let recent = (Utc::now() - Duration::days(29)).to_rfc3339();
            insert_at(&mut storage, "/old.jsonl", 1, vec![msg("m1", &old, 4)]);
            insert_at(
                &mut storage,
                "/recent.jsonl",
                1,
                vec![msg("m2", &recent, 5)],
            );
            assert!(storage.flush());

            let mut c = loaded();
            let files = &c.provider_cache("claude").files;
            assert!(files["/old.jsonl"].records[0].aggregate);
            assert_eq!(files["/old.jsonl"].sealed.len(), 1);
            assert!(!files["/recent.jsonl"].records[0].aggregate);
        });
    }

    #[test]
    fn a_legacy_store_flush_leaves_its_files_unrolled() {
        super::super::with_scratch_home("bitcode-legacy-unrolled", || {
            let path = store_path();
            let mut pc = ProviderCache::default();
            pc.files.insert(
                "/old.jsonl".into(),
                file(vec![
                    msg("m1", "2020-01-01T01:00:00Z", 1),
                    msg("m2", "2020-01-01T02:00:00Z", 2),
                ]),
            );
            fs::write(&path, legacy::encode(&pc).unwrap()).unwrap();

            let mut storage = loaded();
            storage.max_bytes = 64;
            insert_at(
                &mut storage,
                "/new.jsonl",
                1,
                vec![msg("m3", "2020-01-02T01:00:00Z", 3)],
            );
            assert!(storage.flush());

            let disk = legacy::decode(&fs::read(&path).unwrap()).unwrap();
            assert!(disk.legacy);
            assert_eq!(disk.files["/old.jsonl"].records.len(), 2);
            assert_eq!(disk.files["/new.jsonl"].records.len(), 1);
            assert!(disk
                .files
                .values()
                .flat_map(|cf| &cf.records)
                .all(|r| !r.aggregate));
        });
    }

    /// Each instance rolls up what it saw under the lock, so the order of the
    /// flushes decides which file seals a shared message, never the totals.
    #[test]
    fn two_instances_flushing_converge() {
        let run = |a_first: bool| {
            super::super::with_scratch_home("bitcode-rollup-converge", || {
                let mut a = loaded();
                let mut b = loaded();
                insert_at(
                    &mut a,
                    "/a.jsonl",
                    1,
                    vec![
                        msg("shared", "2020-01-01T01:00:00Z", 410),
                        msg("a", "2020-01-01T02:00:00Z", 1),
                    ],
                );
                insert_at(
                    &mut b,
                    "/b.jsonl",
                    1,
                    vec![
                        msg("shared", "2020-01-01T01:00:00Z", 410),
                        msg("b", "2020-01-01T03:00:00Z", 2),
                    ],
                );
                if a_first {
                    assert!(a.flush());
                    assert!(b.flush());
                } else {
                    assert!(b.flush());
                    assert!(a.flush());
                }

                let mut c = loaded();
                let files = &c.provider_cache("claude").files;
                let shared = sealed_fp(&msg("shared", "2020-01-01T01:00:00Z", 0));
                let owners = files
                    .values()
                    .filter(|cf| cf.sealed.contains(&shared))
                    .count();
                assert_eq!(owners, 1);
                c.provider_cache("claude").dirty = true;
                c.force_write();
                assert!(c.flush());
                let bytes = fs::read(store_path()).unwrap();
                c = loaded();
                c.provider_cache("claude").dirty = true;
                c.force_write();
                assert!(c.flush());
                assert_eq!(fs::read(store_path()).unwrap(), bytes);
                total_output(&report(loaded()))
            })
        };
        assert_eq!(run(true), 413);
        assert_eq!(run(false), 413);
    }

    #[test]
    fn files_sharing_a_session_with_overlapping_messages_sum_at_read_time() {
        super::super::with_scratch_home("bitcode-rollup-shared-session", || {
            let mut storage = loaded();
            insert_at(
                &mut storage,
                "/proj-a/s1.jsonl",
                1,
                vec![
                    msg("m1", "2020-01-01T01:00:00Z", 1),
                    msg("m2", "2020-01-01T02:00:00Z", 10),
                ],
            );
            insert_at(
                &mut storage,
                "/proj-b/s1.jsonl",
                1,
                vec![
                    msg("m2", "2020-01-01T02:00:00Z", 10),
                    msg("m3", "2020-01-01T03:00:00Z", 100),
                ],
            );
            assert!(storage.flush());

            let records = report(loaded());
            assert_eq!(records.len(), 2);
            assert!(records.iter().all(|r| r.aggregate));
            assert_eq!(records[0].message_id, records[1].message_id);
            assert_eq!(total_output(&records), 111);
        });
    }

    #[test]
    fn a_resumed_file_replaying_sealed_messages_is_filtered_at_read_time() {
        super::super::with_scratch_home("bitcode-rollup-resume", || {
            let mut storage = loaded();
            insert_at(
                &mut storage,
                "/old.jsonl",
                1,
                vec![msg("m1", "2020-01-01T01:00:00Z", 10)],
            );
            assert!(storage.flush());

            let mut storage = loaded();
            let now = Utc::now().to_rfc3339();
            insert_at(
                &mut storage,
                "/resumed.jsonl",
                2,
                vec![msg("m1", "2020-01-01T01:00:00Z", 10), msg("m2", &now, 5)],
            );
            assert!(storage.flush());

            assert_eq!(total_output(&report(loaded())), 15);
        });
    }

    #[test]
    fn a_compacted_file_whose_source_changes_replaces_its_entry() {
        super::super::with_scratch_home("bitcode-rollup-reparse", || {
            let mut storage = loaded();
            insert_at(
                &mut storage,
                "/s.jsonl",
                1,
                vec![msg("m1", "2020-01-01T01:00:00Z", 10)],
            );
            assert!(storage.flush());

            let mut storage = loaded();
            assert!(storage.provider_cache("claude").files["/s.jsonl"].records[0].aggregate);
            assert!(!storage.is_cached("claude", Path::new("/s.jsonl"), 2, 0));
            let now = Utc::now().to_rfc3339();
            insert_at(
                &mut storage,
                "/s.jsonl",
                2,
                vec![msg("m1", "2020-01-01T01:00:00Z", 10), msg("m2", &now, 5)],
            );
            assert!(storage.flush());

            let mut c = loaded();
            let cf = &c.provider_cache("claude").files["/s.jsonl"];
            assert_eq!(cf.mtime_secs, 2);
            assert!(cf.sealed.is_empty());
            assert!(cf.records.iter().all(|r| !r.aggregate));
            assert_eq!(total_output(&report(c)), 15);
        });
    }

    /// `/a` and `/b`, both old and sharing `shared`, flushed once: `/a` seals
    /// it and `/b` keeps a raw copy.
    fn shared_and_rolled() -> UsageRecord {
        let shared = msg("shared", "2020-01-01T01:00:00Z", 100);
        let mut storage = loaded();
        insert_at(
            &mut storage,
            "/a.jsonl",
            1,
            vec![shared.clone(), msg("a", "2020-01-01T02:00:00Z", 1)],
        );
        insert_at(
            &mut storage,
            "/b.jsonl",
            1,
            vec![shared.clone(), msg("b", "2020-01-01T03:00:00Z", 2)],
        );
        assert!(storage.flush());
        let mut c = loaded();
        let files = &c.provider_cache("claude").files;
        assert!(files["/a.jsonl"].sealed.contains(&sealed_fp(&shared)));
        assert!(files["/b.jsonl"].records.iter().any(|r| !r.aggregate));
        assert_eq!(total_output(&report(c)), 103);
        shared
    }

    fn owners_of(storage: &mut BitcodeStorage, fp: u64) -> Vec<String> {
        let mut owners: Vec<String> = storage
            .provider_cache("claude")
            .files
            .iter()
            .filter(|(_, cf)| cf.sealed.contains(&fp))
            .map(|(k, _)| k.clone())
            .collect();
        owners.sort_unstable();
        owners
    }

    #[test]
    fn pruning_the_sealing_file_brings_back_the_other_copy_once() {
        super::super::with_scratch_home("bitcode-rollup-prune-owner", || {
            let shared = shared_and_rolled();
            let mut storage = loaded();
            storage.prune("claude", &[PathBuf::from("/b.jsonl")]);
            assert!(storage.flush());

            let mut c = loaded();
            assert_eq!(owners_of(&mut c, sealed_fp(&shared)), ["/b.jsonl"]);
            assert!(c.provider_cache("claude").files["/b.jsonl"]
                .records
                .iter()
                .all(|r| r.aggregate));
            assert_eq!(total_output(&report(c)), 102);
        });
    }

    #[test]
    fn re_parsing_the_sealing_file_still_counts_the_message_once() {
        for extra in ["2020-01-01T04:00:00Z".to_string(), Utc::now().to_rfc3339()] {
            super::super::with_scratch_home("bitcode-rollup-reparse-owner", || {
                let shared = shared_and_rolled();
                let mut storage = loaded();
                insert_at(
                    &mut storage,
                    "/a.jsonl",
                    2,
                    vec![shared.clone(), msg("a2", &extra, 10)],
                );
                assert!(storage.flush());

                let mut c = loaded();
                assert!(owners_of(&mut c, sealed_fp(&shared)).len() <= 1);
                assert_eq!(total_output(&report(c)), 112, "extra at {extra}");
            });
        }
    }

    /// A failed write must not leave a seal in memory that the disk copy lacks,
    /// or another instance can seal the same message in a different file.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_undoes_the_rollup() {
        use std::os::unix::fs::PermissionsExt;
        super::super::with_scratch_home("bitcode-rollup-undo", || {
            let shared = msg("shared", "2020-01-01T01:00:00Z", 100);
            let mut seed = loaded();
            seed.rollup_days = NEVER;
            insert_at(&mut seed, "/b.jsonl", 1, vec![shared.clone()]);
            assert!(seed.flush());

            let mut a = loaded();
            insert_at(&mut a, "/a.jsonl", 1, vec![shared.clone()]);
            let dir = store_path().parent().unwrap().to_path_buf();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
            let failed = !a.flush();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(failed);
            let files = &a.provider_cache("claude").files;
            assert!(files.values().all(|cf| cf.sealed.is_empty()));
            assert!(files
                .values()
                .flat_map(|cf| &cf.records)
                .all(|r| !r.aggregate));

            let mut c = loaded();
            insert_at(
                &mut c,
                "/c.jsonl",
                1,
                vec![msg("c", "2020-01-02T01:00:00Z", 1)],
            );
            assert!(c.flush());
            assert_eq!(owners_of(&mut loaded(), sealed_fp(&shared)), ["/b.jsonl"]);

            assert!(a.flush());
            let mut d = loaded();
            assert_eq!(owners_of(&mut d, sealed_fp(&shared)), ["/b.jsonl"]);
            assert_eq!(total_output(&report(d)), 101);
        });
    }

    fn store_state() -> (Vec<u8>, std::time::SystemTime) {
        let path = store_path();
        (
            fs::read(&path).unwrap(),
            fs::metadata(&path).unwrap().modified().unwrap(),
        )
    }

    fn seeded() -> (Vec<u8>, std::time::SystemTime) {
        let mut seed = loaded();
        insert_at(
            &mut seed,
            "/old.jsonl",
            1,
            vec![tagged("2020-01-01T01:00:00Z", 1, Some("old"))],
        );
        insert_at(
            &mut seed,
            "/older.jsonl",
            1,
            vec![rec("2020-01-01T02:00:00Z", "opus", 2)],
        );
        assert!(seed.flush());
        store_state()
    }

    #[test]
    fn live_only_inserts_leave_the_store_untouched_until_forced() {
        super::super::with_scratch_home("bitcode-skip-live", || {
            let before = seeded();
            let mut a = loaded();
            let now = Utc::now().timestamp();
            insert_at(
                &mut a,
                "/live.jsonl",
                now,
                vec![rec("2020-01-03T01:00:00Z", "opus", 3)],
            );
            insert_at(
                &mut a,
                "/edge.jsonl",
                now - LIVE_WINDOW_SECS + 5,
                vec![rec("2020-01-03T02:00:00Z", "opus", 4)],
            );
            assert!(a.flush());
            assert!(store_state() == before);
            assert_eq!(sorted_keys(&mut loaded()), ["/old.jsonl", "/older.jsonl"]);

            a.force_write();
            assert!(a.flush());
            assert_eq!(
                sorted_keys(&mut loaded()),
                ["/edge.jsonl", "/live.jsonl", "/old.jsonl", "/older.jsonl"]
            );
        });
    }

    #[test]
    fn a_non_live_insert_writes() {
        super::super::with_scratch_home("bitcode-skip-live-old", || {
            seeded();
            let mut a = loaded();
            let now = Utc::now().timestamp();
            insert_at(
                &mut a,
                "/live.jsonl",
                now,
                vec![rec("2020-01-03T01:00:00Z", "opus", 3)],
            );
            insert_at(
                &mut a,
                "/stale.jsonl",
                now - LIVE_WINDOW_SECS - 5,
                vec![rec("2020-01-03T02:00:00Z", "opus", 4)],
            );
            assert!(a.flush());
            assert_eq!(
                sorted_keys(&mut loaded()),
                ["/live.jsonl", "/old.jsonl", "/older.jsonl", "/stale.jsonl"]
            );
        });
    }

    #[test]
    fn prune_and_rekey_write_without_a_non_live_insert() {
        super::super::with_scratch_home("bitcode-skip-live-prune", || {
            seeded();
            let mut a = loaded();
            a.prune("claude", &[PathBuf::from("/old.jsonl")]);
            assert!(a.flush());
            assert_eq!(sorted_keys(&mut loaded()), ["/old.jsonl"]);

            let before = store_state();
            let mut b = loaded();
            b.prune("claude", &[PathBuf::from("/old.jsonl")]);
            assert!(b.flush());
            assert!(store_state() == before);

            let mut c = loaded();
            c.rekey_account("old", "new", None, None);
            assert!(c.flush());
            let r = records_of(&mut loaded(), "/old.jsonl");
            assert_eq!(r[0].account_uuid.as_deref(), Some("new"));
        });
    }
}
